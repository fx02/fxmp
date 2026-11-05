//! Portable mode: a folder named [`DIR_NAME`] beside the app (beside `fxmp.exe` on
//! Windows, beside `fxmp.app` on macOS) keeps the settings, the catalog and the
//! log, so a copy on a USB stick or in any folder carries its own data. Passwords
//! stay in the computer's keychain: each computer asks for them once.

use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = "fxmp-data";

/// The folder the app is in, as a user sees it: the folder holding `fxmp.app`
/// for an executable inside a macOS bundle, else the executable's folder.
pub fn app_folder(exe: &Path) -> Option<&Path> {
    let dir = exe.parent()?;
    let bundle = dir
        .parent()
        .filter(|contents| dir.ends_with("Contents/MacOS") && contents.ends_with("Contents"))
        .and_then(Path::parent)
        .filter(|app| app.extension().is_some_and(|e| e == "app"));
    match bundle {
        Some(app) => app.parent(),
        None => Some(dir),
    }
}

/// The portable data folder for the executable `exe`, if there is one.
pub fn data_dir_for(exe: &Path) -> Option<PathBuf> {
    Some(app_folder(exe)?.join(DIR_NAME)).filter(|d| d.is_dir())
}

/// This process's portable data folder (looked up once).
pub fn current() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| data_dir_for(&exe))
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_folder_is_beside_the_exe_or_the_bundle() {
        assert_eq!(
            app_folder(Path::new("/Volumes/USB/fxmp.app/Contents/MacOS/fxmp")),
            Some(Path::new("/Volumes/USB"))
        );
        assert_eq!(
            app_folder(Path::new("/Volumes/USB/fxmp/fxmp.exe")),
            Some(Path::new("/Volumes/USB/fxmp"))
        );
        // Not a bundle: a folder that only looks like one part of it.
        assert_eq!(
            app_folder(Path::new("/x/Contents/MacOS/fxmp")),
            Some(Path::new("/x/Contents/MacOS"))
        );
    }

    #[test]
    fn portable_only_with_the_folder_there() {
        let root = std::env::temp_dir().join(format!("fxmp-portable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("fxmp.exe");
        assert_eq!(data_dir_for(&exe), None);
        // A file of that name is not the folder.
        std::fs::write(root.join(DIR_NAME), "").unwrap();
        assert_eq!(data_dir_for(&exe), None);
        std::fs::remove_file(root.join(DIR_NAME)).unwrap();
        std::fs::create_dir(root.join(DIR_NAME)).unwrap();
        assert_eq!(data_dir_for(&exe), Some(root.join(DIR_NAME)));
        let bundle_exe = root.join("fxmp.app/Contents/MacOS/fxmp");
        assert_eq!(data_dir_for(&bundle_exe), Some(root.join(DIR_NAME)));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
