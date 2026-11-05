//! System media integration: media keys and Now Playing (souvlaki), and the
//! "new track" notification.

use std::cell::RefCell;
use std::sync::Mutex;

use fxmp_core::audio::player::Command;
use fxmp_core::TrackId;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::commands::{next_now, pause_now, play_pause_now, previous_now, spawn_action};
use crate::state::{self, lock, AppState, NowPlaying};
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

/// The window Windows ties the media keys and its media overlay to (the main
/// window, also while hidden). Other systems need none.
#[cfg(windows)]
fn media_hwnd(app: &AppHandle) -> Result<Option<*mut std::ffi::c_void>, String> {
    let window = app
        .get_webview_window("main")
        .ok_or("no main window")?;
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    Ok(Some(hwnd.0 as *mut std::ffi::c_void))
}

#[cfg(not(windows))]
fn media_hwnd(_app: &AppHandle) -> Result<Option<*mut std::ffi::c_void>, String> {
    Ok(None)
}

fn create_controls(app: &AppHandle) -> Result<MediaControls, String> {
    let mut controls = MediaControls::new(PlatformConfig {
        display_name: "fxmp",
        dbus_name: "hr.pvt.fxmp",
        hwnd: media_hwnd(app)?,
    })
    .map_err(|e| format!("{e:?}"))?;
    let handle = app.clone();
    // Called by the system on the main thread: only hands the command over.
    controls
        .attach(move |event| on_media_event(&handle, event))
        .map_err(|e| format!("{e:?}"))?;
    Ok(controls)
}

fn on_media_event(app: &AppHandle, event: MediaControlEvent) {
    log::debug!("media key: {event:?}");
    match event {
        MediaControlEvent::Toggle => spawn_action(app, "play/pause", |app, st| {
            play_pause_now(app, st, Command::TogglePause);
            Ok(())
        }),
        MediaControlEvent::Play => spawn_action(app, "play", |app, st| {
            play_pause_now(app, st, Command::Resume);
            Ok(())
        }),
        MediaControlEvent::Pause | MediaControlEvent::Stop => {
            spawn_action(app, "pause", |_, st| {
                pause_now(st);
                Ok(())
            })
        }
        MediaControlEvent::Next => spawn_action(app, "next", |app, st| {
            next_now(app, st);
            Ok(())
        }),
        MediaControlEvent::Previous => spawn_action(app, "previous", |_, st| previous_now(st)),
        MediaControlEvent::Raise => show_main_window(app),
        _ => {}
    }
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
    // Title and year only: no image, icon or attachment is set.
    let mut builder = app.notification().builder().title(title);
    if !body.is_empty() {
        builder = builder.body(body);
    }
    if let Err(e) = builder.show() {
        log::warn!("notification: {e}");
    }
}

/// Notification title and body: "Artist – Title", and the year (or nothing). The
/// notification carries no image; macOS still shows the app icon.
pub fn notification_text(np: &NowPlaying) -> (String, String) {
    let title = [np.artist.trim(), np.title.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" – ");
    (title, np.year.map(|y| y.to_string()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(artist: &str, title: &str, year: Option<u16>) -> NowPlaying {
        NowPlaying {
            duration_ms: None,
            id: 1,
            artist: artist.into(),
            title: title.into(),
            album: "Album".into(),
            year,
            genre_display: String::new(),
            paused: false,
        }
    }

    #[test]
    fn notification_is_artist_and_title_with_the_year_as_body() {
        let (title, body) = notification_text(&np("Artist", "Song", Some(1991)));
        assert_eq!(title, "Artist – Song");
        assert_eq!(body, "1991");
    }

    #[test]
    fn notification_leaves_out_missing_parts() {
        assert_eq!(
            notification_text(&np("Artist", "Song", None)),
            ("Artist – Song".to_string(), String::new())
        );
        assert_eq!(notification_text(&np(" ", "Song", None)).0, "Song");
    }
}
