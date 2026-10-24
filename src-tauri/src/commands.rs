//! Tauri commands. Every command body runs on its own plain thread: Tauri runs sync
//! commands on the main thread and async ones on its tokio runtime, and neither may
//! block on SMB, the keychain or long catalog queries.

use serde::Serialize;
use smp_core::audio::output::list_output_devices;
use smp_core::audio::player::Command;
use smp_core::genre::{known_genre, known_genre_names};
use smp_core::TrackId;
use tauri::{AppHandle, Emitter, Manager};

use crate::autostart;
use crate::connection::{status_of, test_server, test_target, ConnStatus, StatusPayload};
use crate::provider::CatalogProvider;
use crate::settings::{ServerConfig, Settings};
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
    rx.await
        .map_err(|_| "The command did not finish.".to_string())?
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
    format!("Library database error: {e}")
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
    pub rel_path: String,
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
            .map_err(|e| format!("Cannot save settings: {e}"))?;
        if servers_changed {
            state::cancel_scan(
                st,
                Some("The server settings changed, so the scan was cancelled."),
            );
        }
        st.reconnect(&saved);
        autostart::apply(app, saved.autostart, false).map_err(|e| {
            log::warn!("start at login: {e}");
            "Settings saved, but start at login could not be changed.".to_string()
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
        .ok_or("Nothing is playing.")?;
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

/// The known genre a merge goes into, by its canonical name.
fn merge_target(into: &str) -> CmdResult<String> {
    let into = into.trim();
    if into.is_empty() {
        return Err("Choose a genre.".into());
    }
    known_genre(into)
        .map(|g| g.display)
        .ok_or_else(|| format!("\u{201c}{into}\u{201d} is not a known genre."))
}

/// Display names of all known genres: the choices for "Merge into…".
#[tauri::command(rename_all = "snake_case")]
pub fn known_genres() -> Vec<String> {
    known_genre_names()
}

/// Merges genre `from_key` into the known genre `into_display`; emits `genres-changed`.
#[tauri::command(rename_all = "snake_case")]
pub async fn merge_genre(app: AppHandle, from_key: String, into_display: String) -> CmdResult<()> {
    run(app, move |app, st| {
        let into = merge_target(&into_display)?;
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
pub async fn list_albums(app: AppHandle, artist: String) -> CmdResult<Vec<String>> {
    run(app, move |_, st| {
        lock(&st.query).albums(&artist).map_err(db_err)
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn now_playing(app: AppHandle) -> CmdResult<Option<NowPlaying>> {
    run(app, |_, st| Ok(lock(&st.now_playing).clone())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_target_must_be_a_known_genre() {
        assert_eq!(merge_target(" dnb ").unwrap(), "Drum & bass");
        assert_eq!(merge_target("chillout").unwrap(), "Chillout");
        assert_eq!(merge_target("  ").unwrap_err(), "Choose a genre.");
        assert_eq!(
            merge_target("ZZ Test Preimenovano").unwrap_err(),
            "\u{201c}ZZ Test Preimenovano\u{201d} is not a known genre."
        );
    }
}
