//! Ho My Pod — stream the Windows system audio to HomePods over AirPlay 2.
//!
//! Layout:
//! - `discovery`: finds AirPlay receivers on the LAN (mDNS `_airplay._tcp`).
//! - `capture`:   WASAPI loopback capture of what Windows is playing.
//! - `airplay`:   the AirPlay 2 sender (pairing, RTSP, RTP audio, timing).
//! - `engine`:    glues the three together and is what the UI talks to.

pub mod airplay;
pub mod capture;
pub mod discovery;
pub mod engine;
pub mod firewall;
pub mod media;
pub mod remote;
pub mod winaudio;

/// %APPDATA%\HoMyPod: settings, logs, and the "PC muted by us" marker.
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("HoMyPod")
}
