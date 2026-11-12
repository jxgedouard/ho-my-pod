//! The engine the UI talks to: discovery + capture + AirPlay sessions, and
//! the user's settings.

use crate::airplay::session::{db_to_volume, SenderIdentity, Session, SessionParams};
use crate::airplay::streamer::Streamer;
use crate::airplay::SAMPLE_RATE;
use crate::capture::{self, Capture, CaptureFlags, OutputDevice, Ring};
use crate::discovery::{self, Device};
use crate::remote::{self, RemoteCmd};
use anyhow::Result;
use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};
use log::{error, info, warn};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// Extra latency the receivers add on top of our sync (frames).
const LATENCY_MIN_FRAMES: u32 = 11025;
const KEEP_ALIVE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LatencyPreset {
    Low,
    Balanced,
    Stable,
}

impl LatencyPreset {
    /// How far ahead of playback we send audio.
    fn lead_ms(self) -> u32 {
        match self {
            LatencyPreset::Low => 100,
            LatencyPreset::Balanced => 300,
            LatencyPreset::Stable => 1000,
        }
    }

    /// Approximate total delay, for display.
    pub fn total_ms(self) -> u32 {
        self.lead_ms() + LATENCY_MIN_FRAMES * 1000 / SAMPLE_RATE
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub latency: LatencyPreset,
    /// None = follow the Windows default output
    pub output_device: Option<String>,
    pub volumes: HashMap<String, u8>,
    /// Undo the Windows volume so it only affects the PC speakers
    pub independent_volume: bool,
    /// Mute the PC speakers while streaming to the HomePods
    pub mute_pc: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency: LatencyPreset::Balanced,
            output_device: None,
            volumes: HashMap::new(),
            independent_volume: true,
            mute_pc: true,
        }
    }
}

fn settings_path() -> PathBuf {
    crate::data_dir().join("settings.json")
}

fn load_settings() -> Settings {
    std::fs::read(settings_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_settings(s: &Settings) {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let _ = std::fs::write(path, json);
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Connecting,
    Playing,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    #[serde(flatten)]
    pub device: Device,
    pub status: Status,
    pub error: Option<String>,
    pub volume: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub devices: Vec<DeviceView>,
    pub latency: LatencyPreset,
    pub latency_ms: u32,
    pub output_device: Option<String>,
    pub outputs: Vec<OutputDevice>,
    pub independent_volume: bool,
    pub mute_pc: bool,
}

enum Cmd {
    Volume(u8),
    Stop,
}

struct Link {
    status: Status,
    error: Option<String>,
    tx: Option<Sender<Cmd>>,
}

struct State {
    devices: Vec<Device>,
    links: HashMap<String, Link>,
    settings: Settings,
    capture: Option<Capture>,
    outputs: Vec<OutputDevice>,
}

struct Inner {
    state: Mutex<State>,
    ring: Arc<Ring>,
    flags: Arc<CaptureFlags>,
    streamer: Streamer,
    me: SenderIdentity,
    listener: Box<dyn Fn(Snapshot) + Send + Sync>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
    _mdns: Arc<mdns_sd::ServiceDaemon>,
}

impl Engine {
    pub fn new(listener: impl Fn(Snapshot) + Send + Sync + 'static) -> Result<Engine> {
        let settings = load_settings();
        // A previous run killed while streaming may have left the PC muted
        crate::winaudio::restore_pc_mute();
        let flags = CaptureFlags::new(settings.independent_volume, settings.mute_pc);
        let ring = Arc::new(Ring::new());
        let lead = settings.latency.lead_ms() * SAMPLE_RATE / 1000;
        let streamer = Streamer::new(ring.clone(), lead)?;
        let outputs = capture::list_output_devices().unwrap_or_else(|e| {
            warn!("could not list output devices: {e:#}");
            Vec::new()
        });
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                devices: Vec::new(),
                links: HashMap::new(),
                settings,
                capture: None,
                outputs,
            }),
            ring,
            streamer,
            me: SenderIdentity::random("Ho My Pod (PC)"),
            listener: Box::new(listener),
            flags,
        });

        // Debug aid: HMP_AUTOCONNECT="Name A,Name B" connects as soon as found
        let autoconnect: Vec<String> = std::env::var("HMP_AUTOCONNECT")
            .map(|v| v.split(',').map(|s| s.trim().to_lowercase()).collect())
            .unwrap_or_default();

        let weak = Arc::downgrade(&inner);
        let mdns = discovery::start(move |list| {
            let Some(inner) = weak.upgrade() else { return };
            let fresh: Vec<String> = {
                let mut st = inner.state.lock();
                let fresh = list
                    .iter()
                    .filter(|d| autoconnect.contains(&d.name.to_lowercase()))
                    .filter(|d| !st.devices.iter().any(|old| old.id == d.id))
                    .map(|d| d.id.clone())
                    .collect();
                st.devices = list;
                fresh
            };
            inner.notify();
            for id in fresh {
                inner.connect(&id);
            }
        })?;

        Ok(Engine {
            inner,
            _mdns: Arc::new(mdns),
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.snapshot()
    }

    pub fn level(&self) -> f32 {
        self.inner.ring.level()
    }

    pub fn connect(&self, id: &str) {
        self.inner.connect(id);
    }

    pub fn disconnect(&self, id: &str) {
        let mut st = self.inner.state.lock();
        if let Some(link) = st.links.get_mut(id) {
            match link.tx.take() {
                Some(tx) => {
                    let _ = tx.send(Cmd::Stop);
                }
                None => {
                    st.links.remove(id);
                }
            }
        }
        drop(st);
        self.inner.notify();
    }

    pub fn set_volume(&self, id: &str, volume: u8) {
        let volume = volume.min(100);
        let mut st = self.inner.state.lock();
        st.settings.volumes.insert(id.to_string(), volume);
        if let Some(Link { tx: Some(tx), .. }) = st.links.get(id) {
            let _ = tx.send(Cmd::Volume(volume));
        }
        let settings = st.settings.clone();
        drop(st);
        save_settings(&settings);
    }

    pub fn set_latency(&self, preset: LatencyPreset) {
        let mut st = self.inner.state.lock();
        st.settings.latency = preset;
        let settings = st.settings.clone();
        drop(st);
        self.inner
            .streamer
            .set_lead_frames(preset.lead_ms() * SAMPLE_RATE / 1000);
        save_settings(&settings);
        self.inner.notify();
    }

    pub fn set_output_device(&self, id: Option<String>) {
        let mut st = self.inner.state.lock();
        st.settings.output_device = id.clone();
        let settings = st.settings.clone();
        // Restart the capture on the new device if it is running
        let restart = st.capture.take().is_some();
        drop(st);
        if restart {
            let cap = Capture::start(id, self.inner.ring.clone(), self.inner.flags.clone());
            self.inner.state.lock().capture = Some(cap);
        }
        save_settings(&settings);
        self.inner.notify();
    }

    pub fn set_independent_volume(&self, on: bool) {
        let mut st = self.inner.state.lock();
        st.settings.independent_volume = on;
        let settings = st.settings.clone();
        drop(st);
        self.inner.flags.compensate.store(on, Ordering::Relaxed);
        save_settings(&settings);
        self.inner.notify();
    }

    pub fn set_mute_pc(&self, on: bool) {
        let mut st = self.inner.state.lock();
        st.settings.mute_pc = on;
        let settings = st.settings.clone();
        drop(st);
        self.inner.flags.mute_pc.store(on, Ordering::Relaxed);
        save_settings(&settings);
        self.inner.notify();
    }

    /// Listens for the commands the speakers relay (Siri, touch, Control
    /// Center). Volume reports are handled here; media commands go to
    /// `on_media`.
    pub fn start_remote(&self, on_media: impl Fn(RemoteCmd) + Send + Sync + 'static) -> Result<()> {
        let weak = Arc::downgrade(&self.inner);
        remote::start(&self._mdns, &self.inner.me.dacp_id, self.inner.me.active_remote, move |ip, cmd| {
            match cmd {
                RemoteCmd::DeviceVolume(db) => {
                    let Some(inner) = weak.upgrade() else { return };
                    let id = inner.state.lock().devices.iter().find(|d| d.ip == ip).map(|d| d.id.clone());
                    if let Some(id) = id {
                        inner.device_volume(&id, db_to_volume(db));
                    }
                }
                other => on_media(other),
            }
        })
    }

    pub fn refresh_outputs(&self) {
        let outputs = capture::list_output_devices().unwrap_or_default();
        self.inner.state.lock().outputs = outputs;
        self.inner.notify();
    }

    /// Disconnects everything cleanly (used on exit).
    pub fn shutdown(&self) {
        let ids: Vec<String> = self.inner.state.lock().links.keys().cloned().collect();
        for id in &ids {
            self.disconnect(id);
        }
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if self.inner.state.lock().links.values().all(|l| l.tx.is_none() && l.status != Status::Playing) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // Stop the capture here (it unmutes the PC) rather than racing the exit
        let capture = self.inner.state.lock().capture.take();
        drop(capture);
    }
}

impl Inner {
    fn connect(self: &Arc<Self>, id: &str) {
        let mut st = self.state.lock();
        let Some(device) = st.devices.iter().find(|d| d.id == id).cloned() else {
            return;
        };
        if let Some(l) = st.links.get(id) {
            if matches!(l.status, Status::Connecting | Status::Playing) {
                return;
            }
        }
        let volume = *st.settings.volumes.get(id).unwrap_or(&35);
        let (tx, rx) = unbounded();
        st.links.insert(
            id.to_string(),
            Link {
                status: Status::Connecting,
                error: None,
                tx: Some(tx),
            },
        );
        drop(st);
        self.notify();

        let weak = Arc::downgrade(self);
        thread::Builder::new()
            .name(format!("session-{}", device.name))
            .spawn(move || session_thread(weak, device, volume, rx))
            .ok();
    }

    fn snapshot(&self) -> Snapshot {
        let st = self.state.lock();
        let devices = st
            .devices
            .iter()
            .map(|d| {
                let link = st.links.get(&d.id);
                DeviceView {
                    device: d.clone(),
                    status: link.map(|l| l.status).unwrap_or(Status::Idle),
                    error: link.and_then(|l| l.error.clone()),
                    volume: *st.settings.volumes.get(&d.id).unwrap_or(&35),
                }
            })
            .collect();
        Snapshot {
            devices,
            latency: st.settings.latency,
            latency_ms: st.settings.latency.total_ms(),
            output_device: st.settings.output_device.clone(),
            outputs: st.outputs.clone(),
            independent_volume: st.settings.independent_volume,
            mute_pc: st.settings.mute_pc,
        }
    }

    /// The receiver reported its volume: reflect it, without sending it back.
    fn device_volume(&self, id: &str, volume: u8) {
        let mut st = self.state.lock();
        if st.settings.volumes.get(id) == Some(&volume) {
            return;
        }
        st.settings.volumes.insert(id.to_string(), volume);
        let settings = st.settings.clone();
        drop(st);
        save_settings(&settings);
        self.notify();
    }

    fn notify(&self) {
        (self.listener)(self.snapshot());
    }

    fn set_status(&self, id: &str, status: Status, error: Option<String>) {
        let mut st = self.state.lock();
        match status {
            Status::Idle => {
                st.links.remove(id);
            }
            _ => {
                let link = st.links.entry(id.to_string()).or_insert(Link {
                    status,
                    error: None,
                    tx: None,
                });
                link.status = status;
                link.error = error;
                if status == Status::Error {
                    link.tx = None;
                }
            }
        }
        // Capture runs only while something is playing
        let playing = st.links.values().any(|l| l.status == Status::Playing);
        if playing && st.capture.is_none() {
            let dev = st.settings.output_device.clone();
            st.capture = Some(Capture::start(dev, self.ring.clone(), self.flags.clone()));
        }
        let stop_capture = if !playing && !st.links.values().any(|l| l.status == Status::Connecting) {
            st.capture.take()
        } else {
            None
        };
        drop(st);
        drop(stop_capture); // joins the capture thread outside the lock
        self.notify();
    }
}

fn session_thread(inner: Weak<Inner>, device: Device, volume: u8, rx: crossbeam_channel::Receiver<Cmd>) {
    let Some(engine) = inner.upgrade() else { return };
    let addr = SocketAddr::new(device.ip, device.port);
    let params = SessionParams {
        timing_port: engine.streamer.timing_port(),
        control_port: engine.streamer.control_port(),
        latency_min_frames: LATENCY_MIN_FRAMES,
        volume: None, // adopt the receiver's volume (read right after)
    };

    // One retry: SRP has a rare edge case (salt with a leading zero byte)
    let session = match Session::connect(addr, &device.name, &engine.me, &params) {
        Ok(s) => Ok(s),
        Err(e) => {
            warn!("[{}] first attempt failed: {e:#}", device.name);
            Session::connect(addr, &device.name, &engine.me, &params)
        }
    };

    // The user may have cancelled while we were connecting
    if let Ok(Cmd::Stop) = rx.try_recv() {
        if let Ok(s) = session {
            s.teardown();
        }
        engine.set_status(&device.id, Status::Idle, None);
        return;
    }

    let mut s = match session {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{e}");
            error!("[{}] connection failed: {e:#}", device.name);
            engine.set_status(&device.id, Status::Error, Some(msg));
            return;
        }
    };

    if let Err(e) = engine.streamer.add_target(&device.id, &s.target) {
        engine.set_status(&device.id, Status::Error, Some(format!("{e}")));
        s.teardown();
        return;
    }
    // Show the receiver's real volume rather than imposing ours, like iOS does
    let mut known_volume = match s.get_volume() {
        Ok(db) => {
            let v = db_to_volume(db);
            engine.device_volume(&device.id, v);
            Some(v)
        }
        Err(e) => {
            warn!("[{}] can't read volume ({e:#}), setting ours", device.name);
            if s.set_volume(volume).is_err() {
                warn!("[{}] volume refused", device.name);
            }
            None
        }
    };
    engine.set_status(&device.id, Status::Playing, None);
    info!("[{}] playing (volume {:?})", device.name, known_volume);
    drop(engine);

    let mut last_alive = Instant::now();
    let mut last_local_change = Instant::now();
    let mut pending: Option<Cmd> = None;
    let outcome: Option<String> = loop {
        let wait = KEEP_ALIVE.saturating_sub(last_alive.elapsed());
        let cmd = match pending.take() {
            Some(c) => Ok(c),
            None => rx.recv_timeout(wait),
        };
        match cmd {
            Ok(Cmd::Volume(mut v)) => {
                // Coalesce slider drags, without swallowing a Stop
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::Volume(nv)) => v = nv,
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if let Err(e) = s.set_volume(v) {
                    break Some(format!("Connexion perdue ({e})"));
                }
                known_volume = Some(v);
                last_local_change = Instant::now();
            }
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break None,
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = s.keep_alive() {
                    break Some(format!("Connexion perdue ({e})"));
                }
                last_alive = Instant::now();
                // Volume changed on the speaker itself (Siri, touch, iPhone)?
                if known_volume.is_some() && last_local_change.elapsed() > Duration::from_millis(1500) {
                    if let Ok(db) = s.get_volume() {
                        let v = db_to_volume(db);
                        if Some(v) != known_volume {
                            info!("[{}] volume changed on the device: {v}", device.name);
                            known_volume = Some(v);
                            if let Some(engine) = inner.upgrade() {
                                engine.device_volume(&device.id, v);
                            }
                        }
                    }
                }
            }
        }
    };

    let Some(engine) = inner.upgrade() else { return };
    engine.streamer.remove_target(&device.id);
    s.teardown();
    match outcome {
        None => {
            info!("[{}] stopped", device.name);
            engine.set_status(&device.id, Status::Idle, None);
        }
        Some(msg) => {
            warn!("[{}] {msg}", device.name);
            engine.set_status(&device.id, Status::Error, Some(msg));
        }
    }
}
