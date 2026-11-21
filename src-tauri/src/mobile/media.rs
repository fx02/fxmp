//! Android playback service: started with the first track, it keeps the app
//! (and the Wi-Fi) awake with the screen off and shows the track with
//! previous / play-pause / next in its notification.

use fxmp_core::audio::player::Command;
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};

use super::native::Native;
use crate::commands::{next_now, play_pause_now, previous_now, spawn_action};
use crate::state::{self, NowPlaying};

#[derive(Serialize)]
struct Playback<'a> {
    /// `None`: nothing playing, the service stops.
    title: Option<&'a str>,
    artist: &'a str,
    playing: bool,
}

#[derive(Serialize)]
struct Controls {
    channel: Channel<serde_json::Value>,
}

#[derive(Deserialize)]
struct Pressed {
    action: String,
}

pub fn init(app: &AppHandle) {
    let plugin = app.state::<Native>().0.clone();
    let handle = app.clone();
    let channel = Channel::new(move |body| {
        if let InvokeResponseBody::Json(json) = body {
            match serde_json::from_str::<Pressed>(&json) {
                Ok(p) => pressed(&handle, &p.action),
                Err(e) => log::warn!("playback control: {e}"),
            }
        }
        Ok(())
    });
    if let Err(e) = plugin.run_mobile_plugin::<()>("setControls", Controls { channel }) {
        log::warn!("playback controls: {e}");
    }
    state::on_now_playing(app, move |np: Option<NowPlaying>| {
        let artist = np.as_ref().map(|n| n.artist.clone()).unwrap_or_default();
        let playback = Playback {
            title: np.as_ref().map(|n| n.title.as_str()),
            artist: &artist,
            playing: np.as_ref().is_some_and(|n| !n.paused),
        };
        if let Err(e) = plugin.run_mobile_plugin::<()>("playback", playback) {
            log::warn!("playback service: {e}");
        }
    });
}

fn pressed(app: &AppHandle, action: &str) {
    match action {
        "toggle" => spawn_action(app, "play/pause", |app, st| {
            play_pause_now(app, st, Command::TogglePause);
            Ok(())
        }),
        "next" => spawn_action(app, "next", |app, st| {
            next_now(app, st);
            Ok(())
        }),
        "previous" => spawn_action(app, "previous", |_, st| previous_now(st)),
        other => log::warn!("unknown playback control {other}"),
    }
}
