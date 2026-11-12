//! The shared audio stream. One RTP stream (one clock, one sequence) is sent
//! to every connected receiver, so they all play in sync with each other.
//!
//! Timing model: packet `n` of a run is due at `start + n * 352 / 44100 s`.
//! Every second we tell each receiver (sync packet) that the frame
//! `next_rtp - lead` plays at that same instant, so a frame is heard `lead`
//! after we send it, plus the receiver's own `latencyMin`.

use super::alac;
use super::cipher::PacketCipher;
use super::clock::{ntp_at, ntp_now};
use super::session::StreamTarget;
use super::{CHANNELS, FRAMES_PER_PACKET, RTP_PAYLOAD_TYPE, SAMPLE_RATE};
use anyhow::Result;
use log::{debug, info, warn};
use parking_lot::Mutex;
use rand::Rng;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Where the PCM comes from. Must always fill `out` (pad with silence).
pub trait AudioSource: Send + Sync {
    /// `out` is interleaved stereo i16, `FRAMES_PER_PACKET * 2` long.
    fn read(&self, out: &mut [i16]);
}

const HISTORY: usize = 1024;

struct Target {
    id: String,
    ip: IpAddr,
    data: UdpSocket,
    control_addr: SocketAddr,
    cipher: PacketCipher,
    joined: bool,
}

struct State {
    seq: u16,
    ssrc: u32,
    targets: Vec<Target>,
    /// Recently sent plain RTP packets, for retransmission.
    history: VecDeque<(u16, Vec<u8>)>,
}

struct Inner {
    state: Mutex<State>,
    control: UdpSocket,
    timing_port: u16,
    control_port: u16,
    running: AtomicBool,
    lead_frames: AtomicU32,
    source: Arc<dyn AudioSource>,
}

#[derive(Clone)]
pub struct Streamer {
    inner: Arc<Inner>,
}

impl Streamer {
    pub fn new(source: Arc<dyn AudioSource>, lead_frames: u32) -> Result<Self> {
        let timing = UdpSocket::bind("0.0.0.0:0")?;
        let control = UdpSocket::bind("0.0.0.0:0")?;
        let timing_port = timing.local_addr()?.port();
        let control_port = control.local_addr()?.port();
        let mut rng = rand::thread_rng();
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                seq: rng.gen(),
                ssrc: rng.gen(),
                targets: Vec::new(),
                history: VecDeque::with_capacity(HISTORY),
            }),
            control: control.try_clone()?,
            timing_port,
            control_port,
            running: AtomicBool::new(false),
            lead_frames: AtomicU32::new(lead_frames),
            source,
        });

        thread::Builder::new()
            .name("airplay-timing".into())
            .spawn(move || timing_loop(timing))?;
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("airplay-control".into())
            .spawn(move || control_loop(control, weak))?;

        info!("streamer: timing port {timing_port}, control port {control_port}");
        Ok(Self { inner })
    }

    pub fn timing_port(&self) -> u16 {
        self.inner.timing_port
    }

    pub fn control_port(&self) -> u16 {
        self.inner.control_port
    }

    /// Takes effect at the next sync packet. Receivers smooth it out, but a
    /// big jump can cause a short glitch.
    pub fn set_lead_frames(&self, frames: u32) {
        self.inner.lead_frames.store(frames, Ordering::Relaxed);
    }

    pub fn add_target(&self, id: &str, t: &StreamTarget) -> Result<()> {
        let data = UdpSocket::bind(match t.ip {
            IpAddr::V4(_) => "0.0.0.0:0",
            IpAddr::V6(_) => "[::]:0",
        })?;
        data.connect(SocketAddr::new(t.ip, t.data_port))?;
        {
            let mut st = self.inner.state.lock();
            st.targets.retain(|x| x.id != id);
            st.targets.push(Target {
                id: id.to_string(),
                ip: t.ip,
                data,
                control_addr: SocketAddr::new(t.ip, t.control_port),
                cipher: PacketCipher::new(&t.key),
                joined: false,
            });
        }
        if !self.inner.running.swap(true, Ordering::SeqCst) {
            let inner = self.inner.clone();
            thread::Builder::new()
                .name("airplay-sender".into())
                .spawn(move || sender_loop(inner))?;
        }
        Ok(())
    }

    pub fn remove_target(&self, id: &str) {
        self.inner.state.lock().targets.retain(|x| x.id != id);
    }
}

fn sync_packet(kind: u8, play_rtp: u32, at: Instant, next_rtp: u32) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = kind;
    p[1] = 0xd4;
    p[2] = 0x00;
    p[3] = 0x07;
    p[4..8].copy_from_slice(&play_rtp.to_be_bytes());
    p[8..16].copy_from_slice(&ntp_at(at).to_be_bytes());
    p[16..20].copy_from_slice(&next_rtp.to_be_bytes());
    p
}

fn sender_loop(inner: Arc<Inner>) {
    // Thread priority matters for smooth pacing on a busy desktop
    #[cfg(windows)]
    raise_thread_priority();

    let frames = FRAMES_PER_PACKET as u64;
    let rate = SAMPLE_RATE as u64;
    let packet_dur = Duration::from_nanos(frames * 1_000_000_000 / rate);
    let mut pcm = vec![0i16; FRAMES_PER_PACKET * CHANNELS];

    let rtp0: u32 = rand::thread_rng().gen();
    let start = Instant::now();
    let mut n: u64 = 0; // packets "elapsed" in this run (sent or skipped)
    let mut frames_since_sync: u64 = rate; // force an early sync
    info!("sender: started");

    loop {
        // Packet due time and the stream position it corresponds to
        let due_at = |k: u64| start + Duration::from_nanos(k * frames * 1_000_000_000 / rate);
        let now = Instant::now();
        let behind = now.saturating_duration_since(due_at(n));
        if behind > Duration::from_millis(250) {
            // We were stalled (sleep, debugger...). Jump ahead instead of
            // flooding the receivers with packets that are already late.
            let skip = behind.as_nanos() as u64 / packet_dur.as_nanos() as u64;
            warn!("sender: stalled {behind:?}, skipping {skip} packets");
            n += skip;
            frames_since_sync = rate;
        }

        while due_at(n) <= Instant::now() {
            let rtp = rtp0.wrapping_add((n * frames) as u32);
            inner.source.read(&mut pcm);
            let payload = alac::encode_uncompressed(&pcm);

            let mut st = inner.state.lock();
            if st.targets.is_empty() {
                inner.running.store(false, Ordering::SeqCst);
                info!("sender: no receivers left, stopping");
                return;
            }
            let lead = inner.lead_frames.load(Ordering::Relaxed);
            let at = due_at(n);

            let mut pkt = Vec::with_capacity(12 + payload.len());
            pkt.push(0x80);
            pkt.push(RTP_PAYLOAD_TYPE);
            pkt.extend_from_slice(&st.seq.to_be_bytes());
            pkt.extend_from_slice(&rtp.to_be_bytes());
            pkt.extend_from_slice(&st.ssrc.to_be_bytes());
            pkt.extend_from_slice(&payload);

            let periodic_sync = frames_since_sync >= rate;
            for t in st.targets.iter_mut() {
                if !t.joined {
                    // First contact: sync with the extension bit, then a
                    // packet with the marker bit
                    let s = sync_packet(0x90, rtp.wrapping_sub(lead), at, rtp);
                    let _ = inner.control.send_to(&s, t.control_addr);
                    let mut first = pkt.clone();
                    first[1] |= 0x80;
                    let _ = t.data.send(&t.cipher.seal(&first));
                    t.joined = true;
                    debug!("sender: {} joined at rtp {rtp}", t.id);
                    continue;
                }
                if periodic_sync {
                    let s = sync_packet(0x80, rtp.wrapping_sub(lead), at, rtp);
                    let _ = inner.control.send_to(&s, t.control_addr);
                }
                if let Err(e) = t.data.send(&t.cipher.seal(&pkt)) {
                    debug!("sender: send to {} failed: {e}", t.id);
                }
            }
            if periodic_sync {
                frames_since_sync = 0;
            }
            frames_since_sync += frames;

            let seq = st.seq;
            if st.history.len() == HISTORY {
                st.history.pop_front();
            }
            st.history.push_back((seq, pkt));
            st.seq = seq.wrapping_add(1);
            drop(st);
            n += 1;
        }

        let next = due_at(n);
        let now = Instant::now();
        if next > now {
            thread::sleep((next - now).min(Duration::from_millis(4)));
        }
    }
}

/// Answers the receivers' NTP-style timing requests with our clock.
fn timing_loop(sock: UdpSocket) {
    let mut req = [0u8; 128];
    loop {
        let (len, peer) = match sock.recv_from(&mut req) {
            Ok(x) => x,
            Err(e) => {
                // Windows reports ICMP port unreachable as a recv error
                debug!("timing: recv error {e}");
                continue;
            }
        };
        let recv_ts = ntp_now();
        log::trace!("timing: request from {peer}");
        if len != 32 || req[0] != 0x80 || req[1] != 0xd2 {
            debug!("timing: unexpected packet ({len} bytes) from {peer}");
            continue;
        }
        let mut res = [0u8; 32];
        res[0] = 0x80;
        res[1] = 0xd3;
        res[2] = req[2];
        res[8..16].copy_from_slice(&req[24..32]); // their transmit -> our origin
        res[16..24].copy_from_slice(&recv_ts.to_be_bytes());
        res[24..32].copy_from_slice(&ntp_now().to_be_bytes());
        let _ = sock.send_to(&res, peer);
    }
}

/// Handles retransmit requests: 0x80 0xd5 | seq_start (u16) | count (u16).
fn control_loop(sock: UdpSocket, inner: std::sync::Weak<Inner>) {
    let mut buf = [0u8; 1500];
    loop {
        let (len, peer) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) => {
                debug!("control: recv error {e}");
                continue;
            }
        };
        let Some(inner) = inner.upgrade() else { return };
        if len < 8 || buf[1] != 0xd5 {
            debug!("control: ignoring {len} bytes (type 0x{:02x}) from {peer}", buf[1]);
            continue;
        }
        let seq_start = u16::from_be_bytes([buf[4], buf[5]]);
        let count = u16::from_be_bytes([buf[6], buf[7]]);
        let st = inner.state.lock();
        let Some(target) = st.targets.iter().find(|t| t.ip == peer.ip()) else {
            continue;
        };
        let mut resent = 0;
        for i in 0..count {
            let seq = seq_start.wrapping_add(i);
            if let Some((_, pkt)) = st.history.iter().rev().find(|(s, _)| *s == seq) {
                let _ = target.data.send(&target.cipher.seal(pkt));
                resent += 1;
            }
        }
        debug!("control: {} asked for {count} packets from {seq_start}, resent {resent}", target.id);
    }
}

#[cfg(windows)]
fn raise_thread_priority() {
    // THREAD_PRIORITY_TIME_CRITICAL = 15
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(h: isize, prio: i32) -> i32;
    }
    unsafe {
        SetThreadPriority(GetCurrentThread(), 15);
    }
}
