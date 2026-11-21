//! No start at login on Android.

use tauri::AppHandle;

pub fn is_autostart_launch<S: AsRef<str>>(_args: impl IntoIterator<Item = S>) -> bool {
    false
}

pub fn apply(_app: &AppHandle, _enabled: bool, _at_startup: bool) -> Result<(), String> {
    Ok(())
}
