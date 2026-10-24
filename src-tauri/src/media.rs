//! System media integration: media keys and Now Playing (souvlaki), and the
//! "new track" notification.

use std::cell::RefCell;
use std::sync::Mutex;

#[cfg(target_os = "macos")]
use smp_core::audio::player::Command;
use smp_core::TrackId;
#[cfg(target_os = "macos")]
use souvlaki::{MediaControlEvent, PlatformConfig};
use souvlaki::{MediaControls, MediaMetadata, MediaPlayback};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::{self, lock, AppState, NowPlaying};
#[cfg(target_os = "macos")]
use crate::tray::show_main_window;

thread_local! {
    /// Lives on the main thread: Now Playing is only ever updated from there.
    static CONTROLS: RefCell<Option<MediaControls>> = const { RefCell::new(None) };
}

/// Registers the media keys / Now Playing handlers and the track notification.
/// Must run on the main thread (in `setup`).
pub fn init(app: &AppHandle) {
    match create_controls(app) {
        Ok(controls) => CONTROLS.with(|c| *c.borrow_mut() = Some(controls)),
        Err(e) => log::warn!("media keys unavailable: {e}"),
    }
    let handle = app.clone();
    let last_track = Mutex::new(None::<TrackId>);
    state::on_now_playing(app, move |np| {
        update_now_playing(&handle, np.clone());
        // `now-playing` also reports pause changes: notify only for a new track.
        let id = np.as_ref().map(|n| n.id);
        let new_track = std::mem::replace(&mut *lock(&last_track), id) != id;
        if let Some(np) = np.filter(|_| new_track) {
            notify_track(&handle, &np);
        }
    });
}

/// Windows needs an HWND for media controls, which is not wired up yet (the
/// Windows pass comes later), so media keys are macOS-only for now.
#[cfg(not(target_os = "macos"))]
fn create_controls(_app: &AppHandle) -> Result<MediaControls, String> {
    Err("not supported on this platform yet".into())
}

#[cfg(target_os = "macos")]
fn create_controls(app: &AppHandle) -> Result<MediaControls, String> {
    let mut controls = MediaControls::new(PlatformConfig {
        display_name: "SMB Music Player",
        dbus_name: "hr.pvt.smb_music_player",
        hwnd: None,
    })
    .map_err(|e| format!("{e:?}"))?;
    let handle = app.clone();
    // Called by the system on the main thread: only hands the command over.
    controls
        .attach(move |event| on_media_event(&handle, event))
        .map_err(|e| format!("{e:?}"))?;
    Ok(controls)
}

#[cfg(target_os = "macos")]
fn on_media_event(app: &AppHandle, event: MediaControlEvent) {
    log::debug!("media key: {event:?}");
    let command = match event {
        MediaControlEvent::Toggle => Command::TogglePause,
        MediaControlEvent::Play => Command::Resume,
        MediaControlEvent::Pause | MediaControlEvent::Stop => Command::Pause,
        MediaControlEvent::Next => Command::Next,
        MediaControlEvent::Raise => return show_main_window(app),
        _ => return,
    };
    app.state::<AppState>().player.send(command);
}

fn update_now_playing(app: &AppHandle, np: Option<NowPlaying>) {
    let result = app.run_on_main_thread(move || {
        CONTROLS.with(|c| {
            let mut c = c.borrow_mut();
            let Some(controls) = c.as_mut() else {
                return;
            };
            let (metadata, playback) = match &np {
                Some(n) => (
                    MediaMetadata {
                        title: Some(&n.title),
                        artist: Some(n.artist.as_str()).filter(|a| !a.is_empty()),
                        album: Some(n.album.as_str()).filter(|a| !a.is_empty()),
                        ..Default::default()
                    },
                    if n.paused {
                        MediaPlayback::Paused { progress: None }
                    } else {
                        MediaPlayback::Playing { progress: None }
                    },
                ),
                None => (MediaMetadata::default(), MediaPlayback::Stopped),
            };
            if let Err(e) = controls
                .set_metadata(metadata)
                .and_then(|_| controls.set_playback(playback))
            {
                log::warn!("now playing: {e:?}");
            }
        })
    });
    if let Err(e) = result {
        log::warn!("now playing: {e}");
    }
}

/// Shows the notification if the `notify` setting is on (read each time).
fn notify_track(app: &AppHandle, np: &NowPlaying) {
    if !app.state::<AppState>().settings.get().notify {
        return;
    }
    let (title, body) = notification_text(np);
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        log::warn!("notification: {e}");
    }
}

/// Notification title and body: the song title, and "Izvođač — Album".
pub fn notification_text(np: &NowPlaying) -> (String, String) {
    let body = [np.artist.trim(), np.album.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" — ");
    (np.title.clone(), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(artist: &str, title: &str, album: &str) -> NowPlaying {
        NowPlaying {
            id: 1,
            artist: artist.into(),
            title: title.into(),
            album: album.into(),
            genre_display: String::new(),
            paused: false,
        }
    }

    #[test]
    fn notification_shows_title_then_artist_and_album() {
        let (title, body) = notification_text(&np("Artist", "Song", "Album"));
        assert_eq!(title, "Song");
        assert_eq!(body, "Artist — Album");
    }

    #[test]
    fn notification_body_leaves_out_missing_parts() {
        assert_eq!(notification_text(&np("Artist", "S", "")).1, "Artist");
        assert_eq!(notification_text(&np(" ", "S", "Album")).1, "Album");
        assert_eq!(notification_text(&np("", "S", "")).1, "");
    }
}
