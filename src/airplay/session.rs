//! One AirPlay 2 control session with one receiver: pairing, setup, volume,
//! keep-alive and teardown. Audio itself goes through the shared `Streamer`.

use super::cipher::FramedCipher;
use super::rtsp::{find, RtspClient};
use super::srp::SrpClient;
use super::{tlv, FRAMES_PER_PACKET, RTP_PAYLOAD_TYPE, SAMPLE_RATE};
use anyhow::{anyhow, bail, Context, Result};
use log::{debug, info, warn};
use plist::{Dictionary, Value};
use rand::RngCore;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

/// Identity of this sender, shared by all sessions.
#[derive(Clone)]
pub struct SenderIdentity {
    pub name: String,
    /// "AA:BB:CC:DD:EE:FF"
    pub device_id: String,
    /// 16 hex digits
    pub dacp_id: String,
    pub active_remote: u32,
}

impl SenderIdentity {
    pub fn random(name: &str) -> Self {
        let mut rng = rand::thread_rng();
        let mut mac = [0u8; 6];
        rng.fill_bytes(&mut mac);
        mac[0] = (mac[0] & 0xFE) | 0x02; // locally administered, unicast
        let device_id = mac
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        Self {
            name: name.to_string(),
            device_id,
            dacp_id: format!("{:016X}", rng.next_u64()),
            active_remote: rng.next_u32(),
        }
    }
}

/// What the streamer needs to send audio to this receiver.
#[derive(Clone)]
pub struct StreamTarget {
    pub ip: IpAddr,
    pub data_port: u16,
    pub control_port: u16,
    pub key: [u8; 32],
}

pub struct SessionParams {
    pub timing_port: u16,
    pub control_port: u16,
    /// Extra latency the receiver adds on top of our sync, in frames.
    pub latency_min_frames: u32,
    /// 0..=100, or None to leave the receiver's volume alone
    pub volume: Option<u8>,
}

pub struct Session {
    rtsp: RtspClient,
    url: String,
    events: Option<TcpStream>,
    pub target: StreamTarget,
    device_name: String,
}

// AirPlay status flags (GET /info "statusFlags")
const FLAG_PIN_REQUIRED: u64 = 1 << 3;
const FLAG_PASSWORD_REQUIRED: u64 = 1 << 7;
const FLAG_ONE_TIME_PAIRING_REQUIRED: u64 = 1 << 9;

fn dict_to_bplist(d: Dictionary) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &Value::Dictionary(d))?;
    Ok(out)
}

fn uint(n: u64) -> Value {
    Value::Integer(n.into())
}

pub fn volume_to_db(pct: u8) -> f32 {
    if pct == 0 {
        -144.0
    } else {
        -30.0 + 30.0 * (pct.min(100) as f32) / 100.0
    }
}

/// Inverse of `volume_to_db`.
pub fn db_to_volume(db: f32) -> u8 {
    if db <= -30.0 {
        0
    } else {
        ((db.min(0.0) + 30.0) / 30.0 * 100.0).round() as u8
    }
}

impl Session {
    pub fn connect(
        addr: SocketAddr,
        device_name: &str,
        me: &SenderIdentity,
        params: &SessionParams,
    ) -> Result<Session> {
        let common = vec![
            ("User-Agent".to_string(), "AirPlay/690.7.1".to_string()),
            ("X-Apple-Client-Name".to_string(), me.name.clone()),
            ("Client-Instance".to_string(), me.dacp_id.clone()),
            ("DACP-ID".to_string(), me.dacp_id.clone()),
            ("Active-Remote".to_string(), me.active_remote.to_string()),
        ];
        let mut rtsp = RtspClient::connect(addr, common)?;
        let local_ip = rtsp.local_addr()?.ip();
        info!("[{device_name}] connected to {addr} from {local_ip}");

        // 1. Device info, to catch the cases we can't handle
        let res = rtsp.request("GET", "/info", None, &[], &[])?;
        if res.status == 200 {
            if let Ok(Value::Dictionary(d)) = res.plist() {
                let flags = d
                    .get("statusFlags")
                    .and_then(|v| v.as_unsigned_integer())
                    .unwrap_or(0);
                debug!("[{device_name}] statusFlags=0x{flags:x}");
                if flags & (FLAG_PIN_REQUIRED | FLAG_ONE_TIME_PAIRING_REQUIRED) != 0 {
                    bail!(
                        "{device_name} demande un code PIN. Dans l'app Maison, réglez « Autoriser l'accès aux haut-parleurs et TV » sur « Toute personne sur le même réseau »."
                    );
                }
                if flags & FLAG_PASSWORD_REQUIRED != 0 {
                    bail!("{device_name} est protégé par un mot de passe AirPlay (non pris en charge).");
                }
            }
        }

        // 2. Transient pair-setup (SRP with the fixed PIN 3939)
        let shared = pair_transient(&mut rtsp, device_name)?;
        rtsp.enable_encryption(FramedCipher::control(&shared));
        debug!("[{device_name}] paired, control channel encrypted");

        let mut key = [0u8; 32];
        key.copy_from_slice(&shared[..32]);

        let session_id = rand::thread_rng().next_u32();
        let url = match local_ip {
            IpAddr::V4(ip) => format!("rtsp://{ip}/{session_id}"),
            IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session_id}"),
        };

        // 3. SETUP (session): timing over NTP, served by us
        let mut d = Dictionary::new();
        d.insert("deviceID".into(), Value::String(me.device_id.clone()));
        d.insert(
            "sessionUUID".into(),
            Value::String(uuid::Uuid::new_v4().to_string().to_uppercase()),
        );
        d.insert("timingPort".into(), uint(params.timing_port as u64));
        d.insert("timingProtocol".into(), Value::String("NTP".into()));
        d.insert("name".into(), Value::String(me.name.clone()));
        let res = rtsp.request(
            "SETUP",
            &url,
            Some("application/x-apple-binary-plist"),
            &dict_to_bplist(d)?,
            &[],
        )?;
        if res.status != 200 {
            bail!("SETUP (session) refusé par {device_name} ({} {})", res.status, res.reason);
        }
        let event_port = match res.plist() {
            Ok(Value::Dictionary(d)) => d
                .get("eventPort")
                .and_then(|v| v.as_unsigned_integer())
                .unwrap_or(0) as u16,
            _ => 0,
        };

        // Reverse event channel. Receivers expect it, and we must answer what
        // they send on it.
        let events = if event_port != 0 {
            match open_events(SocketAddr::new(addr.ip(), event_port), &shared, device_name) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("[{device_name}] event channel: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        // 4. RECORD
        let res = rtsp.request("RECORD", &url, None, &[], &[])?;
        if res.status != 200 {
            bail!("RECORD refusé par {device_name} ({} {})", res.status, res.reason);
        }

        // 5. SETUP (stream): realtime ALAC 44100/16/2
        let mut stream = Dictionary::new();
        stream.insert("audioFormat".into(), uint(0x40000));
        stream.insert("audioMode".into(), Value::String("default".into()));
        stream.insert("controlPort".into(), uint(params.control_port as u64));
        stream.insert("ct".into(), uint(2));
        stream.insert("isMedia".into(), Value::Boolean(true));
        stream.insert("latencyMax".into(), uint(88200));
        stream.insert("latencyMin".into(), uint(params.latency_min_frames as u64));
        stream.insert("shk".into(), Value::Data(key.to_vec()));
        stream.insert("spf".into(), uint(FRAMES_PER_PACKET as u64));
        stream.insert("sr".into(), uint(SAMPLE_RATE as u64));
        stream.insert("type".into(), uint(RTP_PAYLOAD_TYPE as u64));
        stream.insert("supportsDynamicStreamID".into(), Value::Boolean(false));
        stream.insert("streamConnectionID".into(), uint(session_id as u64));
        let mut root = Dictionary::new();
        root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let res = rtsp.request(
            "SETUP",
            &url,
            Some("application/x-apple-binary-plist"),
            &dict_to_bplist(root)?,
            &[],
        )?;
        if res.status != 200 {
            bail!("SETUP (stream) refusé par {device_name} ({} {})", res.status, res.reason);
        }
        let (data_port, control_port) = match res.plist()? {
            Value::Dictionary(d) => {
                let s = d
                    .get("streams")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_dictionary())
                    .ok_or_else(|| anyhow!("no stream in SETUP response"))?;
                let get = |k: &str| s.get(k).and_then(|v| v.as_unsigned_integer()).unwrap_or(0) as u16;
                (get("dataPort"), get("controlPort"))
            }
            _ => bail!("unexpected SETUP response"),
        };
        if data_port == 0 || control_port == 0 {
            bail!("{device_name} n'a pas donné de ports audio");
        }
        info!("[{device_name}] stream ready: data={data_port} control={control_port}");

        let mut session = Session {
            rtsp,
            url,
            events,
            target: StreamTarget {
                ip: addr.ip(),
                data_port,
                control_port,
                key,
            },
            device_name: device_name.to_string(),
        };

        // 6. Volume last (some receivers ignore it otherwise)
        if let Some(v) = params.volume {
            session.set_volume(v)?;
        }
        Ok(session)
    }

    pub fn set_volume(&mut self, pct: u8) -> Result<()> {
        self.set_volume_raw(&format!("{:.6}", volume_to_db(pct)))
    }

    /// Sends `volume: <value>` verbatim (value in dB, -30..0, -144 = mute).
    pub fn set_volume_raw(&mut self, value: &str) -> Result<()> {
        let body = format!("volume: {value}\r\n");
        let res = self.rtsp.request(
            "SET_PARAMETER",
            &self.url.clone(),
            Some("text/parameters"),
            body.as_bytes(),
            &[],
        )?;
        if res.status != 200 {
            warn!("[{}] volume {value} refused: {}", self.device_name, res.status);
        }
        Ok(())
    }

    /// The receiver's current volume in dB, as it reports it.
    pub fn get_volume(&mut self) -> Result<f32> {
        let res = self.rtsp.request(
            "GET_PARAMETER",
            &self.url.clone(),
            Some("text/parameters"),
            b"volume\r\n",
            &[],
        )?;
        let text = String::from_utf8_lossy(&res.body);
        text.split(':')
            .nth(1)
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| anyhow!("unexpected GET_PARAMETER reply ({}): {text}", res.status))
    }

    pub fn keep_alive(&mut self) -> Result<()> {
        let res = self.rtsp.request("POST", "/feedback", None, &[], &[])?;
        if res.status != 200 {
            bail!("feedback refused: {}", res.status);
        }
        Ok(())
    }

    pub fn teardown(mut self) {
        let body = dict_to_bplist(Dictionary::new()).unwrap_or_default();
        let url = self.url.clone();
        if let Err(e) = self.rtsp.request(
            "TEARDOWN",
            &url,
            Some("application/x-apple-binary-plist"),
            &body,
            &[],
        ) {
            debug!("[{}] teardown: {e:#}", self.device_name);
        }
        if let Some(ev) = self.events.take() {
            ev.shutdown(Shutdown::Both).ok();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(ev) = self.events.take() {
            ev.shutdown(Shutdown::Both).ok();
        }
    }
}

/// Transient pair-setup: M1..M4 with SRP, PIN 3939. Returns the 64-byte
/// shared secret.
fn pair_transient(rtsp: &mut RtspClient, device_name: &str) -> Result<Vec<u8>> {
    let hkp = [("X-Apple-HKP", "4")];
    let ct = Some("application/octet-stream");

    let m1 = tlv::encode(&[(tlv::STATE, &[1]), (tlv::METHOD, &[0]), (tlv::FLAGS, &[0x10])]);
    let res = rtsp.request("POST", "/pair-setup", ct, &m1, &hkp)?;
    if res.status == 470 {
        bail!(
            "{device_name} refuse la connexion. Dans l'app Maison > Réglages de la maison > Haut-parleurs et TV, autorisez « Toute personne sur le même réseau »."
        );
    }
    if res.status != 200 {
        bail!("pair-setup M1 refusé ({} {})", res.status, res.reason);
    }
    let m2 = tlv::decode(&res.body);
    if let Some(err) = m2.get(&tlv::ERROR) {
        bail!("pair-setup M2: erreur {err:?}");
    }
    let salt = m2.get(&tlv::SALT).ok_or_else(|| anyhow!("pair-setup M2: no salt"))?;
    let b = m2
        .get(&tlv::PUBLIC_KEY)
        .ok_or_else(|| anyhow!("pair-setup M2: no public key"))?;

    let srp = SrpClient::new("Pair-Setup", "3939");
    let proof = srp.process_challenge(salt, b)?;
    let a = srp.public_a();
    let m3 = tlv::encode(&[(tlv::STATE, &[3]), (tlv::PUBLIC_KEY, &a), (tlv::PROOF, &proof.m1)]);
    let res = rtsp.request("POST", "/pair-setup", ct, &m3, &hkp)?;
    if res.status != 200 {
        bail!("pair-setup M3 refusé ({} {})", res.status, res.reason);
    }
    let m4 = tlv::decode(&res.body);
    if let Some(err) = m4.get(&tlv::ERROR) {
        bail!("pair-setup M4: erreur d'authentification {err:?}");
    }
    let hamk = m4
        .get(&tlv::PROOF)
        .ok_or_else(|| anyhow!("pair-setup M4: no proof"))?;
    if *hamk != proof.expected_hamk {
        bail!("pair-setup M4: la preuve de l'enceinte est invalide");
    }
    Ok(proof.session_key)
}

/// Connects the event channel and answers everything the receiver sends on
/// it with 200 OK, in a background thread. Returns a handle to close it.
fn open_events(addr: SocketAddr, shared: &[u8], device_name: &str) -> Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .with_context(|| format!("connect {addr}"))?;
    let handle = stream.try_clone()?;
    let mut cipher = FramedCipher::events(shared);
    let name = device_name.to_string();
    thread::Builder::new()
        .name(format!("events-{name}"))
        .spawn(move || {
            let mut stream = stream;
            let mut raw = Vec::new();
            let mut plain = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                raw.extend_from_slice(&buf[..n]);
                if let Err(e) = cipher.decrypt_available(&mut raw, &mut plain) {
                    warn!("[{name}] events: {e:#}");
                    break;
                }
                // Answer every complete request
                while let Some(end) = find(&plain, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&plain[..end]).to_string();
                    let len: usize = header_value(&head, "Content-Length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if plain.len() < end + 4 + len {
                        break;
                    }
                    let body: Vec<u8> = plain.drain(..end + 4 + len).skip(end + 4).collect();
                    // HomePods send Siri / touch commands here
                    let msg = plist::Value::from_reader(std::io::Cursor::new(&body)).ok();
                    match msg.as_ref().and_then(crate::remote::from_media_remote) {
                        Some(cmd) => crate::remote::dispatch(addr.ip(), cmd),
                        None => info!(
                            "[{name}] event: {} {}",
                            head.lines().next().unwrap_or(""),
                            msg.map(|v| format!("{v:?}"))
                                .unwrap_or_else(|| format!("{} bytes", body.len()))
                        ),
                    }
                    let cseq = header_value(&head, "CSeq").unwrap_or_else(|| "0".into());
                    let reply = format!(
                        "RTSP/1.0 200 OK\r\nServer: AirTunes/690.7.1\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n"
                    );
                    match cipher.encrypt(reply.as_bytes()) {
                        Ok(w) => {
                            if stream.write_all(&w).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            }
            debug!("[{name}] event channel closed");
        })?;
    Ok(handle)
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}
