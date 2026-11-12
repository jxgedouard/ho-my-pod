//! "Now playing" from Windows' media sessions (the API behind the media keys
//! and the volume flyout): Spotify, browsers, the Media Player app...
//! Gives title/artist/artwork/progress and play-pause/next/previous/seek.

use anyhow::Result;
use base64::Engine as _;
use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};
use log::{debug, info, warn};
use serde::Serialize;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::core::Interface;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IInputStream};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NowPlaying {
    pub app: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    pub can_play_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    /// Position at `sampled_at_ms` (Unix ms)
    pub position_ms: u64,
    pub duration_ms: u64,
    pub sampled_at_ms: u64,
    /// data: URL
    pub artwork: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum MediaCmd {
    PlayPause,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    Seek(u64),
}

#[derive(Clone)]
pub struct MediaHandle {
    tx: Sender<MediaCmd>,
}

impl MediaHandle {
    pub fn send(&self, cmd: MediaCmd) {
        let _ = self.tx.send(cmd);
    }
}

/// Polls the current media session and calls `on_change` whenever what is
/// playing changes (track, play state, a seek...). `None` = nothing playing.
pub fn start(on_change: impl Fn(Option<NowPlaying>) + Send + 'static) -> MediaHandle {
    let (tx, rx) = unbounded::<MediaCmd>();
    thread::Builder::new()
        .name("media-sessions".into())
        .spawn(move || {
            let manager = loop {
                match Manager::RequestAsync().and_then(|op| op.get()) {
                    Ok(m) => break m,
                    Err(e) => {
                        warn!("media: session manager unavailable: {e}");
                        thread::sleep(Duration::from_secs(5));
                    }
                }
            };
            info!("media: watching Windows media sessions");
            let mut last: Option<NowPlaying> = None;
            let mut art_key = String::new();
            let mut art: Option<String> = None;
            loop {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(cmd) => {
                        if let Ok(s) = manager.GetCurrentSession() {
                            if let Err(e) = run_cmd(&s, cmd) {
                                debug!("media: {cmd:?} failed: {e}");
                            }
                        }
                        // Let the app apply it before we sample again
                        thread::sleep(Duration::from_millis(150));
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }

                let now = match manager.GetCurrentSession() {
                    Ok(s) => read_session(&s, &mut art_key, &mut art).ok(),
                    Err(_) => None,
                };
                if changed(&last, &now) {
                    on_change(now.clone());
                }
                last = now;
            }
        })
        .expect("spawn media thread");
    MediaHandle { tx }
}

fn run_cmd(s: &Session, cmd: MediaCmd) -> Result<()> {
    match cmd {
        MediaCmd::PlayPause => s.TryTogglePlayPauseAsync()?.get()?,
        MediaCmd::Play => s.TryPlayAsync()?.get()?,
        MediaCmd::Pause => s.TryPauseAsync()?.get()?,
        // Apps rarely implement Stop; pausing is what the user means
        MediaCmd::Stop => s.TryPauseAsync()?.get()?,
        MediaCmd::Next => s.TrySkipNextAsync()?.get()?,
        MediaCmd::Previous => s.TrySkipPreviousAsync()?.get()?,
        MediaCmd::Seek(ms) => s.TryChangePlaybackPositionAsync(ms as i64 * 10_000)?.get()?,
    };
    Ok(())
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Windows DateTime (100 ns ticks since 1601) to Unix ms.
fn filetime_to_unix_ms(ticks: i64) -> u64 {
    ((ticks - 116_444_736_000_000_000).max(0) / 10_000) as u64
}

fn app_name(aumid: &str) -> String {
    let lower = aumid.to_lowercase();
    let known = [
        ("spotify", "Spotify"),
        ("msedge", "Microsoft Edge"),
        ("chrome", "Google Chrome"),
        ("firefox", "Firefox"),
        ("308046b0af4a39cb", "Firefox"),
        ("zunemusic", "Lecteur multimédia"),
        ("deezer", "Deezer"),
        ("tidal", "TIDAL"),
        ("applemusic", "Apple Music"),
        ("vlc", "VLC"),
        ("brave", "Brave"),
        ("opera", "Opera"),
        ("discord", "Discord"),
    ];
    if let Some((_, name)) = known.iter().find(|(k, _)| lower.contains(k)) {
        return name.to_string();
    }
    // "Publisher.App_hash!Id" or "app.exe"
    let base = aumid.split('!').next().unwrap_or(aumid);
    let base = base.split('_').next().unwrap_or(base);
    let base = base.rsplit('.').find(|p| !p.eq_ignore_ascii_case("exe")).unwrap_or(base);
    base.to_string()
}

fn read_session(s: &Session, art_key: &mut String, art: &mut Option<String>) -> Result<NowPlaying> {
    let props = s.TryGetMediaPropertiesAsync()?.get()?;
    let info = s.GetPlaybackInfo()?;
    let controls = info.Controls()?;
    let timeline = s.GetTimelineProperties()?;

    let title = props.Title()?.to_string();
    let artist = props.Artist()?.to_string();
    let album = props.AlbumTitle()?.to_string();
    let playing = info.PlaybackStatus()? == PlaybackStatus::Playing;

    // Artwork only when the track changes
    let key = format!("{title}\u{1}{artist}\u{1}{album}");
    if *art_key != key {
        *art = props.Thumbnail().ok().and_then(|t| read_thumbnail(&t).ok());
        // Some apps publish the artwork a moment after the title: retry later
        if art.is_some() {
            *art_key = key;
        }
    }

    let start = timeline.StartTime()?.Duration;
    let end = timeline.EndTime()?.Duration;
    let pos = timeline.Position()?.Duration;
    let updated = filetime_to_unix_ms(timeline.LastUpdatedTime()?.UniversalTime);
    let now = unix_ms_now();
    let duration_ms = ((end - start).max(0) / 10_000) as u64;
    let mut position_ms = ((pos - start).max(0) / 10_000) as u64;
    if playing && updated > 0 && now > updated {
        position_ms += now - updated;
    }
    if duration_ms > 0 {
        position_ms = position_ms.min(duration_ms);
    }

    Ok(NowPlaying {
        app: app_name(&s.SourceAppUserModelId()?.to_string()),
        title,
        artist,
        album,
        playing,
        can_play_pause: controls.IsPlayPauseToggleEnabled()?
            || controls.IsPlayEnabled()?
            || controls.IsPauseEnabled()?,
        can_next: controls.IsNextEnabled()?,
        can_previous: controls.IsPreviousEnabled()?,
        can_seek: controls.IsPlaybackPositionEnabled()? && duration_ms > 0,
        position_ms,
        duration_ms,
        sampled_at_ms: now,
        artwork: art.clone(),
    })
}

fn read_thumbnail(r: &windows::Storage::Streams::IRandomAccessStreamReference) -> Result<String> {
    let stream = r.OpenReadAsync()?.get()?;
    let size = stream.Size()? as u32;
    if size == 0 || size > 8 * 1024 * 1024 {
        anyhow::bail!("unexpected artwork size {size}");
    }
    let mime = stream.ContentType().map(|s| s.to_string()).unwrap_or_default();
    let input: IInputStream = stream.cast()?;
    let reader = DataReader::CreateDataReader(&input)?;
    reader.LoadAsync(size)?.get()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;
    let mime = if mime.starts_with("image/") { mime } else { "image/png".into() };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&buf)
    ))
}

/// Whether the UI needs an update: anything but the position changed, or the
/// position jumped (seek) compared to where it should be by now.
fn changed(old: &Option<NowPlaying>, new: &Option<NowPlaying>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            let same_meta = a.title == b.title
                && a.artist == b.artist
                && a.album == b.album
                && a.app == b.app
                && a.playing == b.playing
                && a.can_next == b.can_next
                && a.can_previous == b.can_previous
                && a.can_play_pause == b.can_play_pause
                && a.duration_ms == b.duration_ms
                && a.artwork.is_some() == b.artwork.is_some();
            if !same_meta {
                return true;
            }
            let expected = if a.playing {
                a.position_ms + b.sampled_at_ms.saturating_sub(a.sampled_at_ms)
            } else {
                a.position_ms
            };
            (expected as i64 - b.position_ms as i64).abs() > 1500
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(app_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"), "Spotify");
        assert_eq!(app_name("308046B0AF4A39CB"), "Firefox");
        assert_eq!(app_name("foobar2000.exe"), "foobar2000");
    }

    #[test]
    fn filetime_epoch() {
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000 + 10_000_000), 1000);
    }
}
