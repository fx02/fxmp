//! The first start of fxmp after SMB Music Player (its old name): the old app's
//! settings, catalog and playlist are copied from its folder, and its keychain
//! passwords from its keychain service, once. Nothing of the old app is changed
//! or deleted (but its login item), so it keeps working until the owner removes it.
//!
//! A marker file in the new folder tracks the migration: "copying files", then
//! "passwords pending", then "done".
//!
//! Files: when there is no marker, the new folder has neither settings nor a
//! catalog (a fresh fxmp user's data is never replaced) and the old folder
//! exists, the marker is set to "copying files" and every file of the old folder
//! is copied (`catalog.db-shm` excepted: SQLite rebuilds it from the database
//! and its log) to a staging folder. The old folder is listed (size and time of
//! each file) before and after: if the old app wrote meanwhile, or a file could
//! not be copied, the copy is taken again. The copies are then moved into the
//! new folder, the settings and the database last. While the marker says
//! "copying files", whatever is in the new folder is left over from an
//! interrupted attempt, so the next start copies again. A start whose copy
//! failed keeps its data in a throwaway folder ([`unmigrated_dir`]), so the
//! empty catalog it opens never hides the old data from the next attempt.
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

use crate::credentials::{self, CredentialStore};
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
/// The database's files that must never be mixed with another copy's.
const DATABASE_FILES: [&str; 3] = ["catalog.db", "catalog.db-wal", "catalog.db-shm"];
const UNMIGRATED: &str = ".unmigrated-session";
const STAGING: &str = ".migrating-from-smb-music-player";
const PENDING: &str = "copying files\n";
const PASSWORDS_PENDING: &str = "files copied; passwords pending\n";
const DONE: &str = "done\n";
/// Not copied: SQLite rebuilds it, and a copy may not match the log.
const SKIPPED_SUFFIX: &str = "-shm";

/// What [`migrate_files`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Files {
    /// Done on an earlier start.
    AlreadyMigrated,
    /// There is no old folder.
    NoOldData,
    /// The new folder had settings or a catalog before any attempt: left alone.
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

/// How often copies are taken again.
struct Retry {
    /// Copies of the whole folder.
    attempts: usize,
    delay: Duration,
    /// Copies of one file that fails.
    file_attempts: usize,
    file_delay: Duration,
}

/// At most about 0.6 s of waiting, so a start stays quick (see the report of
/// the measured copy time).
const RETRY: Retry = Retry {
    attempts: 4,
    delay: Duration::from_millis(150),
    file_attempts: 3,
    file_delay: Duration::from_millis(25),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    None,
    Pending,
    PasswordsPending,
    Done,
}

fn marker(new: &Path) -> Marker {
    match fs::read_to_string(new.join(MARKER)) {
        Err(_) => Marker::None,
        // Empty: torn while "copying files" was written.
        Ok(m) if m == PENDING || m.is_empty() => Marker::Pending,
        Ok(m) if m == PASSWORDS_PENDING => Marker::PasswordsPending,
        // Anything else: done, so nothing is ever copied over the new data.
        Ok(_) => Marker::Done,
    }
}

/// The old app's folder next to `new_dir` (the same parent, the old identifier).
pub fn old_dir_beside(new_dir: &Path) -> Option<PathBuf> {
    new_dir.parent().map(|p| p.join(OLD_IDENTIFIER))
}

/// Copies the old app's files from `old` to `new` unless that was done before,
/// `old` does not exist or `new` had data before. `old` is only read.
pub fn migrate_files(old: &Path, new: &Path) -> io::Result<Files> {
    migrate_files_with(old, new, &RETRY, &mut |_| {})
}

fn migrate_files_with(
    old: &Path,
    new: &Path,
    retry: &Retry,
    before_copy: &mut dyn FnMut(&Path),
) -> io::Result<Files> {
    match marker(new) {
        Marker::PasswordsPending | Marker::Done => return Ok(Files::AlreadyMigrated),
        Marker::Pending if !old.is_dir() => {
            log::warn!(
                "migration: {} is gone; the interrupted migration is given up",
                old.display()
            );
            // What the interrupted attempt left must not pass for the user's own:
            // a log without its database would corrupt the next one.
            remove_leftovers(new)?;
            remove_dir_if_there(&new.join(STAGING))?;
            fs::remove_file(new.join(MARKER))?;
            return Ok(Files::NoOldData);
        }
        Marker::Pending => log::info!("migration: an interrupted migration is done again"),
        Marker::None => {
            if !old.is_dir() {
                return Ok(Files::NoOldData);
            }
            if new.join(settings::FILE_NAME).exists() || new.join(DATABASE).exists() {
                return Ok(Files::NewHasData);
            }
            fs::create_dir_all(new)?;
            fs::write(new.join(MARKER), PENDING)?;
        }
    }
    let staging = new.join(STAGING);
    let copied = copy_consistent(old, &staging, retry, before_copy)?;
    // Left over from an interrupted attempt: a log of another copy would
    // corrupt the database.
    remove_leftovers(new)?;
    // The settings and the database last: the log must be in place first.
    let mut names: Vec<_> = fs::read_dir(&staging)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort_by_key(|n| n == DATABASE || n == settings::FILE_NAME);
    for name in names {
        let target = new.join(&name);
        if target.is_dir() {
            fs::remove_dir_all(&target)?;
        } else {
            remove_file_if_there(&target)?;
        }
        fs::rename(staging.join(&name), target)?;
    }
    remove_dir_if_there(&staging)?;
    fs::write(new.join(MARKER), PASSWORDS_PENDING)?;
    Ok(Files::Copied(copied))
}

/// Copies `old` to `staging` until a copy matches the old folder before and
/// after it; gives up after `retry.attempts` copies (and removes `staging`).
fn copy_consistent(
    old: &Path,
    staging: &Path,
    retry: &Retry,
    before_copy: &mut dyn FnMut(&Path),
) -> io::Result<usize> {
    let mut last_problem = String::new();
    for attempt in 1..=retry.attempts {
        if attempt > 1 {
            std::thread::sleep(retry.delay);
        }
        remove_dir_if_there(staging)?;
        let copy = listing(old).and_then(|before| {
            let copied = copy_tree(old, staging, retry, before_copy)?;
            Ok((before, copied))
        });
        match copy {
            Ok((before, copied)) => match listing(old) {
                Ok(after) if after == before => return Ok(copied),
                Ok(_) => last_problem = "the old app changed its files meanwhile".into(),
                Err(e) => last_problem = e.to_string(),
            },
            Err(e) => last_problem = e.to_string(),
        }
        log::info!("migration: copy {attempt} not usable ({last_problem})");
    }
    remove_dir_if_there(staging)?;
    Err(io::Error::other(format!(
        "{OLD_APP_NAME}'s files could not be copied: {last_problem}"
    )))
}

/// Removes the database and settings files an interrupted attempt left in `new`.
fn remove_leftovers(new: &Path) -> io::Result<()> {
    for name in DATABASE_FILES.iter().copied().chain([settings::FILE_NAME]) {
        remove_file_if_there(&new.join(name))?;
    }
    Ok(())
}

fn remove_file_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn remove_dir_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether the files came over but the passwords are still to copy.
pub fn passwords_pending(new: &Path) -> bool {
    marker(new) == Marker::PasswordsPending
}

/// Records that the passwords were copied too: the migration is over.
pub fn mark_done(new: &Path) -> io::Result<()> {
    fs::write(new.join(MARKER), DONE)
}

/// Whether the file is copied (and compared): not links, not `-shm`.
fn wanted(name: &std::ffi::OsStr, meta: &fs::Metadata) -> bool {
    !meta.file_type().is_symlink() && !name.to_string_lossy().ends_with(SKIPPED_SUFFIX)
}

/// Every copied file under `dir` with its size and modification time. A file
/// gone meanwhile is left out.
fn listing(dir: &Path) -> io::Result<Vec<(PathBuf, u64, Option<SystemTime>)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let meta = match fs::symlink_metadata(entry.path()) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                other => other?,
            };
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

/// Copies the files under `from` to `to`; returns how many. A file that fails
/// is tried again; one that is gone meanwhile is left out (the listing after the
/// copy then differs, so the copy is taken again).
fn copy_tree(
    from: &Path,
    to: &Path,
    retry: &Retry,
    before_copy: &mut dyn FnMut(&Path),
) -> io::Result<usize> {
    fs::create_dir_all(to)?;
    let mut copied = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let meta = match fs::symlink_metadata(entry.path()) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            other => other?,
        };
        if !wanted(&entry.file_name(), &meta) {
            continue;
        }
        let target = to.join(entry.file_name());
        if meta.is_dir() {
            copied += copy_tree(&entry.path(), &target, retry, before_copy)?;
            continue;
        }
        before_copy(&entry.path());
        let mut attempt = 1;
        loop {
            match fs::copy(entry.path(), &target) {
                Ok(_) => {
                    copied += 1;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) if attempt < retry.file_attempts => {
                    log::info!("migration: copying a file again ({e})");
                    attempt += 1;
                    std::thread::sleep(retry.file_delay);
                }
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("{}: {e}", entry.path().display()),
                    ))
                }
            }
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
/// new service already has is not overwritten. Each old entry is read once (the
/// accounts are distinct).
pub fn copy_passwords(
    old: &dyn CredentialStore,
    new: &dyn CredentialStore,
    settings: &Settings,
) -> Passwords {
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

/// The old app's keychain service: a debug build with its own data folder
/// (`dev`) uses the old app's debug service.
pub fn old_service(dev: bool) -> &'static str {
    if dev {
        OLD_DEV_SERVICE
    } else {
        OLD_SERVICE
    }
}

/// Where a start migrates from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The old app's folder next to the new one (a release build).
    Beside,
    /// This folder (a debug build with `SMP_DATA_DIR` and [`DEV_ENV`]).
    Folder(PathBuf),
    /// Nowhere.
    Skip,
}

impl Source {
    fn old_dir(&self, new_dir: &Path) -> Option<PathBuf> {
        match self {
            Source::Beside => old_dir_beside(new_dir),
            Source::Folder(dir) => Some(dir.clone()),
            Source::Skip => None,
        }
    }
}

/// Where to migrate from: a release build from the old app's folder; a debug
/// build never from the installed app's data, so only with its own data folder
/// (`dev_dir`) and from the folder in [`DEV_ENV`] (`dev_from`).
pub fn source(debug: bool, dev_dir: bool, dev_from: Option<OsString>) -> Source {
    if !debug {
        return Source::Beside;
    }
    match dev_from.filter(|v| dev_dir && !v.is_empty()) {
        Some(dir) => Source::Folder(PathBuf::from(dir)),
        None => Source::Skip,
    }
}

/// The folders a start uses and what its migration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    /// Files were copied now.
    pub copied: bool,
    /// The copy failed: this start uses a throwaway folder (see [`unmigrated_dir`]).
    pub failed: bool,
    /// The passwords are still to copy (see [`passwords_step`]).
    pub passwords_pending: bool,
    /// The failure message's language (see [`message_language`]) when `failed`.
    pub message_language: Option<String>,
}

/// Where a start whose migration failed keeps its data, so nothing in the new
/// folder hides the old data from the next start's attempt.
pub fn unmigrated_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(UNMIGRATED)
}

/// The start's migration of the files; returns the folders to use. The settings
/// go to `config_dir` when it is another folder than `data_dir`.
pub fn prepare(config_dir: PathBuf, data_dir: PathBuf, source: &Source) -> Prepared {
    let aside = unmigrated_dir(&data_dir);
    if let Err(e) = remove_dir_if_there(&aside) {
        log::warn!("migration: cannot remove {}: {e}", aside.display());
    }
    let mut copied = false;
    let mut failed = false;
    let old_dir = source.old_dir(&data_dir);
    if let Some(old) = &old_dir {
        let has_own_settings =
            config_dir != data_dir && config_dir.join(settings::FILE_NAME).exists();
        let result = if has_own_settings && marker(&data_dir) == Marker::None {
            Ok(Files::NewHasData)
        } else {
            migrate_files(old, &data_dir)
        };
        match result {
            Ok(Files::Copied(n)) => {
                log::info!(
                    "migration: {n} files of {OLD_APP_NAME} copied from {} to {}",
                    old.display(),
                    data_dir.display()
                );
                copied = true;
                if config_dir != data_dir {
                    if let Err(e) = move_settings(&data_dir, &config_dir) {
                        log::warn!("migration: cannot move the settings: {e}");
                    }
                }
            }
            Ok(Files::NewHasData) => log::info!(
                "migration: {} already has data; {} left alone",
                data_dir.display(),
                old.display()
            ),
            Ok(Files::AlreadyMigrated | Files::NoOldData) => {}
            Err(e) => {
                log::warn!("migration from {} failed: {e}", old.display());
                failed = true;
            }
        }
    }
    if failed {
        if let Err(e) = fs::create_dir_all(&aside) {
            log::warn!("migration: cannot create {}: {e}", aside.display());
        }
        return Prepared {
            message_language: Some(message_language(&config_dir, old_dir.as_deref())),
            config_dir: aside.clone(),
            data_dir: aside,
            copied,
            failed,
            passwords_pending: false,
        };
    }
    Prepared {
        passwords_pending: passwords_pending(&data_dir),
        message_language: None,
        config_dir,
        data_dir,
        copied,
        failed,
    }
}

fn move_settings(data_dir: &Path, config_dir: &Path) -> io::Result<()> {
    let from = data_dir.join(settings::FILE_NAME);
    if !from.exists() {
        return Ok(());
    }
    fs::create_dir_all(config_dir)?;
    fs::copy(&from, config_dir.join(settings::FILE_NAME))?;
    fs::remove_file(from)
}

/// Copies the passwords if the files came over and they are still to copy, and
/// records the migration as done. `None`: nothing to do.
pub fn passwords_step(
    data_dir: &Path,
    old: &dyn CredentialStore,
    new: &dyn CredentialStore,
    settings: &Settings,
) -> Option<Passwords> {
    if !passwords_pending(data_dir) {
        return None;
    }
    let done = copy_passwords(old, new, settings);
    log::info!(
        "migration: passwords of {OLD_APP_NAME}: {} copied, {} already here, {} not read, {} \
         failed",
        done.copied,
        done.present,
        done.missing,
        done.failed
    );
    if let Err(e) = mark_done(data_dir) {
        log::warn!("migration: cannot record it as done: {e}");
    }
    Some(done)
}

/// The language of the failure message: the new settings', else the old
/// settings', else English (a shipped language only).
pub fn message_language(new_config_dir: &Path, old_dir: Option<&Path>) -> String {
    let language_in = |dir: &Path| -> Option<String> {
        let text = fs::read_to_string(dir.join(settings::FILE_NAME)).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        value.get("language")?.as_str().map(str::to_string)
    };
    language_in(new_config_dir)
        .or_else(|| old_dir.and_then(language_in))
        .filter(|l| crate::i18n::is_supported(l))
        .unwrap_or_else(|| "en".into())
}

/// Tells the user that the copy failed and how to get it to work (a message box
/// that does not block the start).
pub fn tell_failed(app: &tauri::AppHandle, language: &str) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    app.dialog()
        .message(crate::i18n::tr(language, "migrate.failed"))
        .title("fxmp")
        .kind(MessageDialogKind::Warning)
        .show(|_| {});
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
    use std::ffi::OsString;

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
                    let name = e.path().strip_prefix(dir).unwrap().to_path_buf();
                    // The fixture's still-open SQLite connection owns -shm, and on
                    // Windows NTFS updates the times of its open WAL/shm files
                    // lazily. Compare those by content only.
                    let live = name.to_string_lossy().ends_with("-shm")
                        || name.to_string_lossy().ends_with("-wal");
                    let time = if live {
                        SystemTime::UNIX_EPOCH
                    } else {
                        meta.modified().unwrap()
                    };
                    if !name.to_string_lossy().ends_with("-shm") {
                        out.insert(name, (fs::read(e.path()).unwrap(), time));
                    }
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

    fn quick() -> Retry {
        Retry {
            attempts: 3,
            delay: Duration::from_millis(1),
            file_attempts: 2,
            file_delay: Duration::from_millis(1),
        }
    }

    #[test]
    fn an_old_app_writing_all_the_time_fails_the_copy_and_the_next_start_copies() {
        let old = old_app();
        let new = tempfile::tempdir().unwrap();
        let mut writes = 0;
        let result = migrate_files_with(old.dir.path(), new.path(), &quick(), &mut |_| {
            writes += 1;
            fs::write(
                old.dir.path().join("extra").join("note.txt"),
                "y".repeat(writes),
            )
            .unwrap();
        });
        assert!(result.is_err(), "{result:?}");
        assert!(!new.path().join(STAGING).exists());
        assert!(!new.path().join(settings::FILE_NAME).exists());
        assert!(!new.path().join(DATABASE).exists());
        assert!(!passwords_pending(new.path()));
        // The next start: the old app is quiet now.
        assert!(matches!(
            migrate_files(old.dir.path(), new.path()).unwrap(),
            Files::Copied(_)
        ));
        assert!(passwords_pending(new.path()));
        assert!(!Catalog::open(&new.path().join(DATABASE))
            .unwrap()
            .is_empty()
            .unwrap());
    }

    #[test]
    fn a_log_that_vanishes_during_the_copy_is_copied_again_without_it() {
        let old = old_app();
        let Old { dir, _open } = old;
        let mut open = Some(_open);
        let new = tempfile::tempdir().unwrap();
        let result = migrate_files_with(dir.path(), new.path(), &quick(), &mut |path| {
            // The old app quits just before its log is copied: SQLite moves the
            // log into the database and deletes it.
            if path.to_string_lossy().ends_with("-wal") {
                open.take();
            }
        })
        .unwrap();
        assert!(matches!(result, Files::Copied(_)), "{result:?}");
        assert!(!dir.path().join("catalog.db-wal").exists());
        assert!(!new.path().join("catalog.db-wal").exists());
        assert!(!Catalog::open(&new.path().join(DATABASE))
            .unwrap()
            .is_empty()
            .unwrap());
    }

    #[test]
    fn a_copy_interrupted_between_the_renames_is_redone() {
        let old = old_app();
        let new = tempfile::tempdir().unwrap();
        migrate_files(old.dir.path(), new.path()).unwrap();
        // As if the last start stopped after moving the log and the settings
        // but before the database, and the old app has quit since (its log is
        // in its database now).
        fs::write(new.path().join(MARKER), PENDING).unwrap();
        fs::remove_file(new.path().join(DATABASE)).unwrap();
        fs::create_dir(new.path().join(STAGING)).unwrap();
        fs::write(new.path().join(STAGING).join(DATABASE), "half").unwrap();
        let Old { dir, _open } = old;
        drop(_open);
        assert!(!dir.path().join("catalog.db-wal").exists());

        assert!(matches!(
            migrate_files(dir.path(), new.path()).unwrap(),
            Files::Copied(_)
        ));
        assert!(
            !new.path().join("catalog.db-wal").exists(),
            "a stale log would corrupt it"
        );
        assert!(!new.path().join(STAGING).exists());
        assert!(passwords_pending(new.path()));
        assert!(!Catalog::open(&new.path().join(DATABASE))
            .unwrap()
            .is_empty()
            .unwrap());
    }

    #[test]
    fn the_source_depends_on_the_build() {
        let from = || Some(OsString::from("/tmp/old"));
        assert_eq!(source(false, false, from()), Source::Beside);
        assert_eq!(
            source(true, false, from()),
            Source::Skip,
            "plain debug build"
        );
        assert_eq!(source(true, true, None), Source::Skip);
        assert_eq!(source(true, true, Some(OsString::new())), Source::Skip);
        assert_eq!(
            source(true, true, from()),
            Source::Folder("/tmp/old".into())
        );
        assert_eq!(old_service(true), "smb-music-player-dev");
        assert_eq!(old_service(false), "smb-music-player");
        assert_ne!(old_service(false), credentials::SERVICE);
    }

    /// `parent/hr.pvt.smb-music-player` with the old app's files, and the new
    /// folder next to it.
    fn installed() -> (tempfile::TempDir, Old, PathBuf) {
        let parent = tempfile::tempdir().unwrap();
        let old = old_app();
        let old_dir = parent.path().join(OLD_IDENTIFIER);
        copy_tree(old.dir.path(), &old_dir, &quick(), &mut |_| {}).unwrap();
        let new = parent.path().join("hr.pvt.fxmp");
        (parent, old, new)
    }

    #[test]
    fn a_release_start_migrates_once_and_leaves_the_passwords_pending() {
        let (_parent, _old, new) = installed();
        let first = prepare(new.clone(), new.clone(), &Source::Beside);
        assert_eq!(
            first,
            Prepared {
                config_dir: new.clone(),
                data_dir: new.clone(),
                copied: true,
                failed: false,
                passwords_pending: true,
                message_language: None
            }
        );
        let second = prepare(new.clone(), new.clone(), &Source::Beside);
        assert!(!second.copied && !second.failed && second.passwords_pending);
    }

    #[test]
    fn a_plain_debug_build_does_not_migrate() {
        let (_parent, _old, new) = installed();
        let p = prepare(new.clone(), new.clone(), &Source::Skip);
        assert!(!p.copied && !p.failed && !p.passwords_pending);
        assert!(!new.join(settings::FILE_NAME).exists());
    }

    #[test]
    fn settings_go_to_their_own_folder_when_it_is_another_one() {
        let (parent, _old, new) = installed();
        let config = parent.path().join("config");
        let p = prepare(config.clone(), new.clone(), &Source::Beside);
        assert!(p.copied);
        assert!(config.join(settings::FILE_NAME).exists());
        assert!(!new.join(settings::FILE_NAME).exists());
        assert!(new.join(DATABASE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_start_keeps_its_data_aside_and_the_next_start_migrates() {
        use std::os::unix::fs::PermissionsExt;
        let (parent, _old, new) = installed();
        let locked = parent
            .path()
            .join(OLD_IDENTIFIER)
            .join("extra")
            .join("note.txt");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let failed = prepare(new.clone(), new.clone(), &Source::Beside);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(failed.failed && !failed.copied && !failed.passwords_pending);
        assert_eq!(failed.data_dir, unmigrated_dir(&new));
        assert_eq!(failed.message_language.as_deref(), Some("en"));
        assert_eq!(failed.config_dir, unmigrated_dir(&new));
        // That start opens its (empty) catalog and saves settings aside.
        drop(Catalog::open(&failed.data_dir.join(DATABASE)).unwrap());
        fs::write(failed.config_dir.join(settings::FILE_NAME), "{}").unwrap();
        assert!(!new.join(DATABASE).exists());

        let next = prepare(new.clone(), new.clone(), &Source::Beside);
        assert!(next.copied && !next.failed && next.passwords_pending);
        assert_eq!(next.data_dir, new);
        assert!(
            !unmigrated_dir(&new).exists(),
            "the throwaway folder is gone"
        );
        assert!(!Catalog::open(&new.join(DATABASE))
            .unwrap()
            .is_empty()
            .unwrap());
        assert_eq!(
            Settings::load(&new.join(settings::FILE_NAME))
                .unwrap()
                .primary
                .host,
            "10.0.0.1"
        );
    }

    #[test]
    fn the_password_step_runs_once_after_the_files() {
        let (_parent, _old, new) = installed();
        let old = MemoryStore::default();
        old.put(1, "10.0.0.1", "music", "pw1");
        let keychain = MemoryStore::default();
        let s = settings_with_libraries();
        assert_eq!(
            passwords_step(&new, &old, &keychain, &s),
            None,
            "no files yet"
        );
        prepare(new.clone(), new.clone(), &Source::Beside);
        let done = passwords_step(&new, &old, &keychain, &s).unwrap();
        assert_eq!(done.copied, 1);
        assert!(!passwords_pending(&new));
        assert_eq!(passwords_step(&new, &old, &keychain, &s), None);
        assert_eq!(keychain.accounts(), ["library-1/10.0.0.1/music"]);
    }

    #[test]
    fn with_nothing_set_up_the_password_step_reads_no_keychain() {
        let (_parent, _old, new) = installed();
        prepare(new.clone(), new.clone(), &Source::Beside);
        let old = Refusing::default();
        let keychain = MemoryStore::default();
        let done = passwords_step(&new, &old, &keychain, &Settings::default()).unwrap();
        assert_eq!(done, Passwords::default());
        assert!(old.prompts.lock().unwrap().is_empty());
        assert_eq!(keychain.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!passwords_pending(&new));
    }

    #[test]
    fn an_empty_marker_counts_as_copying() {
        let old = old_app();
        let new = tempfile::tempdir().unwrap();
        // Power lost while the marker was written: an empty file.
        fs::write(new.path().join(MARKER), "").unwrap();
        fs::write(new.path().join("catalog.db-wal"), "stale").unwrap();
        assert!(matches!(
            migrate_files(old.dir.path(), new.path()).unwrap(),
            Files::Copied(_)
        ));
        assert!(!Catalog::open(&new.path().join(DATABASE))
            .unwrap()
            .is_empty()
            .unwrap());
    }

    #[test]
    fn giving_up_an_interrupted_copy_removes_its_leftovers() {
        let parent = tempfile::tempdir().unwrap();
        let new = parent.path().join("new");
        fs::create_dir(&new).unwrap();
        fs::write(new.join(MARKER), PENDING).unwrap();
        for name in [
            "catalog.db-wal",
            "catalog.db-shm",
            "catalog.db",
            "settings.json",
        ] {
            fs::write(new.join(name), "left over").unwrap();
        }
        fs::create_dir(new.join(STAGING)).unwrap();
        assert_eq!(
            migrate_files(&parent.path().join("gone"), &new).unwrap(),
            Files::NoOldData
        );
        let names: Vec<_> = fs::read_dir(&new)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn the_failure_message_is_in_the_users_language() {
        let new = tempfile::tempdir().unwrap();
        let old = tempfile::tempdir().unwrap();
        let write =
            |dir: &Path, text: &str| fs::write(dir.join(settings::FILE_NAME), text).unwrap();
        assert_eq!(
            message_language(new.path(), Some(old.path())),
            "en",
            "nothing to go by"
        );
        assert_eq!(message_language(new.path(), None), "en");
        write(old.path(), r#"{"language":"hr"}"#);
        assert_eq!(
            message_language(new.path(), Some(old.path())),
            "hr",
            "the old app's"
        );
        write(new.path(), "{ not json");
        assert_eq!(
            message_language(new.path(), Some(old.path())),
            "hr",
            "unreadable new"
        );
        write(new.path(), r#"{"language":"en"}"#);
        assert_eq!(
            message_language(new.path(), Some(old.path())),
            "en",
            "the new one first"
        );
        write(new.path(), r#"{"volume":0.5}"#);
        assert_eq!(
            message_language(new.path(), Some(old.path())),
            "hr",
            "no language in new"
        );
        write(old.path(), r#"{"language":"xx"}"#);
        assert_eq!(
            message_language(new.path(), Some(old.path())),
            "en",
            "not shipped"
        );
    }

    /// The copy of a library the size of the owner's (59,000 tracks), in a
    /// release build: `cargo test --release -p fxmp copy_time -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn copy_time_of_a_large_library() {
        let old = tempfile::tempdir().unwrap();
        fs::write(old.path().join(settings::FILE_NAME), "{}").unwrap();
        let mut catalog = Catalog::open(&old.path().join(DATABASE)).unwrap();
        catalog
            .transaction(|c| {
                for i in 0..59_000 {
                    let meta = TrackMeta {
                        artist: Some(format!("Some Artist Name {}", i / 120)),
                        album: Some(format!("A Fairly Long Album Title {}", i / 12)),
                        title: Some(format!("The Title Of Track Number {i}")),
                        raw_genre: Some("Progressive Trance".into()),
                        duration_ms: Some(240_000),
                        year: Some(1990 + (i % 30) as u16),
                        ..Default::default()
                    };
                    let path = format!(
                        "MP3/Some Artist Name {}/A Fairly Long Album Title {} ({})/{:02} - The \
                         Title Of Track Number {i}.mp3",
                        i / 120,
                        i / 12,
                        1990 + i % 30,
                        i % 12
                    );
                    c.upsert_track(fxmp_core::DEFAULT_LIBRARY, &path, 9_000_000, 1, &meta)?;
                }
                Ok(())
            })
            .unwrap();
        let size = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let (db, wal) = (
            size(&old.path().join(DATABASE)),
            size(&old.path().join("catalog.db-wal")),
        );
        for run in 1..=3 {
            let new = tempfile::tempdir().unwrap();
            let started = std::time::Instant::now();
            let files = migrate_files(old.path(), new.path()).unwrap();
            let copy = started.elapsed();
            let started = std::time::Instant::now();
            assert!(!Catalog::open(&new.path().join(DATABASE))
                .unwrap()
                .is_empty()
                .unwrap());
            println!(
                "run {run}: db {db} B + wal {wal} B, {files:?}: copy {copy:?}, first open {:?}",
                started.elapsed()
            );
        }
    }
}
