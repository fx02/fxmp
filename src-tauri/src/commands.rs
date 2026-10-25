//! Tauri commands. Every command body runs on its own plain thread: Tauri runs sync
//! commands on the main thread and async ones on its tokio runtime, and neither may
//! block on SMB, the keychain or long catalog queries.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use smp_core::audio::output::list_output_devices;
use smp_core::audio::player::Command;
use smp_core::genre::{known_genre, known_genre_names};
use smp_core::source::Which;
use smp_core::{LibraryId, TrackId, DEFAULT_LIBRARY};
use tauri::{AppHandle, Emitter, Manager};

use crate::about;
use crate::autostart;
use crate::connection::{
    folder_status, status_of, test_server, test_target, ConnStatus, StatusPayload,
};
use crate::credentials::{self, CredentialStore};
use crate::i18n;
use crate::lookup;
use crate::playlist::{self, PlaylistFacets, PlaylistPage, PreviousAction, QueueFilter, Selection};
use crate::settings::{Library, LibrarySource, ServerConfig, Settings, SettingsStore};
use crate::state::{
    self, apply_player_settings, lock, AppState, Engine, NowPlaying, PositionPayload, Press,
    ResumeGate, ScanStatus,
};
use crate::tray;

type CmdResult<T> = Result<T, String>;

/// Runs `f` on a new thread and waits for it without blocking the caller's thread.
async fn run<T: Send + 'static>(
    app: AppHandle,
    f: impl FnOnce(&AppHandle, &AppState) -> CmdResult<T> + Send + 'static,
) -> CmdResult<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("command".into())
        .spawn(move || {
            let st = app.state::<AppState>();
            let _ = tx.send(f(&app, &st));
        })
        .map_err(|e| e.to_string())?;
    rx.await.map_err(|_| i18n::t("err.command"))?
}

/// Runs `f` on a new thread for callers that do not wait for the result (tray, media
/// keys); a failure is logged.
pub fn spawn_action(
    app: &AppHandle,
    what: &'static str,
    f: impl FnOnce(&AppHandle, &AppState) -> CmdResult<()> + Send + 'static,
) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("action".into())
        .spawn(move || {
            let st = app.state::<AppState>();
            if let Err(e) = f(&app, &st) {
                log::warn!("{what}: {e}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("{what}: {e}");
    }
}

fn db_err(e: smp_core::Error) -> String {
    log::warn!("catalog: {e}");
    i18n::tf("err.database", &[("error", &e.to_string())])
}

#[derive(Debug, Clone, Serialize)]
pub struct Genre {
    pub key: String,
    pub display: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BannedTrack {
    pub id: TrackId,
    pub artist: String,
    pub title: String,
    pub year: Option<u16>,
    pub rel_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Album {
    pub name: String,
    pub year: Option<u16>,
}

/// A row of the "Unreadable" list.
#[derive(Debug, Clone, Serialize)]
pub struct UnreadableRow {
    pub id: TrackId,
    /// `smb://host/share/path/to/file.mp3` as read (not encoded), for the eye.
    pub path: String,
    /// The same as a valid, percent-encoded URL, for "Copy path".
    pub url: String,
    /// Failure code (`not_found`, `timeout`, …); none if not recorded.
    pub code: Option<String>,
    pub message: Option<String>,
    pub fail_count: u32,
    /// Unix seconds.
    pub at: Option<i64>,
    /// Left out of the shuffle until the next refresh.
    pub excluded: bool,
    /// For "Copy 'Artist – Title'" and "Look up" (the file name when untitled).
    pub artist: String,
    pub title: String,
    pub year: Option<u16>,
}

/// Where a track lives, as a readable path and as a URL.
#[derive(Debug, Clone, PartialEq)]
pub struct FileLocation {
    pub path: String,
    pub url: String,
}

/// Percent-encodes a URL path segment: everything but RFC 3986 `pchar`s
/// (unreserved, sub-delims, `:` and `@`) as UTF-8 bytes.
fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Where track `rel_path` of library `library` lives. In an SMB library:
/// `smb://host/share/root/rel_path` on the server the failure came from (the
/// primary when unknown or no backup is set up), never with a username or
/// password, even one typed into the server field. In a local folder: the file's
/// path (with the folder's separators) and its `file://` URL. Just `rel_path` for
/// a library no longer set up.
pub fn file_location(
    settings: &Settings,
    library: LibraryId,
    server: Option<Which>,
    rel_path: &str,
) -> FileLocation {
    match settings.library(library).map(|l| l.source) {
        Some(LibrarySource::Smb { primary, secondary }) => {
            let cfg = match (server, &secondary) {
                (Some(Which::Secondary), Some(secondary)) => secondary,
                _ => &primary,
            };
            smb_location(cfg, rel_path)
        }
        Some(LibrarySource::Local { path }) => local_location(&path, rel_path),
        None => FileLocation {
            path: rel_path.to_string(),
            url: rel_path.to_string(),
        },
    }
}

/// A file in local folder `root` (a macOS/Linux path, or a Windows one with `\`).
fn local_location(root: &str, rel_path: &str) -> FileLocation {
    let root = root.trim();
    let windows = root.contains('\\') && !root.starts_with('/');
    let sep = if windows { "\\" } else { "/" };
    let rel: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    let path = format!(
        "{}{sep}{}",
        root.trim_end_matches(['/', '\\']),
        rel.join(sep)
    );
    let segments: Vec<String> = root
        .split(['/', '\\'])
        .chain(rel.iter().copied())
        .filter(|s| !s.is_empty())
        .map(encode_segment)
        .collect();
    // `/Volumes/x` and `D:\x` are file:///…; a network path `\\server\share` is file://server/….
    let lead = if root.starts_with("\\\\") { "" } else { "/" };
    FileLocation {
        path,
        url: format!("file://{lead}{}", segments.join("/")),
    }
}

fn smb_location(cfg: &ServerConfig, rel_path: &str) -> FileLocation {
    let host = cfg.host.trim();
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let segments: Vec<&str> = [cfg.share.trim(), cfg.root.trim(), rel_path]
        .iter()
        .flat_map(|part| part.split('/'))
        .filter(|seg| !seg.is_empty())
        .collect();
    let encoded: Vec<String> = segments.iter().map(|s| encode_segment(s)).collect();
    FileLocation {
        path: format!("smb://{host}/{}", segments.join("/")),
        url: format!("smb://{host}/{}", encoded.join("/")),
    }
}

/// Resets the settings (see [`Settings::reset`]) and saves them at once; with
/// `forget_servers` every library's keychain passwords are deleted first. `apply`
/// runs with the old and the new settings, as in [`SettingsStore::replace_and_save`].
pub fn reset_settings_now(
    store: &SettingsStore,
    creds: &dyn CredentialStore,
    forget_servers: bool,
    apply: impl FnOnce(&Settings, &Settings),
) -> CmdResult<Settings> {
    let old = store.get();
    if forget_servers {
        for library in old.all_libraries() {
            forget_passwords(creds, library.id, &library.source, None)?;
        }
    }
    store
        .replace_and_save(old.reset(forget_servers), apply)
        .map_err(|e| i18n::tf("err.save_settings", &[("error", &e.to_string())]))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_settings(app: AppHandle) -> CmdResult<Settings> {
    run(app, |_, st| Ok(st.settings.get())).await
}

/// Deletes the passwords of `source`'s servers (library `library`) that `kept`
/// (the library's new source) no longer has.
fn forget_passwords(
    creds: &dyn CredentialStore,
    library: LibraryId,
    source: &LibrarySource,
    kept: Option<&LibrarySource>,
) -> CmdResult<()> {
    let servers = |s: Option<&LibrarySource>| -> Vec<ServerConfig> {
        match s {
            Some(LibrarySource::Smb { primary, secondary }) => std::iter::once(primary.clone())
                .chain(secondary.clone())
                .collect(),
            _ => Vec::new(),
        }
    };
    let same = |a: &ServerConfig, b: &ServerConfig| {
        a.host.trim() == b.host.trim() && a.username.trim() == b.username.trim()
    };
    let kept = servers(kept);
    for old in servers(Some(source)) {
        if !kept.iter().any(|k| same(k, &old)) {
            credentials::delete_server_password(creds, library, &old)?;
        }
    }
    Ok(())
}

/// Whether saving server `form` needs a typed password (as the form's own rule):
/// the password is stored per host and username, so a new account (or share) has
/// none to keep.
pub fn password_needed(saved: Option<&ServerConfig>, form: &ServerConfig) -> bool {
    let Some(saved) = saved.filter(|s| !s.username.trim().is_empty()) else {
        return true;
    };
    saved.host.trim() != form.host.trim()
        || saved.share.trim() != form.share.trim()
        || saved.username.trim() != form.username.trim()
}

/// The servers of an SMB source.
fn smb_servers(source: &LibrarySource) -> Option<(&ServerConfig, Option<&ServerConfig>)> {
    match source {
        LibrarySource::Smb { primary, secondary } => Some((primary, secondary.as_ref())),
        LibrarySource::Local { .. } => None,
    }
}

/// `settings` with `library` saved: a new one (id 0) gets the next id, an existing
/// one is changed (the first library only in its name and switch; its servers are
/// the server form's). Checks that it is complete; `typed` says whether a password
/// was typed for its primary and secondary server. Also returns the library as
/// saved and whether its source changed (a new library's did).
pub fn save_library_settings(
    settings: &Settings,
    library: Library,
    typed: (bool, bool),
) -> CmdResult<(Settings, Library, bool)> {
    let mut new = settings.clone();
    let name = library.name.trim().to_string();
    if library.id == DEFAULT_LIBRARY {
        if smb_servers(&library.source).is_none() {
            return Err(i18n::t("err.no_library"));
        }
        new.default_library_name = name;
        new.default_library_enabled = library.enabled;
        let saved = new.library(DEFAULT_LIBRARY).expect("always there");
        return Ok((new, saved, false));
    }
    let old = match library.id {
        0 => None,
        id => Some(
            settings
                .libraries
                .iter()
                .find(|l| l.id == id)
                .cloned()
                .ok_or_else(|| i18n::t("err.no_library"))?,
        ),
    };
    let source = match library.source {
        LibrarySource::Local { path } if path.trim().is_empty() => {
            return Err(i18n::t("err.missing_folder"))
        }
        LibrarySource::Local { path } => LibrarySource::Local {
            path: path.trim().to_string(),
        },
        smb @ LibrarySource::Smb { .. } => smb,
    };
    if let Some((primary, secondary)) = smb_servers(&source) {
        let old_servers = old.as_ref().and_then(|o| smb_servers(&o.source));
        let checks = [
            (Some(primary), old_servers.map(|o| o.0), typed.0),
            (secondary, old_servers.and_then(|o| o.1), typed.1),
        ];
        for (server, saved, typed) in checks {
            let Some(server) = server else { continue };
            if server.host.trim().is_empty() {
                return Err(i18n::t("err.missing_host"));
            }
            if server.username.trim().is_empty() {
                return Err(i18n::t("err.missing_user"));
            }
            if !typed && password_needed(saved, server) {
                return Err(i18n::t("err.missing_password"));
            }
        }
    }
    let saved = Library {
        id: old
            .as_ref()
            .map_or_else(|| settings.next_library_id(), |o| o.id),
        name,
        enabled: library.enabled,
        source,
    };
    let changed = old.as_ref().is_none_or(|o| o.source != saved.source);
    new.take_library_id(saved.id);
    match new.libraries.iter_mut().find(|l| l.id == saved.id) {
        Some(slot) => *slot = saved.clone(),
        None => new.libraries.push(saved.clone()),
    }
    Ok((new, saved, changed))
}

/// Saves library `saved` (formerly `before`): its typed passwords (`typed`:
/// primary, secondary) go to the keychain, then `save` writes the settings, and
/// only once that worked are the passwords of servers it no longer has deleted
/// (a failed save leaves the old server with its password).
pub fn store_library<T>(
    creds: &dyn CredentialStore,
    before: Option<&Library>,
    saved: &Library,
    typed: (Option<&str>, Option<&str>),
    save: impl FnOnce() -> CmdResult<T>,
) -> CmdResult<T> {
    if let Some((primary, secondary)) = smb_servers(&saved.source) {
        if let Some(pw) = typed.0 {
            credentials::set_server_password(creds, saved.id, primary, pw)?;
        }
        if let (Some(server), Some(pw)) = (secondary, typed.1) {
            credentials::set_server_password(creds, saved.id, server, pw)?;
        }
    }
    let result = save()?;
    if let Some(before) = before {
        forget_passwords(creds, saved.id, &before.source, Some(&saved.source))?;
    }
    Ok(result)
}

/// Whether any library's source differs between `old` and `new` (or one came or
/// went): a scan running then reads a source that is no longer set.
fn sources_changed(old: &Settings, new: &Settings) -> bool {
    let sources = |s: &Settings| -> Vec<(LibraryId, LibrarySource)> {
        s.all_libraries()
            .into_iter()
            .map(|l| (l.id, l.source))
            .collect()
    };
    sources(old) != sources(new)
}

/// Saves settings; a non-empty password goes to the keychain, an empty or missing
/// one keeps the stored password.
#[tauri::command(rename_all = "snake_case")]
pub async fn save_settings(
    app: AppHandle,
    settings: Settings,
    primary_password: Option<String>,
    secondary_password: Option<String>,
) -> CmdResult<()> {
    run(app, move |app, st| {
        let settings = settings.clamped();
        let store = |server: Option<&ServerConfig>, pw: Option<String>| match (
            server,
            pw.filter(|p| !p.is_empty()),
        ) {
            (Some(s), Some(pw)) => {
                credentials::set_server_password(st.creds.as_ref(), DEFAULT_LIBRARY, s, &pw)
            }
            _ => Ok(()),
        };
        store(Some(&settings.primary), primary_password)?;
        store(settings.secondary.as_ref(), secondary_password)?;
        let mut servers_changed = false;
        let saved = st
            .settings
            .replace_and_save(settings, |old, new| {
                servers_changed = sources_changed(old, new);
                apply_player_settings(&st.player, new);
            })
            .map_err(|e| i18n::tf("err.save_settings", &[("error", &e.to_string())]))?;
        if servers_changed {
            state::cancel_scan(st, Some(&i18n::t("scan.cancelled_settings")));
        }
        st.reconnect(&saved);
        tray::refresh_audio(app);
        autostart::apply(app, saved.autostart, false).map_err(|e| {
            log::warn!("start at login: {e}");
            i18n::t("err.autostart")
        })
    })
    .await
}

/// Connects once to server `which` ("primary"/"secondary") of library `library`
/// (the first one when not given): "ok", "auth" or "unreachable". `server` and
/// `password` are the form's unsaved values; without them the saved settings and
/// the keychain password are used, and the live connection to that server forgets
/// a rejected password and is re-checked.
#[tauri::command(rename_all = "snake_case")]
pub async fn test_connection(
    app: AppHandle,
    library: Option<LibraryId>,
    which: String,
    server: Option<ServerConfig>,
    password: Option<String>,
) -> CmdResult<ConnStatus> {
    let library = library.unwrap_or(DEFAULT_LIBRARY);
    run(app, move |_, st| {
        let saved = server.is_none();
        let cfg = test_target(
            &st.settings.get(),
            st.creds.as_ref(),
            library,
            &which,
            server,
            password,
        )?;
        let status = status_of(test_server(cfg), None);
        if saved {
            let libs = st.source.libraries();
            if let Some(servers) = libs.as_ref().and_then(|l| l.get(library)?.servers()) {
                let live = match which.as_str() {
                    "primary" => Some(&servers.primary),
                    _ => servers.secondary.as_ref(),
                };
                if let Some(live) = live {
                    live.reconnect();
                }
                st.wake_monitor();
            }
        }
        Ok(status)
    })
    .await
}

/// "Test" for a local folder: "ok", or "missing" when it is not there or cannot
/// be read (a disk not plugged in).
#[tauri::command(rename_all = "snake_case")]
pub async fn test_folder(app: AppHandle, path: String) -> CmdResult<ConnStatus> {
    run(app, move |_, _| {
        if path.trim().is_empty() {
            return Err(i18n::t("err.missing_folder"));
        }
        Ok(folder_status(std::path::Path::new(path.trim())))
    })
    .await
}

/// "Choose folder…": the system's folder picker; `None` when cancelled.
#[tauri::command(rename_all = "snake_case")]
pub async fn pick_folder(app: AppHandle, start: Option<String>) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    run(app, move |app, _| {
        let mut dialog = app
            .dialog()
            .file()
            .set_title(i18n::t("libraries.choose_folder_title"));
        if let Some(dir) = start.filter(|d| std::path::Path::new(d).is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        // Blocks this command thread only; the dialog itself runs on the main thread.
        let Some(chosen) = dialog.blocking_pick_folder() else {
            return Ok(None);
        };
        let path = chosen
            .into_path()
            .map_err(|e| i18n::tf("err.folder", &[("error", &e.to_string())]))?;
        Ok(Some(path.display().to_string()))
    })
    .await
}

/// A library for the Libraries list.
#[derive(Debug, Clone, Serialize)]
pub struct LibraryInfo {
    #[serde(flatten)]
    pub library: Library,
    /// Where its music is: `smb://host/share/folder` or the folder's path.
    pub location: String,
    /// Tracks of it in the catalog.
    pub tracks: usize,
}

fn library_info(
    library: Library,
    counts: &std::collections::HashMap<LibraryId, usize>,
) -> LibraryInfo {
    let location = match &library.source {
        LibrarySource::Smb { primary, .. } => smb_location(primary, "").path,
        LibrarySource::Local { path } => path.clone(),
    };
    LibraryInfo {
        tracks: counts.get(&library.id).copied().unwrap_or(0),
        location,
        library,
    }
}

/// Every library, the first (SMB) one first, with its track count.
#[tauri::command(rename_all = "snake_case")]
pub async fn get_libraries(app: AppHandle) -> CmdResult<Vec<LibraryInfo>> {
    run(app, |_, st| {
        let counts = lock(&st.query).library_track_counts().map_err(db_err)?;
        Ok(st
            .settings
            .get()
            .all_libraries()
            .into_iter()
            .map(|l| library_info(l, &counts))
            .collect())
    })
    .await
}

/// After a library changed: its tracks out of the playlist if it is off, and the
/// views that show tracks reloaded.
fn library_changed(app: &AppHandle, st: &AppState) {
    if let Err(e) = state::prune_playlist(app, st) {
        log::warn!("playlist after a library change: {e}");
    }
    let _ = app.emit("genres-changed", ());
    let _ = app.emit(state::UNREADABLE_EVENT, ());
    let _ = app.emit("libraries-changed", ());
}

/// Adds (id 0) or changes a library (see [`save_library_settings`]); a typed
/// password goes to the keychain under the library, an empty one keeps the stored
/// password. Passwords of servers it no longer has are deleted; a changed source
/// stops a running scan. Returns the library as saved.
#[tauri::command(rename_all = "snake_case")]
pub async fn save_library(
    app: AppHandle,
    library: Library,
    primary_password: Option<String>,
    secondary_password: Option<String>,
) -> CmdResult<LibraryInfo> {
    run(app, move |app, st| {
        let primary_password = primary_password.filter(|p| !p.is_empty());
        let secondary_password = secondary_password.filter(|p| !p.is_empty());
        let mut old = st.settings.get();
        // Ids with tracks in the catalog (a library the settings lost) are taken too.
        let in_catalog = lock(&st.query).library_track_counts().map_err(db_err)?;
        for id in in_catalog.into_keys() {
            old.take_library_id(id);
        }
        let (new, saved, changed) = save_library_settings(
            &old,
            library,
            (primary_password.is_some(), secondary_password.is_some()),
        )?;
        let new = store_library(
            st.creds.as_ref(),
            old.library(saved.id).as_ref(),
            &saved,
            (primary_password.as_deref(), secondary_password.as_deref()),
            || {
                st.settings
                    .replace_and_save(new, |_, _| {})
                    .map_err(|e| i18n::tf("err.save_settings", &[("error", &e.to_string())]))
            },
        )?;
        if changed {
            state::cancel_scan(st, Some(&i18n::t("scan.cancelled_settings")));
        }
        st.forget_or_reconnect(app, &new);
        library_changed(app, st);
        let counts = lock(&st.query).library_track_counts().map_err(db_err)?;
        Ok(library_info(saved, &counts))
    })
    .await
}

/// Switches a library on or off (its tracks are kept either way).
#[tauri::command(rename_all = "snake_case")]
pub async fn set_library_enabled(app: AppHandle, id: LibraryId, enabled: bool) -> CmdResult<()> {
    run(app, move |app, st| {
        let old = st.settings.get();
        let library = old.library(id).ok_or_else(|| i18n::t("err.no_library"))?;
        let (new, _, _) =
            save_library_settings(&old, Library { enabled, ..library }, (false, false))?;
        let new = st
            .settings
            .replace_and_save(new, |_, _| {})
            .map_err(|e| i18n::tf("err.save_settings", &[("error", &e.to_string())]))?;
        st.forget_or_reconnect(app, &new);
        library_changed(app, st);
        Ok(())
    })
    .await
}

/// Removes a library and its tracks: see [`state::remove_library`].
#[tauri::command(rename_all = "snake_case")]
pub async fn remove_library(app: AppHandle, id: LibraryId) -> CmdResult<()> {
    run(app, move |app, _| {
        state::remove_library(app, id)?;
        let _ = app.emit("libraries-changed", ());
        tray::refresh_genres(app);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_connection_status(app: AppHandle) -> CmdResult<Option<StatusPayload>> {
    run(app, |_, st| Ok(lock(&st.status).clone())).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn start_scan(app: AppHandle) -> CmdResult<()> {
    run(app, |app, _| state::start_scan(app)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn cancel_scan(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| {
        state::cancel_scan(st, None);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_scan_status(app: AppHandle) -> CmdResult<ScanStatus> {
    run(app, |_, st| Ok(st.scan_status())).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_genres(app: AppHandle) -> CmdResult<Vec<Genre>> {
    run(app, |_, st| {
        let genres = lock(&st.query).genres().map_err(db_err)?;
        Ok(genres
            .into_iter()
            .map(|g| Genre {
                key: g.key,
                display: g.display,
                count: g.count,
            })
            .collect())
    })
    .await
}

/// Shuffles the selected genres (empty = everything) into the playlist, plays it
/// and remembers the selection.
pub fn play_genres_now(app: &AppHandle, st: &AppState, keys: Vec<String>) -> CmdResult<()> {
    let queue = playlist::genres_queue(&lock(&st.query), &keys).map_err(db_err)?;
    lock(&st.playlist).replace(Selection::Genres { keys: keys.clone() }, queue);
    st.play_playlist();
    st.settings.update(|s| s.last_selection = keys, |_| {});
    // The tray's genre menu lists the last selection.
    tray::refresh_genres(app);
    st.playlist_changed(app);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn play_genres(app: AppHandle, keys: Vec<String>) -> CmdResult<()> {
    run(app, move |app, st| play_genres_now(app, st, keys)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn play_album(app: AppHandle, artist: String, album: String) -> CmdResult<()> {
    run(app, move |app, st| {
        let queue = playlist::album_queue(&lock(&st.query), &artist, &album).map_err(db_err)?;
        lock(&st.playlist).replace(Selection::Album { artist, album }, queue);
        st.play_playlist();
        st.playlist_changed(app);
        Ok(())
    })
    .await
}

/// What pressing Play (or Play/Pause) does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayAction {
    /// The player pauses/resumes (or tries again after failures).
    Player,
    /// The restored playlist still waits for the share: it plays when it comes.
    RememberForResume,
    /// Nothing is loaded: the playlist plays from what is due.
    StartPlaylist,
    /// The restored playlist waits for a library that is offline while another
    /// one answers: the saved position is given up and the playlist plays (its
    /// tracks of the offline library are passed over).
    PlayWithoutResume,
}

/// `loaded`: a track is playing, paused or cued; `pending`: the restored playlist
/// waits for its library; `has_next`: the playlist has a track to play;
/// `elsewhere`: some library answers now.
pub fn play_action(loaded: bool, pending: bool, has_next: bool, elsewhere: bool) -> PlayAction {
    if loaded {
        PlayAction::Player
    } else if pending && has_next && elsewhere {
        PlayAction::PlayWithoutResume
    } else if pending {
        PlayAction::RememberForResume
    } else if has_next {
        PlayAction::StartPlaylist
    } else {
        PlayAction::Player
    }
}

/// Play/Pause (`TogglePause`) or Play (`Resume`) from the UI, tray or media keys;
/// pressing Play is never silent while there is something to play. "Loaded" is
/// what the engine reports (from a sent Play/Cue until it goes idle), not what
/// the window shows.
pub fn play_pause_now(st: &AppState, command: Command) {
    press_play_pause(
        &st.resume,
        &st.engine,
        || lock(&st.playlist).queue.successor().is_some(),
        || {
            lock(&st.status)
                .as_ref()
                .is_some_and(|s| s.libraries.iter().any(|l| l.status.reachable()))
        },
        || {
            log::info!("Play pressed while the resume waits on an offline library: moving on");
            lock(&st.playlist).resume_at = None;
        },
        command,
        |c| st.player.send(c),
        || st.play_playlist(),
    );
}

/// [`play_pause_now`] with its parts passed in: decided, and sent to the player
/// (`send`), while the resume gate is held, so the resume cannot slip in between.
/// `start_playlist` runs after the gate is let go (it takes it itself); before it,
/// `give_up_resume` drops the saved position when the press moves on from a
/// resume waiting on an offline library (`elsewhere`: another library answers).
#[allow(clippy::too_many_arguments)]
pub fn press_play_pause(
    gate: &Mutex<ResumeGate>,
    engine: &Mutex<Engine>,
    has_next: impl FnOnce() -> bool,
    elsewhere: impl FnOnce() -> bool,
    give_up_resume: impl FnOnce(),
    command: Command,
    send: impl FnOnce(Command),
    start_playlist: impl FnOnce(),
) {
    let press = match command {
        Command::Resume => Press::Play,
        Command::Pause => Press::Pause,
        _ => Press::Toggle,
    };
    let mut gate = lock(gate);
    let loaded = lock(engine).loaded();
    let pending = gate.pending;
    let has_next = !loaded && has_next();
    let elsewhere = !loaded && pending && elsewhere();
    match play_action(loaded, pending, has_next, elsewhere) {
        PlayAction::Player => send(command),
        PlayAction::RememberForResume => {
            gate.press(press);
            log::info!(
                "play/pause pressed while the playlist waits for the share (play: {})",
                gate.play_requested
            );
        }
        PlayAction::StartPlaylist => {
            drop(gate);
            start_playlist();
        }
        PlayAction::PlayWithoutResume => {
            drop(gate);
            give_up_resume();
            start_playlist();
        }
    }
}

/// Next from the UI, tray or media keys. While the restored playlist still waits
/// for its library nothing is loaded to skip: it counts as Play (which moves on
/// when another library answers, see [`play_pause_now`]).
pub fn next_now(st: &AppState) {
    if lock(&st.resume).pending && !lock(&st.engine).loaded() {
        play_pause_now(st, Command::Resume);
    } else {
        st.player.send(Command::Next);
    }
}

/// Pause (media key Pause/Stop): also takes back a Play pressed while the resume
/// waits. Media keys are wired on macOS only.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn pause_now(st: &AppState) {
    lock(&st.resume).press(Press::Pause);
    st.player.send(Command::Pause);
}

#[tauri::command(rename_all = "snake_case")]
pub async fn toggle_pause(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| {
        play_pause_now(st, Command::TogglePause);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn next(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| {
        next_now(st);
        Ok(())
    })
    .await
}

/// "Previous": more than 3 s into the track it starts again, otherwise the track
/// before it plays (with the same fade as "Next"). Shared by the UI, the tray and
/// the media keys.
pub fn previous_now(st: &AppState) -> CmdResult<()> {
    let position = lock(&st.position).clone();
    {
        let mut l = lock(&st.playlist);
        let current = l
            .queue
            .current()
            .ok_or_else(|| i18n::t("err.nothing_playing"))?;
        let ms = position
            .filter(|p| p.id == current)
            .map_or(0, |p| p.position_ms);
        match playlist::previous_action(ms, l.queue.can_go_back()) {
            PreviousAction::Restart => {
                l.queue.restart();
                l.resume_at = None;
            }
            PreviousAction::Back => {
                l.queue.go_back();
            }
        }
    }
    st.play_playlist();
    st.save_playlist_soon();
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn previous(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| previous_now(st)).await
}

/// The last reported position of the playing track.
#[tauri::command(rename_all = "snake_case")]
pub async fn get_position(app: AppHandle) -> CmdResult<Option<PositionPayload>> {
    run(app, |_, st| Ok(lock(&st.position).clone())).await
}

/// Rows `offset..offset + limit` (at most 500) of the playlist, or of what
/// `filter` leaves of it.
#[tauri::command(rename_all = "snake_case")]
pub async fn get_playlist(
    app: AppHandle,
    offset: usize,
    limit: usize,
    filter: Option<QueueFilter>,
) -> CmdResult<PlaylistPage> {
    run(app, move |_, st| {
        playlist::page(
            &st.playlist,
            &st.query,
            &i18n::current(),
            offset,
            limit,
            filter.as_ref(),
        )
        .map_err(db_err)
    })
    .await
}

/// The genres and years in the playlist, for its filters.
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_facets(app: AppHandle) -> CmdResult<PlaylistFacets> {
    run(app, |_, st| {
        playlist::facets(&st.playlist, &st.query, &i18n::current()).map_err(db_err)
    })
    .await
}

/// The entry at `index` must still be track `id` (the list may have changed since
/// the UI showed it).
fn check_entry(l: &playlist::Playlist, index: usize, id: TrackId) -> CmdResult<()> {
    if l.queue.order().get(index).map(|e| e.id) == Some(id) {
        Ok(())
    } else {
        Err(i18n::t("err.playlist_changed"))
    }
}

/// Plays the playlist entry at `index` now; the playlist goes on from there in
/// its order (a shuffle is not reshuffled).
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_jump(app: AppHandle, index: usize, id: TrackId) -> CmdResult<()> {
    run(app, move |_, st| {
        {
            let mut l = lock(&st.playlist);
            check_entry(&l, index, id)?;
            l.queue.jump(index);
            l.resume_at = None;
        }
        st.play_playlist();
        st.save_playlist_soon();
        Ok(())
    })
    .await
}

/// Changes the playlist; if that changed the track due next, the player drops the
/// one it prepared. Tells the UI and saves (unless `f` failed).
fn edit_playlist<T>(
    app: &AppHandle,
    st: &AppState,
    f: impl FnOnce(&mut playlist::Playlist) -> CmdResult<T>,
) -> CmdResult<T> {
    let (result, due_changed) = {
        let mut l = lock(&st.playlist);
        let before = l.queue.successor();
        let result = f(&mut l)?;
        (result, l.queue.successor() != before)
    };
    if due_changed {
        st.player.send(Command::Requeue);
    }
    st.playlist_changed(app);
    Ok(result)
}

/// Where hand-added tracks go.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddAt {
    /// Right after the current track ("Play next").
    Next,
    /// At the end ("Add to end").
    End,
}

fn add_to_playlist(
    app: &AppHandle,
    st: &AppState,
    ids: Vec<TrackId>,
    at: AddAt,
) -> CmdResult<usize> {
    let banned: HashSet<TrackId> = lock(&st.query)
        .banned()
        .map_err(db_err)?
        .into_iter()
        .map(|t| t.id)
        .chain(lock(&st.banned).iter().copied())
        .collect();
    let known: HashSet<TrackId> = {
        let q = lock(&st.query);
        let off = q.disabled_libraries().map_err(db_err)?;
        q.tracks_by_ids(&ids)
            .map_err(db_err)?
            .into_iter()
            .filter(|t| !off.contains(&t.library_id))
            .map(|t| t.id)
            .collect()
    };
    let ids: Vec<TrackId> = ids
        .into_iter()
        .filter(|id| known.contains(id) && !banned.contains(id))
        .collect();
    let n = ids.len();
    edit_playlist(app, st, |l| {
        match at {
            AddAt::Next => l.queue.insert_next(&ids),
            AddAt::End => l.queue.append(&ids),
        }
        Ok(n)
    })
}

/// Adds tracks by hand ("Play next" or "Add to end"); returns how many (banned
/// and unknown ones are left out).
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_add(app: AppHandle, ids: Vec<TrackId>, at: AddAt) -> CmdResult<usize> {
    run(app, move |app, st| add_to_playlist(app, st, ids, at)).await
}

/// Adds an album's tracks in order, as [`playlist_add`].
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_add_album(
    app: AppHandle,
    artist: String,
    album: String,
    at: AddAt,
) -> CmdResult<usize> {
    run(app, move |app, st| {
        let ids = lock(&st.query)
            .album_tracks(&artist, &album)
            .map_err(db_err)?;
        add_to_playlist(app, st, ids, at)
    })
    .await
}

/// Takes the entry at `index` (track `id`) out of the playlist.
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_remove(app: AppHandle, index: usize, id: TrackId) -> CmdResult<()> {
    run(app, move |app, st| {
        edit_playlist(app, st, |l| {
            check_entry(l, index, id)?;
            l.queue.remove(index);
            Ok(())
        })
    })
    .await
}

/// Moves the entry at `from` (track `id`) to `to`.
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_move(app: AppHandle, from: usize, to: usize, id: TrackId) -> CmdResult<()> {
    run(app, move |app, st| {
        edit_playlist(app, st, |l| {
            check_entry(l, from, id)?;
            if l.queue.move_entry(from, to) {
                Ok(())
            } else {
                Err(i18n::t("err.playlist_changed"))
            }
        })
    })
    .await
}

/// "New empty playlist": the playing track plays on, then nothing until tracks are
/// added by hand.
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_new(app: AppHandle) -> CmdResult<()> {
    run(app, |app, st| {
        edit_playlist(app, st, |l| {
            let empty = smp_core::queue::Queue::fixed(&[], smp_core::shuffle::entropy_rng());
            l.replace(Selection::Manual, empty);
            Ok(())
        })
    })
    .await
}

/// "Play filtered": a new shuffled playlist of what `filter` leaves of this one.
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_play_filtered(app: AppHandle, filter: QueueFilter) -> CmdResult<()> {
    run(app, move |app, st| {
        let (selection, queue) =
            playlist::filtered_queue(&lock(&st.query), &st.playlist, &filter).map_err(db_err)?;
        if queue.is_empty() {
            return Err(i18n::t("err.filter_empty"));
        }
        lock(&st.playlist).replace(selection, queue);
        st.play_playlist();
        st.playlist_changed(app);
        Ok(())
    })
    .await
}

/// Writes the playlist (what `filter` leaves of it) as `.m3u8` to `path` (`.m3u8`
/// is added when it has no extension); returns how many tracks and where.
fn export_to(st: &AppState, path: PathBuf, filter: Option<&QueueFilter>) -> CmdResult<Exported> {
    let mut path = path;
    if path.extension().is_none() {
        path.set_extension("m3u8");
    }
    let count = playlist::export_m3u8(&st.playlist, &st.query, &path, filter).map_err(|e| {
        log::warn!("export to {}: {e}", path.display());
        i18n::tf("err.export", &[("error", &e)])
    })?;
    Ok(Exported {
        count,
        path: path.display().to_string(),
    })
}

/// A finished export.
#[derive(Debug, Clone, Serialize)]
pub struct Exported {
    pub count: usize,
    pub path: String,
}

/// Asks where to save the playlist (the system's save dialog, suggesting
/// `file_name` in the mounted music folder when there is one), then saves it as
/// in [`export_to`]. `None` when the dialog was cancelled.
#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_export_save(
    app: AppHandle,
    file_name: String,
    filter: Option<QueueFilter>,
) -> CmdResult<Option<Exported>> {
    use tauri_plugin_dialog::DialogExt;
    run(app, move |app, st| {
        let server = st.settings.get().primary;
        let mut dialog = app
            .dialog()
            .file()
            .set_title(i18n::t("playlist.export_title"))
            .set_file_name(&file_name)
            .add_filter("M3U8", &["m3u8"]);
        if let Some(dir) = default_export_dir(&server) {
            dialog = dialog.set_directory(dir);
        }
        // Blocks this command thread only; the dialog itself runs on the main thread.
        let Some(chosen) = dialog.blocking_save_file() else {
            return Ok(None);
        };
        let path = chosen
            .into_path()
            .map_err(|e| i18n::tf("err.export", &[("error", &e.to_string())]))?;
        export_to(st, path, filter.as_ref()).map(Some)
    })
    .await
}

/// Suggestions for the export dialog.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportHint {
    /// The playlist's name as a file name, without date or extension.
    pub file_stem: String,
    /// The music folder on the share (`F/MP3/`): where the file belongs, since its
    /// paths are relative to it.
    pub music_folder: String,
    /// That folder where the share is mounted on this Mac, if it is.
    pub default_dir: Option<String>,
}

/// `share/root/` of a server, without stray slashes.
pub fn music_folder(server: &ServerConfig) -> String {
    let parts: Vec<&str> = [server.share.trim(), server.root.trim()]
        .iter()
        .flat_map(|p| p.split('/'))
        .filter(|p| !p.is_empty())
        .collect();
    format!("{}/", parts.join("/"))
}

/// The music folder where macOS mounts the share (`/Volumes/<share>/<root>`), or the
/// share itself, if it is mounted.
fn mounted_music_folder(server: &ServerConfig, exists: impl Fn(&str) -> bool) -> Option<String> {
    let share = server.share.trim().trim_matches('/');
    if share.is_empty() {
        return None;
    }
    let root = format!("/Volumes/{}", music_folder(server));
    let root = root.trim_end_matches('/').to_string();
    let share_dir = format!("/Volumes/{share}");
    [root, share_dir].into_iter().find(|d| exists(d))
}

/// The mounted music folder on macOS, if the share is mounted.
fn default_export_dir(server: &ServerConfig) -> Option<String> {
    if cfg!(target_os = "macos") {
        mounted_music_folder(server, |d| std::path::Path::new(d).is_dir())
    } else {
        None
    }
}

#[tauri::command(rename_all = "snake_case")]
pub async fn playlist_export_hint(app: AppHandle) -> CmdResult<ExportHint> {
    run(app, |_, st| {
        let selection = lock(&st.playlist).selection.clone();
        let title = playlist::title(&lock(&st.query), &i18n::current(), &selection);
        let server = st.settings.get().primary;
        Ok(ExportHint {
            file_stem: playlist::file_stem(&title),
            music_folder: music_folder(&server),
            default_dir: default_export_dir(&server),
        })
    })
    .await
}

/// A track of an album, for adding single tracks by hand.
#[derive(Debug, Clone, Serialize)]
pub struct AlbumTrack {
    pub id: TrackId,
    pub track_no: Option<u32>,
    pub artist: String,
    pub title: String,
    pub year: Option<u16>,
    pub duration_ms: Option<u64>,
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_album_tracks(
    app: AppHandle,
    artist: String,
    album: String,
) -> CmdResult<Vec<AlbumTrack>> {
    run(app, move |_, st| {
        let q = lock(&st.query);
        let ids = q.album_tracks(&artist, &album).map_err(db_err)?;
        let rows = q.tracks_by_ids(&ids).map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| AlbumTrack {
                id: r.id,
                track_no: r.meta.track_no,
                artist: r.meta.artist.clone().unwrap_or_default(),
                title: playlist::track_title(&r),
                year: r.meta.year,
                duration_ms: r.meta.duration_ms,
            })
            .collect())
    })
    .await
}

/// A lookup site for the window's menus: its id and name.
#[derive(Debug, Clone, Serialize)]
pub struct LookupServiceInfo {
    pub id: lookup::Service,
    pub name: &'static str,
}

#[tauri::command(rename_all = "snake_case")]
pub async fn lookup_services() -> Vec<LookupServiceInfo> {
    lookup::Service::ALL
        .iter()
        .map(|&id| LookupServiceInfo {
            id,
            name: id.name(),
        })
        .collect()
}

/// Opens the search for a track on `service` (the default one when `None`). Only
/// artist and title leave the app.
#[tauri::command(rename_all = "snake_case")]
pub async fn lookup_track(
    app: AppHandle,
    service: Option<lookup::Service>,
    artist: String,
    title: String,
) -> CmdResult<()> {
    run(app, move |app, st| {
        lookup::open(app, st, service, &artist, Some(&title))
    })
    .await
}

/// Opens the search for an artist on the default service.
#[tauri::command(rename_all = "snake_case")]
pub async fn lookup_artist(app: AppHandle, artist: String) -> CmdResult<()> {
    run(app, move |app, st| {
        lookup::open(app, st, None, &artist, None)
    })
    .await
}

/// Looks the current song up on the default service (a click on the now-playing
/// title; the tray uses [`lookup::open_current`] too).
#[tauri::command(rename_all = "snake_case")]
pub async fn lookup_current(app: AppHandle) -> CmdResult<()> {
    run(app, lookup::open_current).await
}

/// The current song as "Artist – Title (Year)", for copying; `None` when nothing
/// is loaded.
#[tauri::command(rename_all = "snake_case")]
pub async fn current_song_text(app: AppHandle) -> CmdResult<Option<String>> {
    run(app, |_, st| Ok(lookup::current_text(st))).await
}

/// "Default lookup service".
#[tauri::command(rename_all = "snake_case")]
pub async fn set_lookup_service(app: AppHandle, service: lookup::Service) -> CmdResult<()> {
    run(app, move |_, st| {
        st.settings.update(|s| s.lookup_service = service, |_| {});
        Ok(())
    })
    .await
}

/// "Resume playing automatically on start".
#[tauri::command(rename_all = "snake_case")]
pub async fn set_resume_autoplay(app: AppHandle, on: bool) -> CmdResult<()> {
    run(app, move |_, st| {
        st.settings.update(|s| s.resume_autoplay = on, |_| {});
        Ok(())
    })
    .await
}

/// The volume and the output device, sent as `audio-changed` whenever either
/// changes, so the window's slider and device list follow the tray.
#[derive(Debug, Clone, Serialize)]
pub struct AudioPayload {
    pub volume: f32,
    pub device: Option<String>,
}

fn audio_changed(app: &AppHandle, s: &Settings) {
    let payload = AudioPayload {
        volume: s.volume,
        device: s.device.clone(),
    };
    if let Err(e) = app.emit("audio-changed", payload) {
        log::warn!("audio-changed: {e}");
    }
    tray::refresh_audio(app);
}

/// Sets the volume (0..1) from the slider or the tray.
pub fn set_volume_now(app: &AppHandle, st: &AppState, v: f32) {
    let s = st.settings.update(
        |s| s.volume = v,
        |s| st.player.send(Command::SetVolume(s.volume)),
    );
    audio_changed(app, &s);
}

/// Picks the output device (`None` = system default) from Settings or the tray.
/// Picking the current one again is sent too: the player leaves an output open on
/// it alone (no dropout) and reopens it after an unplug fallback; the tray's marks
/// are put right either way.
pub fn set_device_now(app: &AppHandle, st: &AppState, name: Option<String>) {
    let s = switch_device(&st.settings, name, |d| {
        st.player.send(Command::SetDevice(d))
    });
    audio_changed(app, &s);
}

/// Saves `name` as the output device and calls `send` with it, under the settings
/// lock. Always sent: picking the saved device again brings it back after the
/// player fell back to the default output (the player ignores a device it has open).
pub fn switch_device(
    store: &SettingsStore,
    name: Option<String>,
    send: impl FnOnce(Option<String>),
) -> Settings {
    store.update(|s| s.device = name, |s| send(s.device.clone()))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_volume(app: AppHandle, v: f32) -> CmdResult<()> {
    run(app, move |app, st| {
        set_volume_now(app, st, v);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_devices(app: AppHandle) -> CmdResult<Vec<String>> {
    run(app, |_, _| Ok(list_output_devices())).await
}

/// `None` = system default.
#[tauri::command(rename_all = "snake_case")]
pub async fn set_device(app: AppHandle, name: Option<String>) -> CmdResult<()> {
    run(app, move |app, st| {
        set_device_now(app, st, name);
        Ok(())
    })
    .await
}

/// Name, version, commit and build date for "About".
#[tauri::command(rename_all = "snake_case")]
pub async fn app_about(app: AppHandle) -> about::About {
    about::about(&app)
}

/// Opens the project page on GitHub (the only URL besides the lookup sites).
#[tauri::command(rename_all = "snake_case")]
pub async fn open_project_page(app: AppHandle) -> CmdResult<()> {
    run(app, |app, _| about::open_project_page(app)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_crossfade(app: AppHandle, enabled: bool, secs: u8) -> CmdResult<()> {
    run(app, move |_, st| {
        st.settings.update(
            |s| {
                s.crossfade_enabled = enabled;
                s.crossfade_secs = secs;
            },
            |s| {
                st.player.send(Command::SetCrossfade {
                    enabled: s.crossfade_enabled,
                    secs: s.crossfade_secs,
                })
            },
        );
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_normalize(app: AppHandle, b: bool) -> CmdResult<()> {
    run(app, move |_, st| {
        st.settings.update(
            |s| s.normalize = b,
            |s| st.player.send(Command::SetNormalize(s.normalize)),
        );
        Ok(())
    })
    .await
}

/// Bans the playing track for good, takes it out of the playlist and skips to the
/// next one.
pub fn ban_current_now(app: &AppHandle, st: &AppState) -> CmdResult<()> {
    let id = lock(&st.now_playing)
        .as_ref()
        .map(|n| n.id)
        .ok_or_else(|| i18n::t("err.nothing_playing"))?;
    lock(&st.banned).insert(id);
    st.writer
        .call(move |c| c.set_banned(id, true))
        .map_err(db_err)?;
    st.player.send(Command::Next);
    lock(&st.playlist).queue.forget(id);
    st.playlist_changed(app);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn ban_current(app: AppHandle) -> CmdResult<()> {
    run(app, ban_current_now).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_banned(app: AppHandle) -> CmdResult<Vec<BannedTrack>> {
    run(app, |_, st| {
        let rows = lock(&st.query).banned().map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|t| BannedTrack {
                id: t.id,
                artist: t.meta.artist.unwrap_or_default(),
                title: t.meta.title.unwrap_or_default(),
                year: t.meta.year,
                rel_path: t.rel_path,
            })
            .collect())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn unban(app: AppHandle, id: TrackId) -> CmdResult<()> {
    run(app, move |_, st| {
        st.writer
            .call(move |c| c.set_banned(id, false))
            .map_err(db_err)?;
        lock(&st.banned).remove(&id);
        Ok(())
    })
    .await
}

/// The known genre a merge goes into, by its canonical name; errors in `lang`.
fn merge_target(lang: &str, into: &str) -> CmdResult<String> {
    let into = into.trim();
    if into.is_empty() {
        return Err(i18n::tr(lang, "err.choose_genre"));
    }
    known_genre(into)
        .map(|g| g.display)
        .ok_or_else(|| i18n::trf(lang, "err.not_a_genre", &[("name", into)]))
}

// The three commands below are cheap (tables built once), but async so that they
// run on Tauri's runtime and not on the main thread.

/// The UI texts of `lang` (English filling gaps), from `ui/locales`.
#[tauri::command(rename_all = "snake_case")]
pub async fn get_locale(lang: String) -> &'static serde_json::Map<String, serde_json::Value> {
    i18n::locale(&lang)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_languages() -> Vec<i18n::Language> {
    i18n::languages()
}

/// Switches the app's language, saves it and relabels the tray menu.
#[tauri::command(rename_all = "snake_case")]
pub async fn set_language(app: AppHandle, lang: String) -> CmdResult<()> {
    run(app, move |app, st| {
        if !i18n::is_supported(&lang) {
            return Err(i18n::tf("err.language", &[("code", &lang)]));
        }
        i18n::set_current(&lang);
        st.settings.update(|s| s.language = lang, |_| {});
        tray::relabel(app);
        Ok(())
    })
    .await
}

/// Display names of all known genres: the choices for "Merge into…".
#[tauri::command(rename_all = "snake_case")]
pub async fn known_genres() -> Vec<String> {
    known_genre_names()
}

/// Merges genre `from_key` into the known genre `into_display`; emits `genres-changed`.
#[tauri::command(rename_all = "snake_case")]
pub async fn merge_genre(app: AppHandle, from_key: String, into_display: String) -> CmdResult<()> {
    run(app, move |app, st| {
        let into = merge_target(&i18n::current(), &into_display)?;
        // Rebuilds genres over the whole library: on the writer's connection, so
        // playback and other queries are not held up.
        st.writer
            .call(move |c| c.add_genre_rule(&from_key, &into))
            .map_err(db_err)?;
        let _ = app.emit("genres-changed", ());
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_artists(app: AppHandle) -> CmdResult<Vec<String>> {
    run(app, |_, st| lock(&st.query).artists().map_err(db_err)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_albums(app: AppHandle, artist: String) -> CmdResult<Vec<Album>> {
    run(app, move |_, st| {
        let albums = lock(&st.query).albums(&artist).map_err(db_err)?;
        Ok(albums
            .into_iter()
            .map(|a| Album {
                name: a.name,
                year: a.year,
            })
            .collect())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_unreadable(app: AppHandle) -> CmdResult<Vec<UnreadableRow>> {
    run(app, |_, st| {
        let q = lock(&st.query);
        let rows = q.unreadable().map_err(db_err)?;
        let ids: Vec<TrackId> = rows.iter().map(|t| t.id).collect();
        let tags: std::collections::HashMap<TrackId, (String, String, Option<u16>)> = q
            .tracks_by_ids(&ids)
            .map_err(db_err)?
            .into_iter()
            .map(|r| {
                let title = playlist::track_title(&r);
                (
                    r.id,
                    (r.meta.artist.unwrap_or_default(), title, r.meta.year),
                )
            })
            .collect();
        drop(q);
        let settings = st.settings.get();
        Ok(rows
            .into_iter()
            .map(|t| {
                let loc = file_location(&settings, t.library_id, t.server, &t.rel_path);
                let tags = tags.get(&t.id).cloned().unwrap_or_default();
                (t, loc, tags)
            })
            .map(|(t, loc, (artist, title, year))| UnreadableRow {
                artist,
                title,
                year,
                id: t.id,
                path: loc.path,
                url: loc.url,
                code: t.code,
                message: t.message,
                fail_count: t.fail_count,
                at: t.at,
                excluded: t.excluded,
            })
            .collect())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn unreadable_count(app: AppHandle) -> CmdResult<usize> {
    run(app, |_, st| {
        lock(&st.query).unreadable_count().map_err(db_err)
    })
    .await
}

/// Lets an unreadable track (`id`), or all of them, play again.
#[tauri::command(rename_all = "snake_case")]
pub async fn retry_unreadable(app: AppHandle, id: Option<TrackId>) -> CmdResult<()> {
    run(app, move |app, st| {
        st.writer
            .call(move |c| match id {
                Some(id) => c.retry_failed(id),
                None => c.retry_all_failed().map(drop),
            })
            .map_err(db_err)?;
        let _ = app.emit(state::UNREADABLE_EVENT, ());
        // Tracks left out of the shuffle count again.
        let _ = app.emit("genres-changed", ());
        Ok(())
    })
    .await
}

/// "Reset settings to defaults" (see [`Settings::reset`]); with `forget_servers`
/// also the servers and their keychain passwords. Start at login is switched off.
#[tauri::command(rename_all = "snake_case")]
pub async fn reset_settings(app: AppHandle, forget_servers: bool) -> CmdResult<Settings> {
    run(app, move |app, st| {
        let mut servers_changed = false;
        let new = reset_settings_now(
            &st.settings,
            st.creds.as_ref(),
            forget_servers,
            |old, new| {
                servers_changed = sources_changed(old, new);
                apply_player_settings(&st.player, new);
            },
        )?;
        log::info!("settings reset to defaults (servers forgotten: {forget_servers})");
        if forget_servers {
            // The other libraries went with the servers: so do their tracks.
            // (The scan is stopped first, so it cannot write them back.)
            let gone: Vec<LibraryId> = lock(&st.query)
                .library_track_counts()
                .map_err(db_err)?
                .into_keys()
                .filter(|id| new.library(*id).is_none())
                .collect();
            state::delete_library_tracks(st, &gone)?;
            if let Err(e) = state::prune_playlist(app, st) {
                log::warn!("playlist after the reset: {e}");
            }
            let _ = app.emit("genres-changed", ());
            let _ = app.emit("libraries-changed", ());
        }
        if servers_changed || forget_servers {
            state::cancel_scan(st, Some(&i18n::t("scan.cancelled_settings")));
            st.forget_or_reconnect(app, &new);
        }
        // The tray's genre menu lists the (now cleared) last selection.
        tray::refresh_genres(app);
        audio_changed(app, &new);
        autostart::apply(app, new.autostart, false).map_err(|e| {
            log::warn!("start at login: {e}");
            i18n::t("err.autostart")
        })?;
        Ok(new)
    })
    .await
}

/// "Clear library": see [`state::clear_library`].
#[tauri::command(rename_all = "snake_case")]
pub async fn clear_library(app: AppHandle) -> CmdResult<()> {
    run(app, |app, _| state::clear_library(app)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn now_playing(app: AppHandle) -> CmdResult<Option<NowPlaying>> {
    run(app, |_, st| Ok(lock(&st.now_playing).clone())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::credentials::{CredentialStore, MemoryStore};
    use crate::settings::SettingsStore;
    use smp_core::source::Which;

    fn two_servers() -> Settings {
        Settings {
            primary: ServerConfig {
                host: " 10.111.100.2 ".into(),
                share: "F".into(),
                root: "/MP3/".into(),
                username: "music".into(),
                domain: None,
            },
            secondary: Some(ServerConfig {
                host: "10.111.0.2".into(),
                share: "F".into(),
                root: "".into(),
                username: "backup".into(),
                domain: Some("WG".into()),
            }),
            ..Settings::default()
        }
    }

    #[test]
    fn file_location_uses_the_failing_server_and_no_credentials() {
        let s = two_servers();
        let loc = |s: &Settings, w, p| file_location(s, 1, w, p);
        assert_eq!(
            loc(&s, None, "Rock/A b.mp3").path,
            "smb://10.111.100.2/F/MP3/Rock/A b.mp3"
        );
        assert_eq!(
            loc(&s, Some(Which::Primary), "x.mp3").url,
            "smb://10.111.100.2/F/MP3/x.mp3"
        );
        assert_eq!(
            loc(&s, Some(Which::Secondary), "x.mp3").url,
            "smb://10.111.0.2/F/x.mp3"
        );
        let one = Settings {
            secondary: None,
            ..s.clone()
        };
        assert_eq!(
            loc(&one, Some(Which::Secondary), "x.mp3").url,
            "smb://10.111.100.2/F/MP3/x.mp3",
            "falls back to the primary"
        );
        let both = loc(&one, None, "x.mp3");
        assert!(!both.url.contains("music") && !both.path.contains("music"));
        // A username typed into the server field is left out too.
        let typed = Settings {
            primary: ServerConfig {
                host: "music:pw@10.0.0.1".into(),
                ..s.primary.clone()
            },
            ..s
        };
        let l = loc(&typed, None, "x.mp3");
        assert_eq!(l.url, "smb://10.0.0.1/F/MP3/x.mp3");
        assert_eq!(l.path, "smb://10.0.0.1/F/MP3/x.mp3");
    }

    #[test]
    fn file_url_is_percent_encoded_and_the_path_readable() {
        let s = two_servers();
        let l = file_location(&s, 1, None, "Rock & Roll/A b#1 100%?ž.mp3");
        assert_eq!(
            l.url,
            "smb://10.111.100.2/F/MP3/Rock%20&%20Roll/A%20b%231%20100%25%3F%C5%BE.mp3"
        );
        assert_eq!(
            l.path,
            "smb://10.111.100.2/F/MP3/Rock & Roll/A b#1 100%?ž.mp3"
        );
    }

    #[test]
    fn reset_settings_keeps_or_forgets_servers_and_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::settings::FILE_NAME);
        let creds = MemoryStore::default();
        creds.put(1, "10.111.100.2", "music", "pw1");
        creds.put(1, "10.111.0.2", "backup", "pw2");
        creds
            .set("10.111.100.2/music", "from before libraries")
            .unwrap();
        creds.put(2, "10.0.0.7", "u", "pw3");
        let start = Settings {
            libraries: vec![crate::settings::Library {
                id: 2,
                name: "Office".into(),
                enabled: true,
                source: LibrarySource::Smb {
                    primary: ServerConfig {
                        host: "10.0.0.7".into(),
                        username: "u".into(),
                        ..ServerConfig::default()
                    },
                    secondary: None,
                },
            }],
            volume: 0.1,
            notify: true,
            autostart: true,
            language: "hr".into(),
            last_selection: vec!["rock".into()],
            ..two_servers()
        };
        let store = SettingsStore::new(path.clone(), start.clone()).unwrap();

        let mut applied = None;
        let kept = reset_settings_now(&store, &creds, false, |old, new| {
            applied = Some((old.volume, new.volume))
        })
        .unwrap();
        assert_eq!(applied, Some((0.1, 0.7)));
        assert_eq!(kept.primary, start.primary);
        assert_eq!(kept.secondary, start.secondary);
        assert_eq!((kept.notify, kept.autostart), (false, false));
        assert!(kept.last_selection.is_empty());
        assert_eq!(kept.language, "hr");
        assert_eq!(Settings::load(&path).unwrap(), kept, "saved at once");
        assert_eq!(kept.libraries, start.libraries);
        assert_eq!(creds.accounts().len(), 4, "passwords kept");

        let forgot = reset_settings_now(&store, &creds, true, |_, _| {}).unwrap();
        assert_eq!(
            forgot,
            Settings {
                language: "hr".into(),
                last_library_id: 2,
                ..Settings::default()
            },
            "library ids are not given out again"
        );
        assert!(creds.accounts().is_empty(), "{:?}", creds.accounts());
        assert_eq!(Settings::load(&path).unwrap(), forgot);
    }

    #[test]
    fn play_is_never_silent() {
        use PlayAction::*;
        // Something loaded (playing, paused or cued): the player toggles/resumes.
        assert_eq!(play_action(true, false, true, true), Player);
        // Waiting for the share to resume: remembered, the resume then plays.
        assert_eq!(play_action(false, true, true, false), RememberForResume);
        // It waits on an offline library while another one answers: the press
        // moves on (the saved position is given up), never silent.
        assert_eq!(play_action(false, true, true, true), PlayWithoutResume);
        // Nothing loaded (the playlist ran out or stopped): play it from what is due.
        assert_eq!(play_action(false, false, true, true), StartPlaylist);
        // Nothing to play at all: the player is asked anyway (it may re-try).
        assert_eq!(play_action(false, false, false, true), Player);
    }

    use crate::connection::ConnStatus as Status;
    use crate::state::{resume_with, start_with};

    struct NoTracks;
    impl smp_core::audio::player::TrackProvider for NoTracks {
        fn next_track(&mut self) -> Option<smp_core::audio::player::PlayItem> {
            None
        }
        fn on_failed(&mut self, _: TrackId, _: &smp_core::failure::Failure) {}
        fn on_played(&mut self, _: TrackId, _: Option<f32>) {}
    }

    fn kind(c: &Command) -> &'static str {
        match c {
            Command::Play(_) => "play",
            Command::Cue(_) => "cue",
            Command::TogglePause => "toggle",
            Command::Pause => "pause",
            Command::Resume => "resume",
            _ => "other",
        }
    }

    #[test]
    fn a_press_while_a_track_loads_goes_to_the_player() {
        let gate = Mutex::new(ResumeGate::default());
        let engine = Mutex::new(Engine::Empty);
        start_with(&gate, &engine, || {});
        let out = std::cell::RefCell::new(Vec::new());
        press_play_pause(
            &gate,
            &engine,
            || true,
            || false,
            || unreachable!("no resume to give up"),
            Command::TogglePause,
            |c| {
                assert!(gate.try_lock().is_err(), "sent under the gate");
                out.borrow_mut().push(kind(&c));
            },
            || out.borrow_mut().push("start"),
        );
        assert_eq!(
            *out.borrow(),
            ["toggle"],
            "pauses the loading track, does not reopen it"
        );
        // Nothing loaded, something due: the playlist starts.
        let engine = Mutex::new(Engine::Empty);
        let out = std::cell::RefCell::new(Vec::new());
        press_play_pause(
            &gate,
            &engine,
            || true,
            || false,
            || unreachable!("no resume to give up"),
            Command::TogglePause,
            |c| out.borrow_mut().push(kind(&c)),
            || out.borrow_mut().push("start"),
        );
        assert_eq!(*out.borrow(), ["start"]);
    }

    #[test]
    fn a_play_press_moves_on_while_the_resume_waits_on_an_offline_library() {
        // The resume waits for the main server (VPN not up yet); a USB library
        // answers. Pressing Play gives up the saved position and plays.
        let gate = Mutex::new(ResumeGate::waiting());
        let engine = Mutex::new(Engine::Empty);
        let out = std::cell::RefCell::new(Vec::new());
        press_play_pause(
            &gate,
            &engine,
            || true,
            || true,
            || out.borrow_mut().push("give up the resume"),
            Command::Resume,
            |c| out.borrow_mut().push(kind(&c)),
            || start_with(&gate, &engine, || out.borrow_mut().push("start")),
        );
        assert_eq!(*out.borrow(), ["give up the resume", "start"]);
        assert!(
            !lock(&gate).pending,
            "the resume does not come later on top"
        );
        // No library answers: the press is remembered for the resume, as before.
        let gate = Mutex::new(ResumeGate::waiting());
        let engine = Mutex::new(Engine::Empty);
        let out = std::cell::RefCell::new(Vec::new());
        press_play_pause(
            &gate,
            &engine,
            || true,
            || false,
            || out.borrow_mut().push("give up the resume"),
            Command::Resume,
            |c| out.borrow_mut().push(kind(&c)),
            || out.borrow_mut().push("start"),
        );
        assert!(out.borrow().is_empty());
        assert!(lock(&gate).play_requested);
    }

    #[test]
    fn a_press_racing_the_resume_is_never_lost() {
        for _ in 0..500 {
            let gate = Mutex::new(ResumeGate::waiting());
            let engine = Mutex::new(Engine::Empty);
            let log = Mutex::new(Vec::new());
            std::thread::scope(|s| {
                s.spawn(|| {
                    resume_with(
                        &gate,
                        &engine,
                        Status::Ok,
                        |pressed| {
                            Some(if pressed {
                                Command::Play(Box::new(NoTracks))
                            } else {
                                Command::Cue(Box::new(NoTracks))
                            })
                        },
                        |c| log.lock().unwrap().push(kind(&c)),
                    );
                });
                s.spawn(|| {
                    press_play_pause(
                        &gate,
                        &engine,
                        || true,
                        || false,
                        || unreachable!("no other library"),
                        Command::Resume,
                        |c| log.lock().unwrap().push(kind(&c)),
                        || start_with(&gate, &engine, || log.lock().unwrap().push("start")),
                    );
                });
            });
            let log = log.into_inner().unwrap();
            // The press is taken by the resume (it plays), or it comes after the
            // cue and goes to the player (which starts the cued track): never a
            // cue as the last word.
            assert!(log == ["play"] || log == ["cue", "resume"], "{log:?}");
        }
    }

    #[test]
    fn export_hint_points_at_the_music_folder() {
        let s = two_servers();
        assert_eq!(music_folder(&s.primary), "F/MP3/");
        let bare = ServerConfig {
            root: "".into(),
            ..s.primary.clone()
        };
        assert_eq!(music_folder(&bare), "F/");
        let mounted = |dirs: &'static [&'static str]| move |d: &str| dirs.contains(&d);
        assert_eq!(
            mounted_music_folder(&s.primary, mounted(&["/Volumes/F", "/Volumes/F/MP3"])),
            Some("/Volumes/F/MP3".into())
        );
        assert_eq!(
            mounted_music_folder(&s.primary, mounted(&["/Volumes/F"])),
            Some("/Volumes/F".into())
        );
        assert_eq!(mounted_music_folder(&s.primary, mounted(&[])), None);
    }

    #[test]
    fn add_at_reads_next_and_end() {
        assert_eq!(
            serde_json::from_str::<AddAt>(r#""next""#).unwrap(),
            AddAt::Next
        );
        assert_eq!(
            serde_json::from_str::<AddAt>(r#""end""#).unwrap(),
            AddAt::End
        );
        let f: QueueFilter =
            serde_json::from_str(r#"{"text":"x","years":[1990,1999],"unknown_year":true}"#)
                .unwrap();
        assert_eq!(f.years, Some((1990, 1999)));
        assert!(f.genres.is_empty());
    }

    #[test]
    fn merge_target_must_be_a_known_genre() {
        assert_eq!(merge_target("en", " dnb ").unwrap(), "Drum & bass");
        assert_eq!(merge_target("en", "chillout").unwrap(), "Chillout");
        assert_eq!(merge_target("en", "  ").unwrap_err(), "Choose a genre.");
        assert_eq!(
            merge_target("hr", "Foo").unwrap_err(),
            "\u{201e}Foo\u{201c} nije poznat \u{17e}anr."
        );
        assert_eq!(
            merge_target("en", "ZZ Test Preimenovano").unwrap_err(),
            "\u{201c}ZZ Test Preimenovano\u{201d} is not a known genre."
        );
    }

    #[test]
    fn picking_the_saved_device_again_is_sent_to_the_player() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("s.json"), Settings::default()).unwrap();
        let mut sent = Vec::new();
        switch_device(&store, Some("DAC".into()), |d| sent.push(d));
        // After an unplug fallback this is the way back: the player decides.
        switch_device(&store, Some("DAC".into()), |d| sent.push(d));
        let s = switch_device(&store, None, |d| sent.push(d));
        assert_eq!(
            sent,
            [Some("DAC".to_string()), Some("DAC".to_string()), None]
        );
        assert_eq!(s.device, None);
    }

    fn library(id: LibraryId, source: LibrarySource) -> Library {
        Library {
            id,
            name: String::new(),
            enabled: true,
            source,
        }
    }

    fn local(path: &str) -> LibrarySource {
        LibrarySource::Local { path: path.into() }
    }

    #[test]
    fn unreadable_paths_show_their_library_location() {
        let s = Settings {
            libraries: vec![
                library(2, local("/Volumes/USB Disk/")),
                library(
                    3,
                    LibrarySource::Smb {
                        primary: ServerConfig {
                            host: "nas".into(),
                            share: "Music".into(),
                            root: "".into(),
                            username: "u".into(),
                            domain: None,
                        },
                        secondary: None,
                    },
                ),
            ],
            ..two_servers()
        };
        let l = file_location(&s, 2, None, "Rock/A b čž#1.mp3");
        assert_eq!(l.path, "/Volumes/USB Disk/Rock/A b čž#1.mp3");
        assert_eq!(
            l.url,
            "file:///Volumes/USB%20Disk/Rock/A%20b%20%C4%8D%C5%BE%231.mp3"
        );
        let l = file_location(&s, 3, Some(Which::Secondary), "x.mp3");
        assert_eq!(l.url, "smb://nas/Music/x.mp3");
        assert_eq!(
            file_location(&s, 1, None, "x.mp3").url,
            "smb://10.111.100.2/F/MP3/x.mp3"
        );
        // A library removed meanwhile: just the path.
        assert_eq!(file_location(&s, 9, None, "x.mp3").path, "x.mp3");
        // Windows folders.
        let w = Settings {
            libraries: vec![library(2, local(r"D:\Glazba\"))],
            ..Settings::default()
        };
        let l = file_location(&w, 2, None, "Rock/a b.mp3");
        assert_eq!(l.path, r"D:\Glazba\Rock\a b.mp3");
        assert_eq!(l.url, "file:///D:/Glazba/Rock/a%20b.mp3");
    }

    #[test]
    fn a_password_is_needed_for_a_new_account_or_share() {
        let saved = ServerConfig {
            username: "music".into(),
            ..ServerConfig::default()
        };
        assert!(password_needed(None, &saved));
        assert!(!password_needed(Some(&saved), &saved));
        let root = ServerConfig {
            root: "Other".into(),
            ..saved.clone()
        };
        assert!(
            !password_needed(Some(&saved), &root),
            "only the folder changed"
        );
        for changed in [
            ServerConfig {
                host: "h2".into(),
                ..saved.clone()
            },
            ServerConfig {
                share: "G".into(),
                ..saved.clone()
            },
            ServerConfig {
                username: " other".into(),
                ..saved.clone()
            },
        ] {
            assert!(password_needed(Some(&saved), &changed), "{changed:?}");
        }
        let no_user = ServerConfig::default();
        assert!(password_needed(Some(&no_user), &saved));
    }

    #[test]
    fn saving_a_library_adds_or_changes_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = Settings::default();
        // A new local folder gets the next id.
        let (s, lib, changed) = save_library_settings(
            &base,
            Library {
                id: 0,
                name: " Disk ".into(),
                ..library(0, local(&dir.path().display().to_string()))
            },
            (false, false),
        )
        .unwrap();
        assert_eq!((lib.id, lib.name.as_str()), (2, "Disk"));
        assert!(changed);
        assert_eq!(s.libraries, std::slice::from_ref(&lib));
        // Renamed and switched off: the source is the same.
        let (s2, _, changed) = save_library_settings(
            &s,
            Library {
                name: "USB".into(),
                enabled: false,
                ..lib.clone()
            },
            (false, false),
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(s2.libraries[0].name, "USB");
        assert_eq!(s2.disabled_libraries(), [2]);
        // Another folder: a source change.
        let (_, _, changed) =
            save_library_settings(&s2, library(2, local("/elsewhere")), (false, false)).unwrap();
        assert!(changed);
        // The first library's name and switch.
        let (s3, first, changed) = save_library_settings(
            &s2,
            Library {
                id: 1,
                name: "NAS".into(),
                enabled: false,
                ..s2.all_libraries()[0].clone()
            },
            (false, false),
        )
        .unwrap();
        assert!(!changed);
        assert_eq!((first.id, s3.default_library_name.as_str()), (1, "NAS"));
        assert!(!s3.default_library_enabled);
        assert_eq!(s3.libraries, s2.libraries);
    }

    #[test]
    fn a_library_must_be_complete_to_be_saved() {
        let base = Settings::default();
        let err = |l| save_library_settings(&base, l, (false, false)).unwrap_err();
        assert_eq!(
            err(library(0, local("  "))),
            i18n::tr("en", "err.missing_folder")
        );
        let smb = |host: &str, user: &str| LibrarySource::Smb {
            primary: ServerConfig {
                host: host.into(),
                username: user.into(),
                ..ServerConfig::default()
            },
            secondary: None,
        };
        assert_eq!(
            err(library(0, smb("", "u"))),
            i18n::tr("en", "err.missing_host")
        );
        assert_eq!(
            err(library(0, smb("h", ""))),
            i18n::tr("en", "err.missing_user")
        );
        // A new SMB library needs a password typed.
        assert_eq!(
            err(library(0, smb("h", "u"))),
            i18n::tr("en", "err.missing_password")
        );
        assert!(save_library_settings(&base, library(0, smb("h", "u")), (true, false)).is_ok());
        // Another library's id is not taken over.
        assert_eq!(
            err(library(7, local("/x"))),
            i18n::tr("en", "err.no_library")
        );
        // The first library stays an SMB server.
        assert_eq!(
            err(library(1, local("/x"))),
            i18n::tr("en", "err.no_library")
        );
    }

    #[test]
    fn an_old_server_keeps_its_password_until_the_settings_are_saved() {
        let creds = MemoryStore::default();
        let smb = |host: &str| LibrarySource::Smb {
            primary: ServerConfig {
                host: host.into(),
                username: "u".into(),
                ..ServerConfig::default()
            },
            secondary: None,
        };
        let before = library(2, smb("old"));
        creds.put(2, "old", "u", "old-pw");
        let after = library(2, smb("new"));
        let failed: CmdResult<()> = store_library(
            &creds,
            Some(&before),
            &after,
            (Some("new-pw"), None),
            || Err("disk full".into()),
        );
        assert!(failed.is_err());
        assert!(
            creds.accounts().contains(&"library-2/old/u".to_string()),
            "the saved settings still name the old server: its password stays"
        );
        store_library(
            &creds,
            Some(&before),
            &after,
            (Some("new-pw"), None),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(creds.accounts(), ["library-2/new/u"]);
    }
}
