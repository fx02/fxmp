//! Tauri commands. Every command body runs on its own plain thread: Tauri runs sync
//! commands on the main thread and async ones on its tokio runtime, and neither may
//! block on SMB, the keychain or long catalog queries.

use serde::Serialize;
use smp_core::audio::output::list_output_devices;
use smp_core::audio::player::Command;
use smp_core::genre::{known_genre, known_genre_names};
use smp_core::source::Which;
use smp_core::TrackId;
use tauri::{AppHandle, Emitter, Manager};

use crate::autostart;
use crate::connection::{status_of, test_server, test_target, ConnStatus, StatusPayload};
use crate::credentials::CredentialStore;
use crate::i18n;
use crate::provider::CatalogProvider;
use crate::settings::{ServerConfig, Settings, SettingsStore};
use crate::state::{self, apply_player_settings, lock, AppState, NowPlaying, ScanStatus};
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

/// Where a track lives: `smb://host/share/root/rel_path` on the server the failure
/// came from (the configured primary when unknown or no backup is set up). Never
/// holds a username or password, even one typed into the server field.
pub fn file_location(settings: &Settings, server: Option<Which>, rel_path: &str) -> FileLocation {
    let cfg = match (server, &settings.secondary) {
        (Some(Which::Secondary), Some(secondary)) => secondary,
        _ => &settings.primary,
    };
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
/// `forget_servers` the servers' keychain passwords are deleted first. `apply` runs
/// with the old and the new settings, as in [`SettingsStore::replace_and_save`].
pub fn reset_settings_now(
    store: &SettingsStore,
    creds: &dyn CredentialStore,
    forget_servers: bool,
    apply: impl FnOnce(&Settings, &Settings),
) -> CmdResult<Settings> {
    let old = store.get();
    if forget_servers {
        for server in std::iter::once(&old.primary).chain(old.secondary.as_ref()) {
            let username = server.username.trim();
            if !username.is_empty() {
                creds.delete(server.host.trim(), username)?;
            }
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
        let store = |server: Option<&crate::settings::ServerConfig>, pw: Option<String>| match (
            server,
            pw.filter(|p| !p.is_empty()),
        ) {
            (Some(s), Some(pw)) => st.creds.set(s.host.trim(), s.username.trim(), &pw),
            _ => Ok(()),
        };
        store(Some(&settings.primary), primary_password)?;
        store(settings.secondary.as_ref(), secondary_password)?;
        let mut servers_changed = false;
        let saved = st
            .settings
            .replace_and_save(settings, |old, new| {
                servers_changed = old.primary != new.primary || old.secondary != new.secondary;
                apply_player_settings(&st.player, new);
            })
            .map_err(|e| i18n::tf("err.save_settings", &[("error", &e.to_string())]))?;
        if servers_changed {
            state::cancel_scan(st, Some(&i18n::t("scan.cancelled_settings")));
        }
        st.reconnect(&saved);
        autostart::apply(app, saved.autostart, false).map_err(|e| {
            log::warn!("start at login: {e}");
            i18n::t("err.autostart")
        })
    })
    .await
}

/// Connects once to a server: "ok", "auth" or "unreachable". `server` and `password`
/// are the form's unsaved values; without them the saved settings and the keychain
/// password are used, and the live connection to that server forgets a rejected
/// password and is re-checked.
#[tauri::command(rename_all = "snake_case")]
pub async fn test_connection(
    app: AppHandle,
    which: String,
    server: Option<ServerConfig>,
    password: Option<String>,
) -> CmdResult<ConnStatus> {
    run(app, move |_, st| {
        let saved = server.is_none();
        let cfg = test_target(
            &st.settings.get(),
            st.creds.as_ref(),
            &which,
            server,
            password,
        )?;
        let status = status_of(test_server(cfg), None);
        if saved {
            if let Some(servers) = st.source.servers() {
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

#[tauri::command(rename_all = "snake_case")]
pub async fn get_connection_status(app: AppHandle) -> CmdResult<Option<StatusPayload>> {
    run(
        app,
        |_, st| Ok((*lock(&st.status)).map(StatusPayload::from)),
    )
    .await
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

/// Shuffles the selected genres (empty = everything) and remembers the selection.
pub fn play_genres_now(app: &AppHandle, st: &AppState, keys: Vec<String>) -> CmdResult<()> {
    let provider =
        CatalogProvider::shuffle(&lock(&st.query), st.playback(), &keys).map_err(db_err)?;
    st.player.send(Command::Play(Box::new(provider)));
    st.settings.update(|s| s.last_selection = keys, |_| {});
    // The tray's genre menu lists the last selection.
    tray::refresh_genres(app);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn play_genres(app: AppHandle, keys: Vec<String>) -> CmdResult<()> {
    run(app, move |app, st| play_genres_now(app, st, keys)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn play_album(app: AppHandle, artist: String, album: String) -> CmdResult<()> {
    run(app, move |_, st| {
        let provider = CatalogProvider::album(&lock(&st.query), st.playback(), &artist, &album)
            .map_err(db_err)?;
        st.player.send(Command::Play(Box::new(provider)));
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn toggle_pause(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| {
        st.player.send(Command::TogglePause);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn next(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| {
        st.player.send(Command::Next);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_volume(app: AppHandle, v: f32) -> CmdResult<()> {
    run(app, move |_, st| {
        st.settings.update(
            |s| s.volume = v,
            |s| st.player.send(Command::SetVolume(s.volume)),
        );
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
    run(app, move |_, st| {
        st.settings.update(
            |s| s.device = name,
            |s| st.player.send(Command::SetDevice(s.device.clone())),
        );
        Ok(())
    })
    .await
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

/// Bans the playing track for good and skips to the next one.
pub fn ban_current_now(st: &AppState) -> CmdResult<()> {
    let id = lock(&st.now_playing)
        .as_ref()
        .map(|n| n.id)
        .ok_or_else(|| i18n::t("err.nothing_playing"))?;
    lock(&st.banned).insert(id);
    st.writer
        .call(move |c| c.set_banned(id, true))
        .map_err(db_err)?;
    st.player.send(Command::Next);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub async fn ban_current(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| ban_current_now(st)).await
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
        let rows = lock(&st.query).unreadable().map_err(db_err)?;
        let settings = st.settings.get();
        Ok(rows
            .into_iter()
            .map(|t| {
                let loc = file_location(&settings, t.server, &t.rel_path);
                (t, loc)
            })
            .map(|(t, loc)| UnreadableRow {
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
                servers_changed = old.primary != new.primary || old.secondary != new.secondary;
                apply_player_settings(&st.player, new);
            },
        )?;
        log::info!("settings reset to defaults (servers forgotten: {forget_servers})");
        if servers_changed || forget_servers {
            state::cancel_scan(st, Some(&i18n::t("scan.cancelled_settings")));
            st.forget_or_reconnect(app, &new);
        }
        // The tray's genre menu lists the (now cleared) last selection.
        tray::refresh_genres(app);
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
        let loc = |s: &Settings, w, p| file_location(s, w, p);
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
        let l = file_location(&s, None, "Rock & Roll/A b#1 100%?ž.mp3");
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
        creds.set("10.111.100.2", "music", "pw1").unwrap();
        creds.set("10.111.0.2", "backup", "pw2").unwrap();
        let start = Settings {
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
        assert_eq!(creds.get("10.111.100.2", "music").as_deref(), Some("pw1"));

        let forgot = reset_settings_now(&store, &creds, true, |_, _| {}).unwrap();
        assert_eq!(
            forgot,
            Settings {
                language: "hr".into(),
                ..Settings::default()
            }
        );
        assert_eq!(creds.get("10.111.100.2", "music"), None);
        assert_eq!(creds.get("10.111.0.2", "backup"), None);
        assert_eq!(Settings::load(&path).unwrap(), forgot);
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
}
