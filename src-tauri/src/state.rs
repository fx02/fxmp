//! Application state shared by commands and background threads, plus the background
//! work itself: player events, the connection monitor and catalog scans.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use smp_core::audio::output::CpalSinkFactory;
use smp_core::audio::player::{Command, Event, Player, PlayerConfig};
use smp_core::catalog::Catalog;
use smp_core::scan::{scan_with, ScanOptions, ScanProgress};
use smp_core::source::{MusicSource, Which};
use smp_core::TrackId;
use tauri::{AppHandle, Emitter, Manager};

use crate::connection::{monitor, AppSource, ConnStatus, Servers, StatusPayload};
use crate::credentials::{CredentialStore, Keychain};
use crate::provider::BannedSet;
use crate::settings::{self, Settings};

/// How often the UI is told to reload genres while a scan runs.
const GENRES_REFRESH: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NowPlaying {
    pub id: TrackId,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub genre_display: String,
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ScanProgressPayload {
    seen: usize,
    read: usize,
    failed: usize,
    current: String,
}

impl From<&ScanProgress> for ScanProgressPayload {
    fn from(p: &ScanProgress) -> Self {
        Self {
            seen: p.seen,
            read: p.read,
            failed: p.failed,
            current: p.current.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
struct ScanDonePayload {
    ok: bool,
    cancelled: bool,
    error: Option<String>,
    total: usize,
    read: usize,
    unchanged: usize,
    failed: usize,
    removed: usize,
}

#[derive(Debug, Clone, Serialize)]
struct TrackSkippedPayload {
    id: TrackId,
    reason: String,
}

pub struct AppState {
    pub settings_path: PathBuf,
    pub catalog_path: PathBuf,
    pub settings: Mutex<Settings>,
    pub catalog: Arc<Mutex<Catalog>>,
    pub banned: BannedSet,
    pub player: Player,
    pub source: Arc<AppSource>,
    pub creds: Arc<dyn CredentialStore>,
    pub status: Mutex<Option<ConnStatus>>,
    wake_monitor: Mutex<Sender<()>>,
    pub now_playing: Mutex<Option<NowPlaying>>,
    scanning: AtomicBool,
    cancel_scan: Arc<AtomicBool>,
}

/// Locks a mutex, carrying on after a panic elsewhere (all state here stays valid).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AppState {
    /// Opens settings, catalog and player and starts the background threads.
    pub fn init(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
        let settings_path = app.path().app_config_dir()?.join(settings::FILE_NAME);
        let data_dir = app.path().app_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;
        let catalog_path = data_dir.join("catalog.db");

        let loaded = Settings::load(&settings_path);
        // Saved settings with a username: the user has set up the server.
        let configured = loaded
            .as_ref()
            .is_some_and(|s| !s.primary.username.trim().is_empty());
        let settings = loaded.unwrap_or_default();
        let catalog = Catalog::open(&catalog_path)?;
        let catalog_empty = catalog.is_empty()?;
        let creds: Arc<dyn CredentialStore> = Arc::new(Keychain);

        let source = Arc::new(AppSource::default());
        if configured {
            source.set(Some(Servers::from_settings(&settings, creds.as_ref())));
        }

        let (events_tx, events_rx) = mpsc::channel();
        let player = Player::spawn(
            source.clone() as Arc<dyn MusicSource>,
            Box::new(CpalSinkFactory),
            events_tx,
            PlayerConfig::default(),
        );
        apply_player_settings(&player, &settings);

        let (wake_tx, wake_rx) = mpsc::channel();
        app.manage(AppState {
            settings_path,
            catalog_path,
            settings: Mutex::new(settings),
            catalog: Arc::new(Mutex::new(catalog)),
            banned: BannedSet::default(),
            player,
            source: source.clone(),
            creds,
            status: Mutex::new(None),
            wake_monitor: Mutex::new(wake_tx),
            now_playing: Mutex::new(None),
            scanning: AtomicBool::new(false),
            cancel_scan: Arc::new(AtomicBool::new(false)),
        });

        let handle = app.clone();
        std::thread::Builder::new()
            .name("player-events".into())
            .spawn(move || {
                for ev in events_rx {
                    on_player_event(&handle, ev);
                }
            })?;

        let handle = app.clone();
        std::thread::Builder::new()
            .name("connection".into())
            .spawn(move || {
                monitor(source, wake_rx, |status| {
                    log::info!("connection status: {status:?}");
                    *lock(&handle.state::<AppState>().status) = Some(status);
                    let _ = handle.emit("connection-status", StatusPayload::from(status));
                })
            })?;

        if configured && catalog_empty {
            log::info!("catalog is empty: starting the first scan");
            if let Err(e) = start_scan(app) {
                log::warn!("cannot start scan: {e}");
            }
        }
        Ok(())
    }

    pub fn wake_monitor(&self) {
        let _ = lock(&self.wake_monitor).send(());
    }

    /// Replaces the servers after the settings changed and re-checks the connection.
    pub fn reconnect(&self, settings: &Settings) {
        self.source
            .set(Some(Servers::from_settings(settings, self.creds.as_ref())));
        self.wake_monitor();
    }

    /// Changes the settings in memory and on disk.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
        let mut s = lock(&self.settings);
        f(&mut s);
        *s = s.clone().clamped();
        s.save(&self.settings_path)
            .map_err(|e| format!("Postavke nije moguće spremiti: {e}"))?;
        Ok(s.clone())
    }
}

pub fn apply_player_settings(player: &Player, s: &Settings) {
    player.send(Command::SetVolume(s.volume));
    if s.device.is_some() {
        player.send(Command::SetDevice(s.device.clone()));
    }
    player.send(Command::SetCrossfade {
        enabled: s.crossfade_enabled,
        secs: s.crossfade_secs,
    });
    player.send(Command::SetNormalize(s.normalize));
}

fn now_playing_of(catalog: &Catalog, id: TrackId, paused: bool) -> smp_core::Result<NowPlaying> {
    let row = catalog.track(id)?;
    let title = row
        .meta
        .title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            let name = row.rel_path.rsplit('/').next().unwrap_or(&row.rel_path);
            name.rsplit_once('.')
                .map_or(name, |(stem, _)| stem)
                .to_string()
        });
    Ok(NowPlaying {
        id,
        artist: row.meta.artist.unwrap_or_default(),
        title,
        album: row.meta.album.unwrap_or_default(),
        genre_display: catalog.track_genres(id)?.join(", "),
        paused,
    })
}

fn emit_now_playing(app: &AppHandle, np: &Option<NowPlaying>) {
    let _ = app.emit("now-playing", np);
}

fn on_player_event(app: &AppHandle, ev: Event) {
    let st = app.state::<AppState>();
    match ev {
        Event::TrackStarted { id } => {
            let paused = lock(&st.now_playing).as_ref().is_some_and(|n| n.paused);
            let np = match now_playing_of(&lock(&st.catalog), id, paused) {
                Ok(np) => Some(np),
                Err(e) => {
                    log::warn!("track {id} not in catalog: {e}");
                    None
                }
            };
            *lock(&st.now_playing) = np.clone();
            emit_now_playing(app, &np);
            // Playback moved between servers: let the monitor confirm and report it.
            if let Some(servers) = st.source.servers() {
                let expected = match servers.failover.active() {
                    Which::Primary => ConnStatus::Ok,
                    Which::Secondary => ConnStatus::Secondary,
                };
                if *lock(&st.status) != Some(expected) {
                    st.wake_monitor();
                }
            }
        }
        Event::TrackSkipped { id, reason } => {
            let _ = app.emit("track-skipped", TrackSkippedPayload { id, reason });
            // Possibly the network: re-check the connection.
            st.wake_monitor();
        }
        Event::Paused(paused) => {
            let np = {
                let mut np = lock(&st.now_playing);
                if let Some(n) = np.as_mut() {
                    n.paused = paused;
                }
                np.clone()
            };
            emit_now_playing(app, &np);
        }
        Event::DeviceChanged(name) => log::info!("output device: {name}"),
        Event::Idle => {
            *lock(&st.now_playing) = None;
            emit_now_playing(app, &None);
        }
    }
}

/// Starts a scan on its own thread with its own catalog connection.
pub fn start_scan(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    if st.source.servers().is_none() {
        return Err("Najprije spremi postavke servera.".into());
    }
    if st.scanning.swap(true, Ordering::SeqCst) {
        return Err("Skeniranje je već u tijeku.".into());
    }
    st.cancel_scan.store(false, Ordering::SeqCst);
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || run_scan(&handle));
    if let Err(e) = spawned {
        st.scanning.store(false, Ordering::SeqCst);
        return Err(format!("Skeniranje nije moguće pokrenuti: {e}"));
    }
    Ok(())
}

pub fn cancel_scan(st: &AppState) {
    st.cancel_scan.store(true, Ordering::SeqCst);
}

fn run_scan(app: &AppHandle) {
    let st = app.state::<AppState>();
    let started = Instant::now();
    let _ = app.emit(
        "scan-progress",
        ScanProgressPayload::from(&ScanProgress::default()),
    );
    let mut last_genres = Instant::now();
    let mut progress = |p: &ScanProgress| {
        let _ = app.emit("scan-progress", ScanProgressPayload::from(p));
        if last_genres.elapsed() >= GENRES_REFRESH {
            last_genres = Instant::now();
            let _ = app.emit("genres-changed", ());
        }
    };
    let result = Catalog::open(&st.catalog_path).and_then(|mut catalog| {
        scan_with(
            st.source.as_ref(),
            &mut catalog,
            &mut progress,
            &st.cancel_scan,
            &ScanOptions::default(),
        )
    });
    let cancelled = st.cancel_scan.load(Ordering::SeqCst);
    let payload = match result {
        Ok(r) => {
            log::info!(
                "scan finished in {:.1} s: {} files, {} read, {} unchanged, {} failed, {} removed{}",
                started.elapsed().as_secs_f32(),
                r.total,
                r.read,
                r.unchanged,
                r.failed,
                r.removed,
                if cancelled { " (cancelled)" } else { "" }
            );
            ScanDonePayload {
                ok: true,
                cancelled,
                error: None,
                total: r.total,
                read: r.read,
                unchanged: r.unchanged,
                failed: r.failed,
                removed: r.removed,
            }
        }
        Err(e) => {
            log::warn!("scan failed: {e}");
            st.wake_monitor();
            let error = match &e {
                smp_core::Error::Source(_) => ConnStatus::Unreachable.message().to_string(),
                other => other.to_string(),
            };
            ScanDonePayload {
                cancelled,
                error: Some(error),
                ..Default::default()
            }
        }
    };
    st.scanning.store(false, Ordering::SeqCst);
    let _ = app.emit("genres-changed", ());
    let _ = app.emit("scan-done", payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use smp_core::catalog::TrackMeta;

    #[test]
    fn now_playing_falls_back_to_file_name_and_lists_genres() {
        let c = Catalog::open_in_memory().unwrap();
        let meta = TrackMeta {
            artist: Some("Artist".into()),
            raw_genre: Some("Trance; Chillout".into()),
            ..Default::default()
        };
        let id = c.upsert_track("x/Some Song.mp3", 1, 1, &meta).unwrap();
        c.rebuild_genres().unwrap();
        let np = now_playing_of(&c, id, true).unwrap();
        assert_eq!(np.title, "Some Song");
        assert_eq!(np.artist, "Artist");
        assert_eq!(np.album, "");
        assert_eq!(np.genre_display, "Chillout, Trance");
        assert!(np.paused);
    }
}
