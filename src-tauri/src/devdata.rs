//! Debug builds only: `SMP_DATA_DIR=/some/folder` keeps settings, catalog and
//! playlist in that folder and passwords under a separate keychain service, so a
//! developer build never reads or writes the installed app's data. Release builds
//! ignore the variable.

#[cfg_attr(not(debug_assertions), allow(unused_imports))]
use std::ffi::OsString;
use std::path::PathBuf;

use crate::credentials;

/// The environment variable naming the data folder.
pub const ENV: &str = "SMP_DATA_DIR";

/// The keychain service used with a data folder override.
pub const DEV_SERVICE: &str = "fxmp-dev";

/// The folder to use instead of the app's own: only in a debug build and only for
/// a non-empty `value`.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub fn data_dir_override(debug: bool, value: Option<OsString>) -> Option<PathBuf> {
    if !debug {
        return None;
    }
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The keychain service: a separate one when the data folder is overridden.
pub fn keychain_service(overridden: bool) -> &'static str {
    if overridden {
        DEV_SERVICE
    } else {
        credentials::SERVICE
    }
}

/// The override of this process (read once). A release build does not even read
/// the variable.
#[cfg(debug_assertions)]
pub fn current() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| data_dir_override(true, std::env::var_os(ENV)))
        .clone()
}

#[cfg(not(debug_assertions))]
pub fn current() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_debug_build_with_a_folder_set_uses_it() {
        let dir = || Some(OsString::from("/tmp/smp-dev"));
        assert_eq!(
            data_dir_override(true, dir()),
            Some(PathBuf::from("/tmp/smp-dev"))
        );
        assert_eq!(data_dir_override(false, dir()), None, "release ignores it");
        assert_eq!(data_dir_override(true, Some(OsString::new())), None);
        assert_eq!(data_dir_override(true, None), None);
    }

    #[test]
    fn an_override_keeps_passwords_apart_from_the_real_ones() {
        assert_eq!(keychain_service(false), credentials::SERVICE);
        assert_eq!(keychain_service(true), "fxmp-dev");
        assert_ne!(keychain_service(true), credentials::SERVICE);
    }
}
