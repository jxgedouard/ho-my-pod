//! Command-line test tool for the AirPlay engine.
//!
//!   hmp-cli list
//!   hmp-cli play <name-substring>[,<name-substring>...] [--tone] [--secs N] [--lead-ms N] [--latency-min N]

use ho_my_pod::airplay::session::{SenderIdentity, Session, SessionParams};
use ho_my_pod::airplay::streamer::{AudioSource, Streamer};
use ho_my_pod::airplay::SAMPLE_RATE;
use ho_my_pod::capture::{Capture, CaptureFlags, Ring};
use ho_my_pod::discovery::{self, Device};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

struct Tone {
    phase: Mutex<f64>,
}

impl AudioSource for Tone {
    fn read(&self, out: &mut [i16]) {
        let mut phase = self.phase.lock();
        for frame in out.chunks_mut(2) {
            let s = (*phase * 2.0 * std::f64::consts::PI).sin() * 0.2 * 32767.0;
            frame[0] = s as i16;
            frame[1] = s as i16;
            *phase += 440.0 / SAMPLE_RATE as f64;
            if *phase >= 1.0 {
                *phase -= 1.0;
            }
        }
    }
}

/// Does the Windows master volume of a device change what loopback captures?
/// Plays a sine into the device (pick one with no speakers attached, e.g. the
/// Steam virtual device), captures it back, and changes the device volume.
fn loopback_test(name_part: &str, compensate: bool, silent_only: bool) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dev = ho_my_pod::capture::list_output_devices()?
        .into_iter()
        .find(|o| o.name.contains(name_part))
        .ok_or_else(|| anyhow::anyhow!("no output device containing '{name_part}'"))?;
    println!("device: {}", dev.name);
    let vol = ho_my_pod::winaudio::EndpointVolume::open(Some(&dev.id))?;
    let original = vol.level()?;
    let original_mute = vol.muted()?;
    // With real speakers attached, mute before any sound is rendered
    if silent_only {
        vol.set_mute(true)?;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let render = {
        let stop = stop.clone();
        let id = dev.id.clone();
        std::thread::spawn(move || -> anyhow::Result<()> {
            use wasapi::*;
            let _ = initialize_mta();
            let coll = DeviceCollection::new(&Direction::Render)?;
            let device = (&coll)
                .into_iter()
                .filter_map(|d| d.ok())
                .find(|d| d.get_id().map(|i| i == id).unwrap_or(false))
                .ok_or_else(|| anyhow::anyhow!("device vanished"))?;
            let mut client = device.get_iaudioclient()?;
            let fmt = WaveFormat::new(32, 32, &SampleType::Float, 44100, 2, None);
            let (_, min) = client.get_device_period()?;
            client.initialize_client(
                &fmt,
                &Direction::Render,
                &StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: min },
            )?;
            let ev = client.set_get_eventhandle()?;
            let rc = client.get_audiorenderclient()?;
            client.start_stream()?;
            let mut phase = 0f32;
            while !stop.load(Ordering::Relaxed) {
                let n = client.get_available_space_in_frames()? as usize;
                let mut buf = Vec::with_capacity(n * 8);
                for _ in 0..n {
                    let s = (phase * std::f32::consts::TAU).sin() * 0.5;
                    phase = (phase + 440.0 / 44100.0).fract();
                    buf.extend_from_slice(&s.to_le_bytes());
                    buf.extend_from_slice(&s.to_le_bytes());
                }
                rc.write_to_device(n, &buf, None)?;
                let _ = ev.wait_for_event(1000);
            }
            client.stop_stream()?;
            Ok(())
        })
    };

    let ring = Arc::new(Ring::new());
    let _cap = Capture::start(Some(dev.id.clone()), ring.clone(), CaptureFlags::new(compensate, false));
    let mut buf = vec![0i16; 352 * 2];
    let mut measure = |label: &str| {
        std::thread::sleep(Duration::from_millis(700));
        let mut peak = 0f32;
        for _ in 0..60 {
            ring.read(&mut buf);
            peak = peak.max(ring.level());
            std::thread::sleep(Duration::from_millis(8));
        }
        println!("{label:<28} captured peak = {peak:.3}");
    };
    if silent_only {
        vol.set_level(1.0)?;
        measure("muted, volume 100%");
    } else {
        vol.set_mute(false)?;
        vol.set_level(1.0)?;
        measure("windows volume 100%");
        vol.set_level(0.3)?;
        measure("windows volume 30%");
        vol.set_mute(true)?;
        measure("muted, volume 30%");
        vol.set_mute(false)?;
    }
    vol.set_level(0.0)?;
    println!("(volume 0% = {:.1} dB)", vol.level_db()?);
    measure("windows volume 0%");
    // Stop the tone before unmuting so nothing is heard
    stop.store(true, Ordering::Relaxed);
    render.join().ok();
    vol.set_level(original)?;
    vol.set_mute(original_mute)?;
    println!("restored volume to {:.0}%, mute {original_mute}", original * 100.0);
    Ok(())
}

fn discover(wait: Duration) -> Vec<Device> {
    let found = Arc::new(Mutex::new(Vec::new()));
    let f = found.clone();
    let _mdns = discovery::start(move |list| *f.lock() = list).expect("mdns");
    std::thread::sleep(wait);
    let list = found.lock().clone();
    list
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("list");
    let flag = |name: &str| args.iter().any(|a| a == name);
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u64>().ok())
    };

    if cmd == "outputs" {
        for o in ho_my_pod::capture::list_output_devices()? {
            let v = ho_my_pod::winaudio::EndpointVolume::open(Some(&o.id))?;
            println!(
                "{} {:<42} vol {:>3.0}% ({:>6.1} dB) {}",
                if o.is_default { "*" } else { " " },
                o.name,
                v.level()? * 100.0,
                v.level_db()?,
                if v.hardware_volume() { "hardware" } else { "software" }
            );
        }
        return Ok(());
    }

    if cmd == "media" {
        // Prints what Windows reports as playing, for a few seconds
        let _h = ho_my_pod::media::start(|np| match np {
            Some(mut np) => {
                let art = np.artwork.take().map(|a| format!("{} bytes", a.len()));
                println!("{np:?}\n  artwork: {art:?}");
            }
            None => println!("(nothing playing)"),
        });
        std::thread::sleep(Duration::from_secs(4));
        return Ok(());
    }

    if cmd == "remote" {
        // Announces a DACP server and prints the commands it receives
        let mdns = mdns_sd::ServiceDaemon::new()?;
        let me = SenderIdentity::random("hmp-cli");
        ho_my_pod::remote::start(&mdns, &me.dacp_id, me.active_remote, |ip, cmd| {
            println!("command from {ip}: {cmd:?}")
        })?;
        println!("DACP-ID {} Active-Remote {}", me.dacp_id, me.active_remote);
        std::thread::sleep(Duration::from_secs(opt("--secs").unwrap_or(30)));
        return Ok(());
    }

    if cmd == "mute-test" {
        // Capture with "mute the PC" on, then stop: the PC must end up as before
        let vol = ho_my_pod::winaudio::EndpointVolume::open(None)?;
        println!("before:          muted={}", vol.muted()?);
        let cap = Capture::start(None, Arc::new(Ring::new()), CaptureFlags::new(true, true));
        std::thread::sleep(Duration::from_millis(1500));
        println!("while capturing: muted={}", vol.muted()?);
        drop(cap);
        println!("after:           muted={}", vol.muted()?);
        return Ok(());
    }

    if cmd == "loopback-test" {
        return loopback_test(
            args.get(2).map(String::as_str).unwrap_or("Steam"),
            !flag("--raw"),
            flag("--silent"),
        );
    }

    let devices = discover(Duration::from_secs(3));
    match cmd {
        "list" => {
            for d in &devices {
                println!("{:<24} {:<26} {:<18} {}:{}", d.name, d.model_name, d.model, d.ip, d.port);
            }
        }
        "play" => {
            let wanted: Vec<String> = args
                .get(2)
                .map(|s| s.split(',').map(|x| x.to_lowercase()).collect())
                .unwrap_or_default();
            let targets: Vec<&Device> = devices
                .iter()
                .filter(|d| wanted.iter().any(|w| d.name.to_lowercase() == *w || d.id.to_lowercase() == *w))
                .collect();
            if targets.is_empty() {
                anyhow::bail!("no device matches {wanted:?}");
            }
            let secs = opt("--secs").unwrap_or(15);
            let lead_ms = opt("--lead-ms").unwrap_or(250) as u32;
            let latency_min = opt("--latency-min").unwrap_or(11025) as u32;

            let ring = Arc::new(Ring::new());
            let _capture;
            let source: Arc<dyn AudioSource> = if flag("--tone") {
                Arc::new(Tone { phase: Mutex::new(0.0) })
            } else {
                _capture = Capture::start(None, ring.clone(), CaptureFlags::new(true, flag("--mute-pc")));
                ring
            };
            let streamer = Streamer::new(source, lead_ms * SAMPLE_RATE / 1000)?;
            let me = SenderIdentity::random("Ho My Pod");
            let params = SessionParams {
                timing_port: streamer.timing_port(),
                control_port: streamer.control_port(),
                latency_min_frames: latency_min,
                volume: Some(opt("--volume").unwrap_or(35) as u8),
            };
            let mut sessions = Vec::new();
            for d in targets {
                let s = Session::connect(SocketAddr::new(d.ip, d.port), &d.name, &me, &params)?;
                streamer.add_target(&d.id, &s.target)?;
                sessions.push(s);
            }
            println!("streaming for {secs}s...");
            for _ in 0..secs / 2 {
                std::thread::sleep(Duration::from_secs(2));
                for s in sessions.iter_mut() {
                    if let Err(e) = s.keep_alive() {
                        eprintln!("keep-alive failed: {e:#}");
                    }
                }
            }
            for s in sessions {
                s.teardown();
            }
        }
        "vol-probe" => {
            // Silent stream; reads back what the receiver does with each value
            let name = args.get(2).map(|s| s.to_lowercase()).unwrap_or_default();
            let d = devices
                .iter()
                .find(|d| d.name.to_lowercase() == name)
                .ok_or_else(|| anyhow::anyhow!("no device '{name}'"))?;
            let silence: Arc<dyn AudioSource> = Arc::new(Ring::new());
            let streamer = Streamer::new(silence, 300 * SAMPLE_RATE / 1000)?;
            let me = SenderIdentity::random("Ho My Pod");
            let params = SessionParams {
                timing_port: streamer.timing_port(),
                control_port: streamer.control_port(),
                latency_min_frames: 11025,
                volume: None,
            };
            let mut s = Session::connect(SocketAddr::new(d.ip, d.port), &d.name, &me, &params)?;
            streamer.add_target(&d.id, &s.target)?;
            std::thread::sleep(Duration::from_millis(800));
            let original = s.get_volume()?;
            println!("original: {original}");
            for v in ["-15.000000", "-0.300000", "0.000000", "-0.000000", "-15.000000", "0.000000"] {
                s.set_volume_raw(v)?;
                std::thread::sleep(Duration::from_millis(700));
                println!("set {v:>11} -> device reports {}", s.get_volume()?);
            }
            s.set_volume_raw(&format!("{original:.6}"))?;
            std::thread::sleep(Duration::from_millis(500));
            println!("restored -> {}", s.get_volume()?);
            s.teardown();
        }
        _ => eprintln!("usage: hmp-cli list | outputs | play <name>[,<name>] [--tone] [--secs N] | vol-probe <name>"),
    }
    Ok(())
}
