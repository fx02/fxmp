//! Tauri commands. Every command body runs on its own plain thread: Tauri runs sync
//! commands on the main thread and async ones on its tokio runtime, and neither may
//! block on SMB, the keychain or long catalog queries.

use serde::Serialize;
use smp_core::audio::output::list_output_devices;
use smp_core::audio::player::Command;
use smp_core::TrackId;
use tauri::{AppHandle, Emitter, Manager};

use crate::connection::{smb_config, status_of, test_server, ConnStatus, StatusPayload};
use crate::provider::CatalogProvider;
use crate::settings::Settings;
use crate::state::{self, apply_player_settings, lock, AppState, NowPlaying};

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
    rx.await.map_err(|_| "Naredba nije dovršena.".to_string())?
}

fn db_err(e: smp_core::Error) -> String {
    log::warn!("catalog: {e}");
    format!("Greška baze: {e}")
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
    run(app, |_, st| Ok(lock(&st.settings).clone())).await
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
    run(app, move |_, st| {
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
        let saved = st.update_settings(|s| *s = settings)?;
        apply_player_settings(&st.player, &saved);
        st.reconnect(&saved);
        Ok(())
    })
    .await
}

/// Connects once to the saved primary or secondary server: "ok", "auth" or "unreachable".
#[tauri::command(rename_all = "snake_case")]
pub async fn test_connection(app: AppHandle, which: String) -> CmdResult<ConnStatus> {
    run(app, move |_, st| {
        let settings = lock(&st.settings).clone();
        let server = match which.as_str() {
            "primary" => settings.primary,
            "secondary" => settings
                .secondary
                .ok_or("Rezervni server nije postavljen.")?,
            other => return Err(format!("Nepoznat server: {other}")),
        };
        let password = st
            .creds
            .get(server.host.trim(), server.username.trim())
            .unwrap_or_default();
        Ok(status_of(test_server(smb_config(&server, password)), None))
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
        state::cancel_scan(st);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_genres(app: AppHandle) -> CmdResult<Vec<Genre>> {
    run(app, |_, st| {
        let genres = lock(&st.catalog).genres().map_err(db_err)?;
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
#[tauri::command(rename_all = "snake_case")]
pub async fn play_genres(app: AppHandle, keys: Vec<String>) -> CmdResult<()> {
    run(app, move |_, st| {
        let provider = CatalogProvider::shuffle(st.catalog.clone(), st.banned.clone(), &keys)
            .map_err(db_err)?;
        st.player.send(Command::Play(Box::new(provider)));
        st.update_settings(|s| s.last_selection = keys)?;
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn play_album(app: AppHandle, artist: String, album: String) -> CmdResult<()> {
    run(app, move |_, st| {
        let provider =
            CatalogProvider::album(st.catalog.clone(), st.banned.clone(), &artist, &album)
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
        let s = st.update_settings(|s| s.volume = v)?;
        st.player.send(Command::SetVolume(s.volume));
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
        st.update_settings(|s| s.device = name.clone())?;
        st.player.send(Command::SetDevice(name));
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_crossfade(app: AppHandle, enabled: bool, secs: u8) -> CmdResult<()> {
    run(app, move |_, st| {
        let s = st.update_settings(|s| {
            s.crossfade_enabled = enabled;
            s.crossfade_secs = secs;
        })?;
        st.player.send(Command::SetCrossfade {
            enabled: s.crossfade_enabled,
            secs: s.crossfade_secs,
        });
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_normalize(app: AppHandle, b: bool) -> CmdResult<()> {
    run(app, move |_, st| {
        st.update_settings(|s| s.normalize = b)?;
        st.player.send(Command::SetNormalize(b));
        Ok(())
    })
    .await
}

/// Bans the playing track for good and skips to the next one.
#[tauri::command(rename_all = "snake_case")]
pub async fn ban_current(app: AppHandle) -> CmdResult<()> {
    run(app, |_, st| {
        let id = lock(&st.now_playing)
            .as_ref()
            .map(|n| n.id)
            .ok_or("Ništa ne svira.")?;
        lock(&st.catalog).set_banned(id, true).map_err(db_err)?;
        lock(&st.banned).insert(id);
        st.player.send(Command::Next);
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_banned(app: AppHandle) -> CmdResult<Vec<BannedTrack>> {
    run(app, |_, st| {
        let rows = lock(&st.catalog).banned().map_err(db_err)?;
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
        lock(&st.catalog).set_banned(id, false).map_err(db_err)?;
        lock(&st.banned).remove(&id);
        Ok(())
    })
    .await
}

/// Merges (or renames) genre `from_key` into `into_display`; emits `genres-changed`.
#[tauri::command(rename_all = "snake_case")]
pub async fn merge_genre(app: AppHandle, from_key: String, into_display: String) -> CmdResult<()> {
    run(app, move |app, st| {
        let into = into_display.trim();
        if into.is_empty() {
            return Err("Naziv žanra ne smije biti prazan.".into());
        }
        lock(&st.catalog)
            .add_genre_rule(&from_key, into)
            .map_err(db_err)?;
        let _ = app.emit("genres-changed", ());
        Ok(())
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_artists(app: AppHandle) -> CmdResult<Vec<String>> {
    run(app, |_, st| lock(&st.catalog).artists().map_err(db_err)).await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_albums(app: AppHandle, artist: String) -> CmdResult<Vec<String>> {
    run(app, move |_, st| {
        lock(&st.catalog).albums(&artist).map_err(db_err)
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub async fn now_playing(app: AppHandle) -> CmdResult<Option<NowPlaying>> {
    run(app, |_, st| Ok(lock(&st.now_playing).clone())).await
}
