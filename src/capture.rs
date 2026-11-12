//! Captures what Windows is playing (WASAPI loopback on a render device),
//! converted by Windows to 44.1 kHz / 16-bit / stereo, into a small jitter
//! buffer that the AirPlay sender pulls from at its own pace.

use crate::airplay::streamer::AudioSource;
use crate::airplay::{CHANNELS, SAMPLE_RATE};
use crate::winaudio::{EndpointVolume, PcMute};
use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wasapi::{Direction, SampleType, StreamMode, WaveFormat};

const FRAME_MS: usize = SAMPLE_RATE as usize / 1000;
/// Wait for this much audio before playing after an underrun.
const PREFILL_FRAMES: usize = 25 * FRAME_MS;
/// Above this, we drop a frame per packet to catch up (clock drift).
const HIGH_FRAMES: usize = 70 * FRAME_MS;
/// Above this, something stalled: jump back to the prefill level.
const MAX_FRAMES: usize = 250 * FRAME_MS;

#[derive(Debug, Clone, Serialize)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Lists the active Windows playback devices. Runs on its own thread so COM
/// is initialised the way we need, whatever the caller's thread uses.
pub fn list_output_devices() -> Result<Vec<OutputDevice>> {
    thread::spawn(list_output_devices_mta)
        .join()
        .map_err(|_| anyhow!("device enumeration panicked"))?
}

fn list_output_devices_mta() -> Result<Vec<OutputDevice>> {
    let _ = wasapi::initialize_mta();
    let default_id = wasapi::get_default_device(&Direction::Render)
        .and_then(|d| d.get_id())
        .unwrap_or_default();
    let coll = wasapi::DeviceCollection::new(&Direction::Render)?;
    let mut out = Vec::new();
    for dev in &coll {
        let dev = dev?;
        let id = dev.get_id()?;
        out.push(OutputDevice {
            is_default: id == default_id,
            name: dev.get_friendlyname().unwrap_or_else(|_| "?".into()),
            id,
        });
    }
    Ok(out)
}

struct RingState {
    q: VecDeque<i16>,
    buffering: bool,
}

/// Jitter buffer between the capture thread and the sender.
pub struct Ring {
    state: Mutex<RingState>,
    /// Peak level of the last packet, for the UI meter (0..1, f32 bits).
    level: std::sync::atomic::AtomicU32,
}

impl Ring {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RingState {
                q: VecDeque::with_capacity(MAX_FRAMES * CHANNELS * 2),
                buffering: true,
            }),
            level: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn push(&self, samples: &[i16]) {
        let mut st = self.state.lock();
        st.q.extend(samples.iter().copied());
        if st.q.len() > MAX_FRAMES * CHANNELS {
            let excess = st.q.len() - PREFILL_FRAMES * CHANNELS;
            st.q.drain(..excess);
            debug!("ring: overflow, dropped {} frames", excess / CHANNELS);
        }
    }

    fn clear(&self) {
        let mut st = self.state.lock();
        st.q.clear();
        st.buffering = true;
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

impl AudioSource for Ring {
    fn read(&self, out: &mut [i16]) {
        let mut st = self.state.lock();
        if st.buffering {
            if st.q.len() >= PREFILL_FRAMES * CHANNELS {
                st.buffering = false;
            } else {
                out.fill(0);
                self.level.store(0f32.to_bits(), Ordering::Relaxed);
                return;
            }
        }
        if st.q.len() < out.len() {
            // Underrun: play what we have, then wait for the buffer to refill
            let have = st.q.len();
            for (o, s) in out.iter_mut().zip(st.q.drain(..)) {
                *o = s;
            }
            out[have..].fill(0);
            st.buffering = true;
        } else {
            let n = out.len();
            for (o, s) in out.iter_mut().zip(st.q.drain(..n)) {
                *o = s;
            }
            // Capture clock slightly faster than ours: drop one frame
            if st.q.len() > HIGH_FRAMES * CHANNELS {
                st.q.drain(..CHANNELS);
            }
        }
        drop(st);
        let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0;
        self.level.store(peak.to_bits(), Ordering::Relaxed);
    }
}

/// Live options the capture thread re-reads while running.
#[derive(Default)]
pub struct CaptureFlags {
    /// Undo the Windows volume of the device, so the PC volume only affects
    /// the PC's own speakers.
    pub compensate: AtomicBool,
    /// Mute the device while capturing, so only the HomePods play.
    pub mute_pc: AtomicBool,
}

impl CaptureFlags {
    pub fn new(compensate: bool, mute_pc: bool) -> Arc<Self> {
        Arc::new(Self {
            compensate: AtomicBool::new(compensate),
            mute_pc: AtomicBool::new(mute_pc),
        })
    }
}

pub struct Capture {
    ring: Arc<Ring>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// `device_id`: a render device id, or `None` to follow the Windows
    /// default output device (switching when it changes).
    pub fn start(device_id: Option<String>, ring: Arc<Ring>, flags: Arc<CaptureFlags>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let ring = ring.clone();
            let stop = stop.clone();
            thread::Builder::new()
                .name("wasapi-capture".into())
                .spawn(move || {
                    let _ = wasapi::initialize_mta();
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(e) = capture_run(device_id.as_deref(), &ring, &stop, &flags) {
                            warn!("capture: {e:#}, retrying");
                            ring.clear();
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                })
                .expect("spawn capture thread")
        };
        Self {
            ring,
            stop,
            thread: Some(thread),
        }
    }

    pub fn ring(&self) -> Arc<Ring> {
        self.ring.clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_device(device_id: Option<&str>) -> Result<wasapi::Device> {
    match device_id {
        None => Ok(wasapi::get_default_device(&Direction::Render)?),
        Some(id) => {
            let coll = wasapi::DeviceCollection::new(&Direction::Render)?;
            for dev in &coll {
                let dev = dev?;
                if dev.get_id()? == id {
                    return Ok(dev);
                }
            }
            Err(anyhow!("audio device not found"))
        }
    }
}

/// Runs until stopped, an error, or (when following the default device) the
/// default device changes, in which case it returns Ok to be restarted.
fn capture_run(
    device_id: Option<&str>,
    ring: &Ring,
    stop: &AtomicBool,
    flags: &CaptureFlags,
) -> Result<()> {
    let device = open_device(device_id)?;
    let current_id = device.get_id()?;
    info!(
        "capture: loopback on '{}'",
        device.get_friendlyname().unwrap_or_default()
    );

    // Float, so a quiet Windows volume loses no precision before we undo it
    let mut client = device.get_iaudioclient()?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);
    let mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: 1_000_000, // 100 ms
    };
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    ring.clear();

    let volume = EndpointVolume::open(Some(&current_id)).ok();
    if let Some(v) = &volume {
        info!(
            "capture: windows volume {:.0}% ({:.1} dB), applied in {}",
            v.level().unwrap_or(0.0) * 100.0,
            v.level_db().unwrap_or(0.0),
            if v.hardware_volume() { "hardware" } else { "software" }
        );
    }
    // Restores the PC speakers when this function returns, however it returns
    let mut pc_mute = volume.as_ref().map(|v| PcMute::new(v, &current_id));
    let mut gain = 1.0f32;
    let mut last_gain_check = Instant::now() - Duration::from_secs(1);

    let bytes_per_frame = CHANNELS * 4;
    let mut bytes = vec![0u8; 4096 * bytes_per_frame];
    let mut samples: Vec<i16> = Vec::with_capacity(4096 * CHANNELS);
    let mut last_default_check = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if last_gain_check.elapsed() > Duration::from_millis(200) {
            last_gain_check = Instant::now();
            if let Some(m) = &mut pc_mute {
                m.update(flags.mute_pc.load(Ordering::Relaxed));
            }
            let target = match (&volume, flags.compensate.load(Ordering::Relaxed)) {
                (Some(v), true) => v.compensation_gain(),
                _ => 1.0,
            };
            if (target - gain).abs() > 0.001 {
                debug!("capture: compensation gain {gain:.3} -> {target:.3}");
                gain = target;
            }
        }

        while let Some(frames) = capture.get_next_packet_size()? {
            if frames == 0 {
                break;
            }
            let need = frames as usize * bytes_per_frame;
            if bytes.len() < need {
                bytes.resize(need, 0);
            }
            let (got, info) = capture.read_from_device(&mut bytes[..need])?;
            let got = got as usize;
            samples.clear();
            if info.flags.silent {
                samples.resize(got * CHANNELS, 0);
            } else {
                samples.extend(bytes[..got * bytes_per_frame].chunks_exact(4).map(|b| {
                    let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * gain;
                    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
                }));
            }
            ring.push(&samples);
        }

        if device_id.is_none() && last_default_check.elapsed() > Duration::from_secs(1) {
            last_default_check = Instant::now();
            if let Ok(id) = wasapi::get_default_device(&Direction::Render).and_then(|d| d.get_id()) {
                if id != current_id {
                    info!("capture: default output device changed, switching");
                    let _ = client.stop_stream();
                    return Ok(());
                }
            }
        }
        thread::sleep(Duration::from_millis(3));
    }
    let _ = client.stop_stream();
    Ok(())
}
