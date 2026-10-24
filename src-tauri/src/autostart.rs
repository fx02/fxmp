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

/// What to do with the login item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItemAction {
    /// Leave it alone.
    Keep,
    /// (Re)write it for the running executable.
    Enable,
    Disable,
    /// Debug build: never touch it (it would point at `target/debug`); only log.
    SkipDebug,
}

/// `wanted` is the setting, `current` whether the login item exists. At startup an
/// existing item is rewritten, so it points at where the app is now.
pub fn login_item_action(
    debug: bool,
    wanted: bool,
    current: bool,
    at_startup: bool,
) -> LoginItemAction {
    match (debug, wanted, current) {
        (true, ..) => LoginItemAction::SkipDebug,
        (false, true, false) => LoginItemAction::Enable,
        (false, true, true) if at_startup => LoginItemAction::Enable,
        (false, false, true) => LoginItemAction::Disable,
        _ => LoginItemAction::Keep,
    }
}

/// Makes start at login match `enabled`; at startup an enabled item is rewritten so
/// it points at the running app. Debug builds only log.
pub fn apply(app: &AppHandle, enabled: bool, at_startup: bool) -> Result<(), String> {
    let launcher = app.autolaunch();
    let debug = cfg!(debug_assertions);
    // A debug build never reads or writes the login item.
    let current = !debug && launcher.is_enabled().map_err(|e| e.to_string())?;
    let result = match login_item_action(debug, enabled, current, at_startup) {
        LoginItemAction::Keep => return Ok(()),
        LoginItemAction::SkipDebug => {
            log::info!("debug build: start at login left unchanged (setting: {enabled})");
            return Ok(());
        }
        LoginItemAction::Enable => launcher.enable(),
        LoginItemAction::Disable => launcher.disable(),
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
    fn debug_builds_never_touch_the_login_item() {
        for wanted in [false, true] {
            for current in [false, true] {
                for at_startup in [false, true] {
                    assert_eq!(
                        login_item_action(true, wanted, current, at_startup),
                        LoginItemAction::SkipDebug
                    );
                }
            }
        }
    }

    #[test]
    fn release_rewrites_an_enabled_login_item_at_startup() {
        use LoginItemAction::*;
        assert_eq!(login_item_action(false, true, true, true), Enable);
        assert_eq!(login_item_action(false, true, false, true), Enable);
        assert_eq!(login_item_action(false, false, true, true), Disable);
        assert_eq!(login_item_action(false, false, false, true), Keep);
    }

    #[test]
    fn release_save_changes_the_login_item_only_when_needed() {
        use LoginItemAction::*;
        assert_eq!(login_item_action(false, true, true, false), Keep);
        assert_eq!(login_item_action(false, true, false, false), Enable);
        assert_eq!(login_item_action(false, false, true, false), Disable);
        assert_eq!(login_item_action(false, false, false, false), Keep);
    }

    #[test]
    fn autostart_launch_is_recognised_by_its_argument() {
        assert!(is_autostart_launch(["/Applications/x", "--autostart"]));
        assert!(!is_autostart_launch(["/Applications/x"]));
        assert!(!is_autostart_launch(["/Applications/x", "--autostart=no"]));
    }
}
