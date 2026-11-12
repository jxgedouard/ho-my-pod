//! Remote control from the speakers (DACP).
//!
//! When a HomePod is asked to pause, skip, etc. (Siri, a tap on its top, the
//! iPhone's Control Center) while it plays our stream, it forwards the
//! command to the sender: it looks up the `_dacp._tcp` service named after
//! the `DACP-ID` header we send in every RTSP request, and makes plain HTTP
//! requests to it, e.g. `GET /ctrl-int/1/playpause` with our `Active-Remote`
//! token. This module is that small HTTP server and its mDNS announcement.
//! HomePods actually use the AirPlay 2 events channel instead (see
//! `from_media_remote`); DACP stays for older receivers.

use anyhow::Result;
use log::{debug, info, warn};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

const SERVICE: &str = "_dacp._tcp.local.";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteCmd {
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// The speaker reports its own new volume, in AirPlay dB (-30..0, -144)
    DeviceVolume(f32),
}

/// Parses the path of a DACP request (`/ctrl-int/1/<command>[?query]`).
pub fn parse(path: &str) -> Option<RemoteCmd> {
    let rest = path.strip_prefix("/ctrl-int/1/")?;
    let (cmd, query) = rest.split_once('?').unwrap_or((rest, ""));
    Some(match cmd {
        "playpause" => RemoteCmd::PlayPause,
        "play" | "playresume" => RemoteCmd::Play,
        "pause" => RemoteCmd::Pause,
        "stop" => RemoteCmd::Stop,
        "nextitem" | "beginff" => RemoteCmd::Next,
        "previtem" | "beginrew" => RemoteCmd::Previous,
        "setproperty" => {
            let db = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "dmcp.device-volume")?
                .1
                .parse()
                .ok()?;
            RemoteCmd::DeviceVolume(db)
        }
        _ => return None,
    })
}

/// Parses a command that HomePods send on the AirPlay 2 events channel
/// (`POST /command`, a plist with `type: sendMediaRemoteCommand`). This is
/// what they use for Siri and touch; DACP is for older receivers.
pub fn from_media_remote(msg: &plist::Value) -> Option<RemoteCmd> {
    let d = msg.as_dictionary()?;
    if d.get("type")?.as_string()? != "sendMediaRemoteCommand" {
        return None;
    }
    // MRMediaRemoteCommand numbers, sent as a string
    let modern = d
        .get("modernMediaRemoteCommand")
        .and_then(|v| v.as_string().map(str::to_string).or_else(|| v.as_signed_integer().map(|i| i.to_string())));
    let by_number = modern.as_deref().and_then(|n| match n {
        "0" => Some(RemoteCmd::Play),
        "1" => Some(RemoteCmd::Pause),
        "2" => Some(RemoteCmd::PlayPause),
        "3" => Some(RemoteCmd::Stop),
        "4" => Some(RemoteCmd::Next),
        "5" => Some(RemoteCmd::Previous),
        _ => None,
    });
    // Older four-letter codes, as a fallback
    by_number.or_else(|| match d.get("value")?.as_string()? {
        "play" => Some(RemoteCmd::Play),
        "paus" => Some(RemoteCmd::Pause),
        "plps" | "pltg" => Some(RemoteCmd::PlayPause),
        "stop" => Some(RemoteCmd::Stop),
        "nitm" => Some(RemoteCmd::Next),
        "pitm" => Some(RemoteCmd::Previous),
        _ => None,
    })
}

type Handler = Box<dyn Fn(IpAddr, RemoteCmd) + Send + Sync>;
static HANDLER: std::sync::OnceLock<Handler> = std::sync::OnceLock::new();

/// Delivers a command received from a speaker (by any route).
pub fn dispatch(from: IpAddr, cmd: RemoteCmd) {
    info!("remote: {from} -> {cmd:?}");
    if let Some(h) = HANDLER.get() {
        h(from, cmd);
    }
}

/// Starts the DACP server and announces it, and routes every speaker
/// command to `on_cmd`, with the address of the speaker that sent it.
pub fn start(
    mdns: &ServiceDaemon,
    dacp_id: &str,
    active_remote: u32,
    on_cmd: impl Fn(IpAddr, RemoteCmd) + Send + Sync + 'static,
) -> Result<()> {
    let _ = HANDLER.set(Box::new(on_cmd));
    let listener = TcpListener::bind(("0.0.0.0", 0))?;
    let port = listener.local_addr()?.port();

    let host = format!("ho-my-pod-{}.local.", &dacp_id[..8].to_lowercase());
    let props = [
        ("txtvers", "1"),
        ("Ver", "131073"),
        ("DbId", dacp_id),
        ("OSsi", "0x1F5"),
    ];
    let info = ServiceInfo::new(
        SERVICE,
        &format!("iTunes_Ctrl_{dacp_id}"),
        &host,
        "",
        port,
        &props[..],
    )?
    .enable_addr_auto();
    mdns.register(info)?;
    info!("remote: DACP server on port {port}");

    thread::Builder::new()
        .name("dacp".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                thread::spawn(move || {
                    if let Err(e) = serve(conn, active_remote) {
                        debug!("remote: connection ended: {e}");
                    }
                });
            }
        })?;
    Ok(())
}

/// Handles the requests of one connection (speakers keep it alive).
fn serve(mut conn: TcpStream, active_remote: u32) -> Result<()> {
    let peer = conn.peer_addr()?.ip();
    conn.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        while let Some(end) = crate::airplay::rtsp::find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let body_len = header(&head, "Content-Length")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() < end + 4 + body_len {
                break;
            }
            buf.drain(..end + 4 + body_len);

            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let token_ok = header(&head, "Active-Remote")
                .and_then(|v| v.parse::<u32>().ok())
                == Some(active_remote);
            let status = if !token_ok {
                warn!("remote: {peer} sent {path} with a wrong Active-Remote");
                "403 Forbidden"
            } else if let Some(cmd) = parse(&path) {
                dispatch(peer, cmd);
                "204 No Content"
            } else {
                info!("remote: {peer} sent unsupported {path}");
                "204 No Content"
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nDAAP-Server: iTunes/12.12 (Windows; N;)\r\n\
                 Content-Type: application/x-dmap-tagged\r\nContent-Length: 0\r\n\r\n"
            );
            conn.write_all(reply.as_bytes())?;
        }
    }
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse("/ctrl-int/1/playpause"), Some(RemoteCmd::PlayPause));
        assert_eq!(parse("/ctrl-int/1/nextitem"), Some(RemoteCmd::Next));
        assert_eq!(parse("/ctrl-int/1/previtem"), Some(RemoteCmd::Previous));
        assert_eq!(
            parse("/ctrl-int/1/setproperty?dmcp.device-volume=-12.500000"),
            Some(RemoteCmd::DeviceVolume(-12.5))
        );
        assert_eq!(parse("/ctrl-int/1/shuffle_songs"), None);
        assert_eq!(parse("/other"), None);
    }

    fn mr(modern: &str, value: &str) -> plist::Value {
        let mut d = plist::Dictionary::new();
        d.insert("type".into(), "sendMediaRemoteCommand".into());
        d.insert("modernMediaRemoteCommand".into(), modern.into());
        d.insert("value".into(), value.into());
        plist::Value::Dictionary(d)
    }

    #[test]
    fn parses_media_remote_commands() {
        // As logged from a HomePod mini: "Dis Siri, pause" / "chanson suivante"
        assert_eq!(from_media_remote(&mr("1", "paus")), Some(RemoteCmd::Pause));
        assert_eq!(from_media_remote(&mr("4", "nitm")), Some(RemoteCmd::Next));
        assert_eq!(from_media_remote(&mr("", "pitm")), Some(RemoteCmd::Previous));
        assert_eq!(from_media_remote(&mr("99", "zzzz")), None);
    }
}
