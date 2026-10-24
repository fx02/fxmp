//! Start at login (a LaunchAgent on macOS), kept in line with the `autostart` setting.

use tauri::AppHandle;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// Passed by the login item, so a start at login keeps the window hidden.
pub const LAUNCH_ARG: &str = "--autostart";

pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![LAUNCH_ARG]))
}

/// Whether these arguments are a start at login.
pub fn is_autostart_launch<S: AsRef<str>>(args: impl IntoIterator<Item = S>) -> bool {
    args.into_iter().any(|a| a.as_ref() == LAUNCH_ARG)
}

/// Turns start at login on or off to match `enabled`, if it does not already.
pub fn apply(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    let current = launcher.is_enabled().map_err(|e| e.to_string())?;
    if current == enabled {
        return Ok(());
    }
    let result = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    result.map_err(|e| e.to_string())?;
    log::info!(
        "start at login {}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_launch_is_recognised_by_its_argument() {
        assert!(is_autostart_launch(["/Applications/x", "--autostart"]));
        assert!(!is_autostart_launch(["/Applications/x"]));
        assert!(!is_autostart_launch(["/Applications/x", "--autostart=no"]));
    }
}
