#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ho_my_pod::engine::{Engine, LatencyPreset, Snapshot};
use ho_my_pod::firewall;
use ho_my_pod::media::{self, MediaCmd, MediaHandle, NowPlaying};
use ho_my_pod::remote::RemoteCmd;
use parking_lot::Mutex;
use std::io::Write;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

#[tauri::command]
fn get_state(engine: State<Engine>) -> Snapshot {
    engine.snapshot()
}

#[tauri::command]
fn connect(id: String, engine: State<Engine>) {
    engine.connect(&id);
}

#[tauri::command]
fn disconnect(id: String, engine: State<Engine>) {
    engine.disconnect(&id);
}

#[tauri::command]
fn set_volume(id: String, volume: u8, engine: State<Engine>) {
    engine.set_volume(&id, volume);
}

#[tauri::command]
fn set_latency(preset: LatencyPreset, engine: State<Engine>) {
    engine.set_latency(preset);
}

#[tauri::command]
fn set_output(id: Option<String>, engine: State<Engine>) {
    engine.set_output_device(id);
}

#[tauri::command]
fn refresh_outputs(engine: State<Engine>) {
    engine.refresh_outputs();
}

#[tauri::command]
fn set_independent_volume(on: bool, engine: State<Engine>) {
    engine.set_independent_volume(on);
}

#[tauri::command]
fn set_mute_pc(on: bool, engine: State<Engine>) {
    engine.set_mute_pc(on);
}

struct Media {
    handle: MediaHandle,
    last: Arc<Mutex<Option<NowPlaying>>>,
}

#[tauri::command]
fn get_media(media: State<Media>) -> Option<NowPlaying> {
    media.last.lock().clone()
}

#[tauri::command]
fn media_cmd(action: String, position_ms: Option<u64>, media: State<Media>) {
    let cmd = match action.as_str() {
        "playpause" => MediaCmd::PlayPause,
        "next" => MediaCmd::Next,
        "previous" => MediaCmd::Previous,
        "seek" => MediaCmd::Seek(position_ms.unwrap_or(0)),
        _ => return,
    };
    media.handle.send(cmd);
}

#[tauri::command]
fn level(engine: State<Engine>) -> f32 {
    engine.level()
}

#[tauri::command]
async fn firewall_status() -> bool {
    tauri::async_runtime::spawn_blocking(firewall::is_allowed)
        .await
        .unwrap_or(false)
}

#[tauri::command]
async fn firewall_allow() -> bool {
    tauri::async_runtime::spawn_blocking(firewall::request_allow)
        .await
        .unwrap_or(false)
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn init_logging() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,mdns_sd=warn"));
    // Release builds have no console: log to %APPDATA%\HoMyPod\log.txt
    if !cfg!(debug_assertions) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let dir = std::path::PathBuf::from(appdata).join("HoMyPod");
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(file) = std::fs::File::create(dir.join("log.txt")) {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
        }
    }
    builder.format(|buf, record| {
        writeln!(
            buf,
            "{} {:<5} {}",
            buf.timestamp_millis(),
            record.level(),
            record.args()
        )
    });
    builder.init();
}

fn main() {
    init_logging();

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let engine = Engine::new(move |snapshot| {
                let _ = handle.emit("state", snapshot);
            })?;
            app.manage(engine);

            let last = Arc::new(Mutex::new(None));
            let handle = app.handle().clone();
            let last_cb = last.clone();
            let media_handle = media::start(move |np| {
                *last_cb.lock() = np.clone();
                let _ = handle.emit("media", np);
            });
            // Siri / touch / Control Center on the speakers drive the player
            let remote_media = media_handle.clone();
            let started = app.state::<Engine>().start_remote(move |cmd| {
                let cmd = match cmd {
                    RemoteCmd::PlayPause => MediaCmd::PlayPause,
                    RemoteCmd::Play => MediaCmd::Play,
                    RemoteCmd::Pause => MediaCmd::Pause,
                    RemoteCmd::Stop => MediaCmd::Stop,
                    RemoteCmd::Next => MediaCmd::Next,
                    RemoteCmd::Previous => MediaCmd::Previous,
                    RemoteCmd::DeviceVolume(_) => return,
                };
                remote_media.send(cmd);
            });
            if let Err(e) = started {
                log::warn!("remote control unavailable: {e:#}");
            }
            app.manage(Media {
                handle: media_handle,
                last,
            });

            let show = MenuItem::with_id(app, "show", "Afficher Ho My Pod", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("tray")
                .icon(app.default_window_icon().cloned().expect("app icon"))
                .tooltip("Ho My Pod")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        app.state::<Engine>().shutdown();
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the audio going; quit from the tray
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            connect,
            disconnect,
            set_volume,
            set_latency,
            set_output,
            refresh_outputs,
            level,
            set_independent_volume,
            set_mute_pc,
            get_media,
            media_cmd,
            firewall_status,
            firewall_allow
        ])
        .run(tauri::generate_context!())
        .expect("error while running Ho My Pod");
}
