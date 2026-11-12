//! Windows endpoint (master) volume of a playback device.

use anyhow::Result;
use windows::core::HSTRING;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

pub struct EndpointVolume(IAudioEndpointVolume);

impl EndpointVolume {
    /// `device_id`: a render device id, or None for the default output.
    pub fn open(device_id: Option<&str>) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = match device_id {
                Some(id) => en.GetDevice(&HSTRING::from(id))?,
                None => en.GetDefaultAudioEndpoint(eRender, eConsole)?,
            };
            Ok(Self(dev.Activate(CLSCTX_ALL, None)?))
        }
    }

    /// 0.0..=1.0
    pub fn level(&self) -> Result<f32> {
        Ok(unsafe { self.0.GetMasterVolumeLevelScalar()? })
    }

    pub fn set_level(&self, level: f32) -> Result<()> {
        unsafe { self.0.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null())? };
        Ok(())
    }

    pub fn muted(&self) -> Result<bool> {
        Ok(unsafe { self.0.GetMute()?.as_bool() })
    }

    pub fn set_mute(&self, mute: bool) -> Result<()> {
        unsafe { self.0.SetMute(mute, std::ptr::null())? };
        Ok(())
    }

    /// Current attenuation in dB (0 = full, negative = quieter).
    pub fn level_db(&self) -> Result<f32> {
        Ok(unsafe { self.0.GetMasterVolumeLevel()? })
    }

    /// True if the device applies its volume in hardware, in which case the
    /// loopback stream is not attenuated by it.
    pub fn hardware_volume(&self) -> bool {
        const ENDPOINT_HARDWARE_SUPPORT_VOLUME: u32 = 1;
        unsafe { self.0.QueryHardwareSupport() }
            .map(|f| f & ENDPOINT_HARDWARE_SUPPORT_VOLUME != 0)
            .unwrap_or(false)
    }

    /// Linear gain that undoes the software master volume (1.0 if none).
    /// A muted endpoint is not attenuated at all in loopback (measured on
    /// software and hardware volume devices), so nothing to undo then either.
    pub fn compensation_gain(&self) -> f32 {
        if self.hardware_volume() || self.muted().unwrap_or(false) {
            return 1.0;
        }
        match self.level_db() {
            // Cap at +60 dB: below that Windows is essentially silent anyway
            Ok(db) => 10f32.powf((-db).clamp(0.0, 60.0) / 20.0),
            Err(_) => 1.0,
        }
    }
}

/// While streaming, the PC speakers are muted (Windows still hands the full
/// signal to loopback capture). This file remembers that we did it, so a crash
/// or a kill does not leave the PC muted: the next launch unmutes it.
fn mute_marker() -> std::path::PathBuf {
    crate::data_dir().join("pc-muted-by-ho-my-pod")
}

/// Mutes a device for the lifetime of the guard (unless it already was
/// muted, or the user unmutes it meanwhile), then unmutes it.
pub struct PcMute<'a> {
    vol: &'a EndpointVolume,
    device_id: String,
    active: bool,
    /// The user unmuted the PC while we held it: leave it alone from then on
    overridden: bool,
}

impl<'a> PcMute<'a> {
    pub fn new(vol: &'a EndpointVolume, device_id: &str) -> Self {
        Self {
            vol,
            device_id: device_id.to_string(),
            active: false,
            overridden: false,
        }
    }

    /// Called periodically with whether the PC should be muted.
    pub fn update(&mut self, want: bool) {
        let muted = self.vol.muted().unwrap_or(false);
        if want && !self.active {
            if !muted && !self.overridden {
                let _ = std::fs::create_dir_all(crate::data_dir());
                let _ = std::fs::write(mute_marker(), &self.device_id);
                if self.vol.set_mute(true).is_ok() {
                    log::info!("pc speakers muted while streaming");
                    self.active = true;
                }
            }
        } else if self.active && !want {
            self.release();
        } else if self.active && !muted {
            log::info!("pc speakers unmuted by the user, leaving them on");
            self.active = false;
            self.overridden = true;
            let _ = std::fs::remove_file(mute_marker());
        }
    }

    fn release(&mut self) {
        if self.active {
            let _ = self.vol.set_mute(false);
            let _ = std::fs::remove_file(mute_marker());
            log::info!("pc speakers unmuted");
            self.active = false;
        }
    }
}

impl Drop for PcMute<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// If a previous run muted the PC and did not get to undo it, undo it now.
pub fn restore_pc_mute() {
    let Ok(id) = std::fs::read_to_string(mute_marker()) else { return };
    let id = id.trim().to_string();
    let _ = std::thread::spawn(move || {
        if let Ok(v) = EndpointVolume::open(Some(&id)) {
            let _ = v.set_mute(false);
            log::info!("unmuted the pc speakers left muted by a previous run");
        }
    })
    .join();
    let _ = std::fs::remove_file(mute_marker());
}
