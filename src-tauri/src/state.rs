//! Application state shared by commands and background threads, plus the background
//! work itself: player events, the connection monitor and catalog scans.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use smp_core::audio::output::CpalSinkFactory;
use smp_core::audio::player::{Command, Event, Player, PlayerConfig};
use smp_core::catalog::Catalog;
use smp_core::scan::{scan_with, ScanOptions, ScanProgress};
use smp_core::source::{MusicSource, SourceError, Which};
use smp_core::TrackId;
use tauri::{AppHandle, Emitter, Listener, Manager};

use crate::connection::{monitor, AppSource, ConnStatus, Servers, StatusPayload};
use crate::credentials::{CredentialStore, Keychain};
use crate::provider::{BannedSet, Playback};
use crate::settings::{self, Settings, SettingsStore};
use crate::writer::CatalogWriter;

/// How often the UI is told to reload genres while a scan runs.
const GENRES_REFRESH: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowPlaying {
    pub id: TrackId,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub genre_display: String,
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgressPayload {
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
    /// Why the app cancelled the scan (none when the user did).
    cancel_reason: Option<String>,
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

/// `get_scan_status` result.
#[derive(Debug, Clone, Serialize)]
pub struct ScanStatus {
    pub scanning: bool,
    pub progress: Option<ScanProgressPayload>,
}

pub struct AppState {
    pub catalog_path: PathBuf,
    pub settings: SettingsStore,
    /// Read connection for the playback path (the providers on the engine thread).
    pub catalog: Arc<Mutex<Catalog>>,
    /// Read connection for commands and UI queries.
    pub query: Mutex<Catalog>,
    /// Performs every catalog write.
    pub writer: CatalogWriter,
    pub banned: BannedSet,
    pub player: Player,
    pub source: Arc<AppSource>,
    pub creds: Arc<dyn CredentialStore>,
    pub status: Mutex<Option<ConnStatus>>,
    wake_monitor: Mutex<Sender<()>>,
    pub now_playing: Mutex<Option<NowPlaying>>,
    scanning: AtomicBool,
    cancel_scan: Arc<AtomicBool>,
    cancel_reason: Mutex<Option<String>>,
    scan_progress: Mutex<Option<ScanProgressPayload>>,
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
        let query = Catalog::open(&catalog_path)?;
        let catalog_empty = query.is_empty()?;
        let playback_db = Catalog::open(&catalog_path)?;
        let writer = CatalogWriter::spawn(Catalog::open(&catalog_path)?)?;
        let creds: Arc<dyn CredentialStore> = Arc::new(Keychain);

        // Servers are set once their passwords are read (see below).
        let source = Arc::new(AppSource::default());

        let (events_tx, events_rx) = mpsc::channel();
        let player = Player::spawn(
            source.clone() as Arc<dyn MusicSource>,
            Box::new(CpalSinkFactory),
            events_tx,
            PlayerConfig::default(),
        );
        send_mix_settings(&player, &settings);
        if settings.device.is_some() {
            player.send(Command::SetDevice(settings.device.clone()));
        }

        let (wake_tx, wake_rx) = mpsc::channel();
        app.manage(AppState {
            catalog_path,
            settings: SettingsStore::new(settings_path, settings)?,
            catalog: Arc::new(Mutex::new(playback_db)),
            query: Mutex::new(query),
            writer,
            banned: BannedSet::default(),
            player,
            source: source.clone(),
            creds,
            status: Mutex::new(None),
            wake_monitor: Mutex::new(wake_tx),
            now_playing: Mutex::new(None),
            scanning: AtomicBool::new(false),
            cancel_scan: Arc::new(AtomicBool::new(false)),
            cancel_reason: Mutex::new(None),
            scan_progress: Mutex::new(None),
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

        if configured {
            // The keychain may ask the user for access (e.g. after an update):
            // never on the main thread, so the window comes up meanwhile.
            let handle = app.clone();
            std::thread::Builder::new()
                .name("credentials".into())
                .spawn(move || {
                    let st = handle.state::<AppState>();
                    let servers = Servers::from_settings(&st.settings.get(), st.creds.as_ref());
                    if !st.source.set_if_unset(servers) {
                        return; // Settings were saved meanwhile.
                    }
                    st.wake_monitor();
                    if catalog_empty {
                        log::info!("catalog is empty: starting the first scan");
                        if let Err(e) = start_scan(&handle) {
                            log::warn!("cannot start scan: {e}");
                        }
                    }
                })?;
        }
        Ok(())
    }

    pub fn wake_monitor(&self) {
        let _ = lock(&self.wake_monitor).send(());
    }

    /// Replaces the servers (fresh connections, forgetting a rejected password) and
    /// re-checks the connection.
    pub fn reconnect(&self, settings: &Settings) {
        self.source
            .set(Some(Servers::from_settings(settings, self.creds.as_ref())));
        self.wake_monitor();
    }

    /// What a new track provider needs.
    pub fn playback(&self) -> Playback {
        Playback {
            db: self.catalog.clone(),
            writer: self.writer.clone(),
            banned: self.banned.clone(),
        }
    }

    pub fn scan_status(&self) -> ScanStatus {
        let scanning = self.scanning.load(Ordering::SeqCst);
        ScanStatus {
            scanning,
            progress: lock(&self.scan_progress).clone().filter(|_| scanning),
        }
    }
}

fn send_mix_settings(player: &Player, s: &Settings) {
    player.send(Command::SetVolume(s.volume));
    player.send(Command::SetCrossfade {
        enabled: s.crossfade_enabled,
        secs: s.crossfade_secs,
    });
    player.send(Command::SetNormalize(s.normalize));
}

/// Applies all player-related settings, including `device: None` (system default).
pub fn apply_player_settings(player: &Player, s: &Settings) {
    send_mix_settings(player, s);
    player.send(Command::SetDevice(s.device.clone()));
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

const NOW_PLAYING_EVENT: &str = "now-playing";

fn emit_now_playing(app: &AppHandle, np: &Option<NowPlaying>) {
    let _ = app.emit(NOW_PLAYING_EVENT, np);
}

/// Calls `f` with every `now-playing` update (on the thread that sent it).
pub fn on_now_playing(app: &AppHandle, f: impl Fn(Option<NowPlaying>) + Send + 'static) {
    app.listen_any(NOW_PLAYING_EVENT, move |e| {
        match serde_json::from_str::<Option<NowPlaying>>(e.payload()) {
            Ok(np) => f(np),
            Err(err) => log::warn!("bad {NOW_PLAYING_EVENT} payload: {err}"),
        }
    });
}

fn on_player_event(app: &AppHandle, ev: Event) {
    let st = app.state::<AppState>();
    match ev {
        Event::TrackStarted { id } => {
            let paused = lock(&st.now_playing).as_ref().is_some_and(|n| n.paused);
            let np = match now_playing_of(&lock(&st.query), id, paused) {
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
        return Err("Save the server settings first.".into());
    }
    if st.scanning.swap(true, Ordering::SeqCst) {
        return Err("A scan is already running.".into());
    }
    st.cancel_scan.store(false, Ordering::SeqCst);
    *lock(&st.cancel_reason) = None;
    *lock(&st.scan_progress) = Some(ScanProgressPayload::from(&ScanProgress::default()));
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || run_scan(&handle));
    if let Err(e) = spawned {
        st.scanning.store(false, Ordering::SeqCst);
        return Err(format!("Cannot start the scan: {e}"));
    }
    Ok(())
}

/// Cancels a running scan; `reason` is reported in `scan-done` when the app (not the
/// user) cancelled. Returns whether a scan was running.
pub fn cancel_scan(st: &AppState, reason: Option<&str>) -> bool {
    if !st.scanning.load(Ordering::SeqCst) {
        return false;
    }
    *lock(&st.cancel_reason) = reason.map(str::to_string);
    st.cancel_scan.store(true, Ordering::SeqCst);
    true
}

/// Text for a failed scan: connection problems in the words the status uses.
fn scan_error_message(e: &smp_core::Error) -> String {
    match e {
        smp_core::Error::Source(SourceError::Auth) => ConnStatus::Auth.message().to_string(),
        smp_core::Error::Source(_) => ConnStatus::Unreachable.message().to_string(),
        other => other.to_string(),
    }
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
        let payload = ScanProgressPayload::from(p);
        *lock(&st.scan_progress) = Some(payload.clone());
        let _ = app.emit("scan-progress", payload);
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
    let cancel_reason = lock(&st.cancel_reason).take().filter(|_| cancelled);
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
                cancel_reason,
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
            ScanDonePayload {
                cancelled,
                cancel_reason,
                error: Some(scan_error_message(&e)),
                ..Default::default()
            }
        }
    };
    st.scanning.store(false, Ordering::SeqCst);
    *lock(&st.scan_progress) = None;
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

    #[test]
    fn scan_errors_use_the_matching_status_text() {
        let auth = smp_core::Error::Source(SourceError::Auth);
        assert_eq!(scan_error_message(&auth), ConnStatus::Auth.message());
        for e in [SourceError::Unreachable, SourceError::Timeout] {
            let e = smp_core::Error::Source(e);
            assert_eq!(scan_error_message(&e), ConnStatus::Unreachable.message());
        }
        let other = smp_core::Error::Other("disk full".into());
        assert_eq!(scan_error_message(&other), "disk full");
    }
}
