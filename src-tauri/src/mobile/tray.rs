//! No tray on Android: the playback notification (see `media.rs`) has the
//! controls, and the app's window is always the one on screen.

use tauri::{AppHandle, Manager};

pub fn init(_app: &AppHandle) -> tauri::Result<()> {
    Ok(())
}

pub fn refresh_audio(_app: &AppHandle) {}

pub fn refresh_genres(_app: &AppHandle) {}

pub fn relabel(_app: &AppHandle) {}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
    }
}
