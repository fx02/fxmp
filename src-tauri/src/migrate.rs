//! The first start of fxmp after SMB Music Player (its old name): the old app's
//! settings, catalog and playlist are copied from its folder, and its keychain
//! passwords from its keychain service, once. Nothing of the old app is changed
//! or deleted, so it keeps working until the owner removes it.
//!
//! Files: when the new folder has neither settings nor a catalog and the old
//! folder exists, every file of the old folder is copied (`catalog.db-shm`
//! excepted: SQLite rebuilds it from the database and its log). The copy goes to
//! a staging folder first and is checked against the old folder afterwards: if
//! the old app wrote meanwhile, the copy is taken again. A marker file then
//! records that the files came over and that the passwords are still to copy;
//! once they are, the marker says so and nothing is done on later starts.
//!
//! Passwords: each server of every library in the copied settings is read from
//! the old service and written to the new one, then read back. A read the user
//! refuses (macOS asks whether fxmp may read the old app's entry) loses nothing:
//! the app asks for that password, as for any password it does not have.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::credentials::{self, CredentialStore, Remembered};
use crate::settings::{self, LibrarySource, Settings};

/// The old app's bundle identifier: its folder's name.
pub const OLD_IDENTIFIER: &str = "hr.pvt.smb-music-player";
/// The old app's keychain service.
pub const OLD_SERVICE: &str = "smb-music-player";
/// The old app's keychain service of a debug build with `SMP_DATA_DIR`.
pub const OLD_DEV_SERVICE: &str = "smb-music-player-dev";
/// The old app's name, which is also the name of its login item.
pub const OLD_APP_NAME: &str = "SMB Music Player";
/// The marker file in the new folder.
pub const MARKER: &str = "migrated-from-smb-music-player";
/// Debug builds with `SMP_DATA_DIR`: the folder to migrate from (none if unset).
pub const DEV_ENV: &str = "SMP_MIGRATE_FROM";

/// The catalog's file name (see `state.rs`).
const DATABASE: &str = "catalog.db";
const STAGING: &str = ".migrating-from-smb-music-player";
const PASSWORDS_PENDING: &str = "files copied; passwords pending\n";
const DONE: &str = "done\n";
/// Not copied: SQLite rebuilds it, and a copy may not match the log.
const SKIPPED_SUFFIX: &str = "-shm";
/// How often the copy is taken again while the old app writes.
const ATTEMPTS: usize = 5;
const RETRY_DELAY: Duration = Duration::from_millis(300);

/// What [`migrate_files`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Files {
    /// Done on an earlier start.
    AlreadyMigrated,
    /// There is no old folder.
    NoOldData,
    /// The new folder already has settings or a catalog: left alone.
    NewHasData,
    /// This many files were copied now.
    Copied(usize),
}

/// What [`copy_passwords`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Passwords {
    /// Written to the new service and read back the same.
    pub copied: usize,
    /// Already in the new service: left as they are.
    pub present: usize,
    /// Not read from the old service (none stored, or the user refused).
    pub missing: usize,
    /// Not written, or not read back the same.
    pub failed: usize,
}

/// The old app's folder next to `new_dir` (the same parent, the old identifier).
pub fn old_dir_beside(new_dir: &Path) -> Option<PathBuf> {
    new_dir.parent().map(|p| p.join(OLD_IDENTIFIER))
}

/// Copies the old app's files from `old` to `new` unless that was done before,
/// `old` does not exist or `new` already has data. `old` is only read.
pub fn migrate_files(old: &Path, new: &Path) -> io::Result<Files> {
    if new.join(MARKER).exists() {
        return Ok(Files::AlreadyMigrated);
    }
    if !old.is_dir() {
        return Ok(Files::NoOldData);
    }
    if new.join(settings::FILE_NAME).exists() || new.join(DATABASE).exists() {
        return Ok(Files::NewHasData);
    }
    fs::create_dir_all(new)?;
    let staging = new.join(STAGING);
    let mut attempt = 0;
    let copied = loop {
        attempt += 1;
        remove_staging(&staging)?;
        let before = listing(old)?;
        let copied = copy_tree(old, &staging)?;
        if listing(old)? == before {
            break copied;
        }
        if attempt == ATTEMPTS {
            remove_staging(&staging)?;
            return Err(io::Error::other(
                "SMB Music Player kept changing its files while they were copied",
            ));
        }
        log::info!("migration: the old app wrote meanwhile; copying again");
        std::thread::sleep(RETRY_DELAY);
    };
    // The settings and the database last: once either is in place the new
    // folder counts as having data, so the database's log must already be there.
    let mut names: Vec<_> = fs::read_dir(&staging)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort_by_key(|n| n == DATABASE || n == settings::FILE_NAME);
    for name in names {
        let target = new.join(&name);
        if target.exists() {
            log::warn!(
                "migration: {} is already in the new folder; kept",
                name.to_string_lossy()
            );
            continue;
        }
        fs::rename(staging.join(&name), target)?;
    }
    remove_staging(&staging)?;
    fs::write(new.join(MARKER), PASSWORDS_PENDING)?;
    Ok(Files::Copied(copied))
}

/// Whether the files came over but the passwords are still to copy.
pub fn passwords_pending(new: &Path) -> bool {
    fs::read_to_string(new.join(MARKER)).is_ok_and(|m| m == PASSWORDS_PENDING)
}

/// Records that the passwords were copied too: the migration is over.
pub fn mark_done(new: &Path) -> io::Result<()> {
    fs::write(new.join(MARKER), DONE)
}

fn remove_staging(staging: &Path) -> io::Result<()> {
    match fs::remove_dir_all(staging) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether the file is copied (and compared): not links, not `-shm`.
fn wanted(name: &std::ffi::OsStr, meta: &fs::Metadata) -> bool {
    !meta.file_type().is_symlink() && !name.to_string_lossy().ends_with(SKIPPED_SUFFIX)
}

/// Every copied file under `dir` with its size and modification time.
fn listing(dir: &Path) -> io::Result<Vec<(PathBuf, u64, Option<SystemTime>)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if !wanted(&entry.file_name(), &meta) {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                out.push((entry.path(), meta.len(), meta.modified().ok()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Copies the files under `from` to `to`; returns how many.
fn copy_tree(from: &Path, to: &Path) -> io::Result<usize> {
    fs::create_dir_all(to)?;
    let mut copied = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if !wanted(&entry.file_name(), &meta) {
            continue;
        }
        let target = to.join(entry.file_name());
        if meta.is_dir() {
            copied += copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// The keychain accounts of every server of `settings`' libraries, each once,
/// with the first library's accounts from before libraries.
fn accounts(settings: &Settings) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for library in settings.all_libraries() {
        let LibrarySource::Smb { primary, secondary } = &library.source else {
            continue;
        };
        for server in std::iter::once(primary).chain(secondary.as_ref()) {
            if server.username.trim().is_empty() {
                continue;
            }
            let mut found = vec![credentials::account(
                library.id,
                &server.host,
                &server.username,
            )];
            if library.id == fxmp_core::DEFAULT_LIBRARY {
                found.push(credentials::legacy_account(&server.host, &server.username));
            }
            for account in found {
                if !out.contains(&account) {
                    out.push(account);
                }
            }
        }
    }
    out
}

/// Copies the passwords of every server of `settings`' libraries from `old` to
/// `new`, verifying each by reading it back. Old entries are kept; an entry the
/// new service already has is not overwritten. Each old entry is read at most
/// once.
pub fn copy_passwords(
    old: &dyn CredentialStore,
    new: &dyn CredentialStore,
    settings: &Settings,
) -> Passwords {
    let old = Remembered::new(old);
    let mut done = Passwords::default();
    for account in accounts(settings) {
        if new.get(&account).is_some() {
            done.present += 1;
            continue;
        }
        let Some(password) = old.get(&account) else {
            done.missing += 1;
            continue;
        };
        if let Err(e) = new.set(&account, &password) {
            log::warn!("migration: a password was not copied: {e}");
            done.failed += 1;
            continue;
        }
        if new.get(&account).as_deref() == Some(password.as_str()) {
            done.copied += 1;
        } else {
            log::warn!("migration: a copied password does not read back the same");
            done.failed += 1;
        }
    }
    done
}

/// The folder to migrate `new_dir` from: the old app's, next to it; in a debug
/// build with its own data folder (`dev`) only the folder in [`DEV_ENV`], so a
/// developer build never reads the installed app's data.
pub fn old_dir(new_dir: &Path, dev: bool, dev_from: Option<OsString>) -> Option<PathBuf> {
    if dev {
        dev_from.filter(|v| !v.is_empty()).map(PathBuf::from)
    } else {
        old_dir_beside(new_dir)
    }
}

/// The old app's keychain service (see [`old_dir`] for `dev`).
pub fn old_service(dev: bool) -> &'static str {
    if dev {
        OLD_DEV_SERVICE
    } else {
        OLD_SERVICE
    }
}

/// The start's file migration, for the data folder and (if it is another one)
/// the settings folder. Returns whether files were copied now. A failure is
/// logged; the app then starts without the old data and tries again next time.
pub fn migrate_at_start(config_dir: &Path, data_dir: &Path, dev: bool) -> bool {
    let mut dirs = vec![data_dir];
    if config_dir != data_dir {
        dirs.push(config_dir);
    }
    let mut copied_now = false;
    for new in dirs {
        let dev_from = if dev { std::env::var_os(DEV_ENV) } else { None };
        let Some(old) = old_dir(new, dev, dev_from) else {
            continue;
        };
        match migrate_files(&old, new) {
            Ok(Files::Copied(n)) => {
                log::info!(
                    "migration: {n} files of {OLD_APP_NAME} copied from {} to {}",
                    old.display(),
                    new.display()
                );
                copied_now = true;
            }
            Ok(Files::NewHasData) => log::info!(
                "migration: {} already has data; {} left alone",
                new.display(),
                old.display()
            ),
            Ok(Files::AlreadyMigrated | Files::NoOldData) => {}
            Err(e) => log::warn!("migration from {} failed: {e}", old.display()),
        }
    }
    copied_now
}

/// Removes the old app's login item (a LaunchAgent named after it on macOS, a
/// `Run` value on Windows), so it does not start beside fxmp; fxmp's own is set
/// from the copied `autostart` setting. Nothing to do if there is none.
pub fn remove_old_login_item() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut builder = auto_launch::AutoLaunchBuilder::new();
    builder
        .set_app_name(OLD_APP_NAME)
        .set_app_path(&exe.display().to_string())
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);
    builder
        .build()
        .and_then(|launcher| launcher.disable())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use crate::settings::{Library, ServerConfig};
    use fxmp_core::catalog::Catalog;
    use fxmp_core::catalog::TrackMeta;
    use std::collections::BTreeMap;

    /// Every file under `dir`: relative path → (contents, modification time).
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let meta = e.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(e.path());
                } else {
                    out.insert(
                        e.path().strip_prefix(dir).unwrap().to_path_buf(),
                        (fs::read(e.path()).unwrap(), meta.modified().unwrap()),
                    );
                }
            }
        }
        out
    }

    /// An old folder with settings, a catalog with one track (still in its log:
    /// the connection stays open), a renamed bad settings file and a subfolder.
    struct Old {
        dir: tempfile::TempDir,
        _open: Catalog,
    }

    fn old_app() -> Old {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(settings::FILE_NAME),
            r#"{"primary":{"host":"10.0.0.1","username":"music"}}"#,
        )
        .unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.db")).unwrap();
        let meta = TrackMeta {
            title: Some("Song".into()),
            ..Default::default()
        };
        catalog
            .upsert_track(fxmp_core::DEFAULT_LIBRARY, "a/song.mp3", 1, 1, &meta)
            .unwrap();
        fs::write(dir.path().join("settings.json.bad-1"), "{").unwrap();
        fs::create_dir(dir.path().join("extra")).unwrap();
        fs::write(dir.path().join("extra").join("note.txt"), "x").unwrap();
        Old {
            dir,
            _open: catalog,
        }
    }

    #[test]
    fn the_old_folder_sits_next_to_the_new_one() {
        assert_eq!(
            old_dir_beside(Path::new(
                "/Users/u/Library/Application Support/hr.pvt.fxmp"
            )),
            Some(PathBuf::from(
                "/Users/u/Library/Application Support/hr.pvt.smb-music-player"
            ))
        );
    }

    #[test]
    fn a_developer_build_migrates_only_from_the_folder_it_is_given() {
        let new = Path::new("/tmp/dev-data");
        assert_eq!(old_dir(new, true, None), None, "never the real old folder");
        assert_eq!(old_dir(new, true, Some(OsString::new())), None);
        assert_eq!(
            old_dir(new, true, Some(OsString::from("/tmp/old"))),
            Some(PathBuf::from("/tmp/old"))
        );
        assert_eq!(
            old_dir(new, false, Some(OsString::from("/tmp/old"))),
            Some(PathBuf::from("/tmp/hr.pvt.smb-music-player")),
            "a release ignores the variable"
        );
        assert_eq!(old_service(true), "smb-music-player-dev");
        assert_eq!(old_service(false), "smb-music-player");
        assert_ne!(old_service(false), credentials::SERVICE);
    }

    #[test]
    fn files_are_copied_and_the_catalog_opens_with_its_tracks() {
        let old = old_app();
        let new = tempfile::tempdir().unwrap();
        let new_dir = new.path().join("hr.pvt.fxmp");
        assert!(old.dir.path().join("catalog.db-wal").exists());

        let copied = migrate_files(old.dir.path(), &new_dir).unwrap();

        assert!(matches!(copied, Files::Copied(n) if n >= 4), "{copied:?}");
        assert_eq!(
            fs::read_to_string(new_dir.join(settings::FILE_NAME)).unwrap(),
            fs::read_to_string(old.dir.path().join(settings::FILE_NAME)).unwrap()
        );
        assert!(new_dir.join("catalog.db-wal").exists());
        assert!(
            !new_dir.join("catalog.db-shm").exists(),
            "SQLite rebuilds it"
        );
        assert!(new_dir.join("settings.json.bad-1").exists());
        assert_eq!(
            fs::read_to_string(new_dir.join("extra").join("note.txt")).unwrap(),
            "x"
        );
        assert!(!new_dir.join(STAGING).exists());
        assert!(new_dir.join(MARKER).exists());
        assert!(passwords_pending(&new_dir));
        let settings = Settings::load(&new_dir.join(settings::FILE_NAME)).unwrap();
        assert_eq!(settings.primary.host, "10.0.0.1");
        let copy = Catalog::open(&new_dir.join("catalog.db")).unwrap();
        assert!(!copy.is_empty().unwrap(), "the track in the log came over");
    }

    #[test]
    fn the_old_folder_is_left_untouched() {
        let old = old_app();
        let before = snapshot(old.dir.path());
        let new = tempfile::tempdir().unwrap();
        migrate_files(old.dir.path(), new.path()).unwrap();
        // Opening the copy must not reach back into the old folder either.
        drop(Catalog::open(&new.path().join("catalog.db")).unwrap());
        assert_eq!(snapshot(old.dir.path()), before);
    }

    #[test]
    fn it_runs_once() {
        let old = old_app();
        let new = tempfile::tempdir().unwrap();
        migrate_files(old.dir.path(), new.path()).unwrap();
        // The new app changes its settings, and even loses its catalog.
        fs::write(new.path().join(settings::FILE_NAME), "{}").unwrap();
        fs::remove_file(new.path().join("catalog.db")).unwrap();
        fs::remove_file(new.path().join("catalog.db-wal")).unwrap();

        assert_eq!(
            migrate_files(old.dir.path(), new.path()).unwrap(),
            Files::AlreadyMigrated
        );
        assert_eq!(
            fs::read_to_string(new.path().join(settings::FILE_NAME)).unwrap(),
            "{}"
        );
        assert!(!new.path().join("catalog.db").exists());
        assert!(passwords_pending(new.path()), "still pending");
        mark_done(new.path()).unwrap();
        assert!(!passwords_pending(new.path()));
        assert_eq!(
            migrate_files(old.dir.path(), new.path()).unwrap(),
            Files::AlreadyMigrated
        );
    }

    #[test]
    fn a_new_folder_with_data_is_left_alone() {
        let old = old_app();
        for existing in [settings::FILE_NAME, "catalog.db"] {
            let new = tempfile::tempdir().unwrap();
            fs::write(new.path().join(existing), "mine").unwrap();
            assert_eq!(
                migrate_files(old.dir.path(), new.path()).unwrap(),
                Files::NewHasData
            );
            let names: Vec<_> = fs::read_dir(new.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, [existing], "nothing copied, no marker");
            assert!(!passwords_pending(new.path()));
        }
    }

    #[test]
    fn without_an_old_folder_there_is_nothing_to_do() {
        let parent = tempfile::tempdir().unwrap();
        let new = parent.path().join("new");
        assert_eq!(
            migrate_files(&parent.path().join("missing"), &new).unwrap(),
            Files::NoOldData
        );
        assert!(!new.join(MARKER).exists());
    }

    #[test]
    fn a_staging_folder_left_by_a_crash_is_replaced() {
        let old = old_app();
        let new = tempfile::tempdir().unwrap();
        fs::create_dir(new.path().join(STAGING)).unwrap();
        fs::write(new.path().join(STAGING).join("junk"), "x").unwrap();
        migrate_files(old.dir.path(), new.path()).unwrap();
        assert!(!new.path().join(STAGING).exists());
        assert!(!new.path().join("junk").exists());
        assert!(new.path().join(settings::FILE_NAME).exists());
    }

    fn settings_with_libraries() -> Settings {
        Settings {
            primary: ServerConfig {
                host: "10.0.0.1".into(),
                username: "music".into(),
                ..ServerConfig::default()
            },
            secondary: Some(ServerConfig {
                host: "10.0.0.2".into(),
                username: "backup".into(),
                ..ServerConfig::default()
            }),
            libraries: vec![
                Library {
                    id: 2,
                    name: String::new(),
                    enabled: false,
                    source: LibrarySource::Smb {
                        primary: ServerConfig {
                            host: "nas".into(),
                            username: "me".into(),
                            ..ServerConfig::default()
                        },
                        secondary: None,
                    },
                },
                Library {
                    id: 3,
                    name: String::new(),
                    enabled: true,
                    source: LibrarySource::Local {
                        path: "/Volumes/Music".into(),
                    },
                },
            ],
            last_library_id: 3,
            ..Settings::default()
        }
    }

    #[test]
    fn passwords_are_copied_and_the_old_ones_kept() {
        let old = MemoryStore::default();
        old.put(1, "10.0.0.1", "music", "pw1");
        old.put(1, "10.0.0.2", "backup", "pw2");
        old.put(2, "nas", "me", "pw3");
        // From before libraries: copied too (moved within the new service later).
        old.set("10.0.0.1/music", "legacy").unwrap();
        old.set("unrelated", "x").unwrap();
        let new = MemoryStore::default();

        let done = copy_passwords(&old, &new, &settings_with_libraries());

        assert_eq!(done.copied, 4, "{done:?}");
        assert_eq!(done.failed, 0);
        assert_eq!(
            new.accounts(),
            [
                "10.0.0.1/music",
                "library-1/10.0.0.1/music",
                "library-1/10.0.0.2/backup",
                "library-2/nas/me"
            ]
        );
        assert_eq!(
            credentials::server_password(
                &new,
                2,
                &ServerConfig {
                    host: "nas".into(),
                    username: "me".into(),
                    ..ServerConfig::default()
                }
            )
            .as_deref(),
            Some("pw3")
        );
        assert_eq!(old.accounts().len(), 5, "the old entries stay");
    }

    #[test]
    fn a_password_the_new_service_has_is_not_overwritten() {
        let old = MemoryStore::default();
        old.put(1, "10.0.0.1", "music", "old");
        let new = MemoryStore::default();
        new.put(1, "10.0.0.1", "music", "new");
        let done = copy_passwords(&old, &new, &settings_with_libraries());
        assert_eq!(done.present, 1);
        assert_eq!(done.copied, 0);
        assert_eq!(new.get("library-1/10.0.0.1/music").as_deref(), Some("new"));
    }

    /// The user refuses every read of the old app's entries: each is asked once.
    #[derive(Default)]
    struct Refusing {
        prompts: std::sync::Mutex<Vec<String>>,
    }
    impl CredentialStore for Refusing {
        fn get(&self, account: &str) -> Option<String> {
            self.prompts.lock().unwrap().push(account.to_string());
            None
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            panic!("the old service is never written")
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            panic!("the old service is never changed")
        }
    }

    #[test]
    fn a_refused_read_loses_nothing_and_is_asked_once() {
        let old = Refusing::default();
        let new = MemoryStore::default();
        let mut s = settings_with_libraries();
        // The same server twice: still one prompt for its entry.
        s.libraries.push(Library {
            id: 4,
            name: String::new(),
            enabled: true,
            source: LibrarySource::Smb {
                primary: s.primary.clone(),
                secondary: Some(s.primary.clone()),
            },
        });
        let done = copy_passwords(&old, &new, &s);
        assert_eq!(done.copied, 0);
        assert_eq!(done.missing, 6, "{done:?}");
        assert!(new.accounts().is_empty());
        let mut prompts = old.prompts.lock().unwrap().clone();
        let asked = prompts.len();
        prompts.sort();
        prompts.dedup();
        assert_eq!(asked, prompts.len(), "no entry asked twice: {prompts:?}");
    }

    /// Writes are lost: nothing reads back.
    #[derive(Default)]
    struct Forgetful;
    impl CredentialStore for Forgetful {
        fn get(&self, _: &str) -> Option<String> {
            None
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn a_password_that_does_not_read_back_counts_as_failed() {
        let old = MemoryStore::default();
        old.put(1, "10.0.0.1", "music", "pw1");
        let done = copy_passwords(&old, &Forgetful, &settings_with_libraries());
        assert_eq!(done.failed, 1);
        assert_eq!(done.copied, 0);
        assert_eq!(old.accounts().len(), 1);
    }
}
