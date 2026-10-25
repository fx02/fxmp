//! Application state shared by commands and background threads, plus the background
//! work itself: player events, the connection monitor and catalog scans.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use smp_core::audio::output::CpalSinkFactory;
use smp_core::audio::player::{Command, Event, Player, PlayerConfig, TrackProvider};
use smp_core::catalog::Catalog;
use smp_core::genre::{migrate_selection, UNKNOWN_DISPLAY};
use smp_core::scan::{scan_with, ScanOptions, ScanProgress};
use smp_core::source::{MusicSource, SourceError, Which};
use smp_core::TrackId;
use tauri::{AppHandle, Emitter, Listener, Manager};

use crate::connection::{monitor, AppSource, ConnStatus, Servers, StatusPayload};
use crate::credentials::{CredentialStore, Keychain};
use crate::i18n;
use crate::playlist::{self, SharedPlaylist};
use crate::provider::{BannedSet, CatalogProvider, Playback};
use crate::settings::{self, Settings, SettingsStore};
use crate::writer::CatalogWriter;

/// How often the UI is told to reload genres while a scan runs.
const GENRES_REFRESH: Duration = Duration::from_secs(3);
/// Failures come in bursts (a dead share): the "Unreadable" list is reloaded once
/// they settle for this long.
const UNREADABLE_DEBOUNCE: Duration = Duration::from_secs(1);
/// Longest wait for a running scan to stop before the library is cleared.
const CLEAR_SCAN_WAIT: Duration = Duration::from_secs(30);
/// Tells the UI that the "Unreadable" list changed.
pub const UNREADABLE_EVENT: &str = "unreadable-changed";
/// Tells the UI that the playlist was replaced or its order changed.
pub const PLAYLIST_EVENT: &str = "playlist-changed";
/// Where the playing track is (see [`PositionPayload`]).
pub const POSITION_EVENT: &str = "position";
/// Playlist changes come in bursts: they are saved once they settle this long.
const PLAYLIST_SAVE_DEBOUNCE: Duration = Duration::from_secs(1);
/// While playing, the position is saved this often.
const POSITION_SAVE_EVERY: Duration = Duration::from_secs(10);
/// Longest wait on quit for the playlist to be saved.
const QUIT_SAVE_WAIT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NowPlaying {
    pub id: TrackId,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub year: Option<u16>,
    pub genre_display: String,
    pub paused: bool,
    /// From the catalog; the `position` event also has the decoder's when this is
    /// unknown.
    pub duration_ms: Option<u64>,
}

/// The `position` event: `position_ms` of track `id` have played. `duration_ms`
/// comes from the catalog, else from the decoder; none if neither knows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionPayload {
    pub id: TrackId,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
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
    /// Folders that could not be listed (their tracks were kept).
    skipped_dirs: usize,
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
    /// The player is paused (also before a track is loaded).
    pub paused: AtomicBool,
    /// The last reported position.
    pub position: Mutex<Option<PositionPayload>>,
    /// The playlist, shared with the providers the player pulls from.
    pub playlist: SharedPlaylist,
    save_playlist: Mutex<Sender<()>>,
    playlist_saved_at: Mutex<Instant>,
    /// The restored playlist waits for the share to be reachable (cleared once it
    /// is cued, or once the user plays something else).
    resume_pending: AtomicBool,
    scanning: AtomicBool,
    /// "Clear library" holds the scan slot: shown as such, not as a scan.
    clearing: AtomicBool,
    cancel_scan: Arc<AtomicBool>,
    cancel_reason: Mutex<Option<String>>,
    scan_progress: Mutex<Option<ScanProgressPayload>>,
    unreadable_changed: Mutex<Sender<()>>,
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
        let mut settings = loaded.unwrap_or_default();
        i18n::set_current(&settings.language);
        // Genre keys renamed by newer genre rules (saved with the next settings write).
        settings.last_selection = migrate_selection(&settings.last_selection);
        let query = Catalog::open(&catalog_path)?;
        let catalog_empty = query.is_empty()?;
        let saved_playlist = match query.load_playlist() {
            Ok(Some(saved)) => playlist::restore(&query, saved).unwrap_or_else(|e| {
                log::warn!("cannot restore the playlist: {e}");
                Default::default()
            }),
            Ok(None) => Default::default(),
            Err(e) => {
                log::warn!("cannot read the saved playlist: {e}");
                Default::default()
            }
        };
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
        let (unreadable_tx, unreadable_rx) = mpsc::channel::<()>();
        let (save_tx, save_rx) = mpsc::channel::<()>();
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
            paused: AtomicBool::new(false),
            position: Mutex::new(None),
            playlist: Arc::new(Mutex::new(saved_playlist)),
            save_playlist: Mutex::new(save_tx),
            playlist_saved_at: Mutex::new(Instant::now()),
            resume_pending: AtomicBool::new(true),
            scanning: AtomicBool::new(false),
            clearing: AtomicBool::new(false),
            cancel_scan: Arc::new(AtomicBool::new(false)),
            cancel_reason: Mutex::new(None),
            scan_progress: Mutex::new(None),
            unreadable_changed: Mutex::new(unreadable_tx),
        });

        let handle = app.clone();
        std::thread::Builder::new()
            .name("unreadable-events".into())
            .spawn(move || {
                while unreadable_rx.recv().is_ok() {
                    std::thread::sleep(UNREADABLE_DEBOUNCE);
                    while unreadable_rx.try_recv().is_ok() {}
                    // Failures are written by the writer: wait for those queued.
                    let _ = handle.state::<AppState>().writer.call(|_| Ok(()));
                    let _ = handle.emit(UNREADABLE_EVENT, ());
                }
            })?;

        let handle = app.clone();
        std::thread::Builder::new()
            .name("playlist-save".into())
            .spawn(move || {
                while save_rx.recv().is_ok() {
                    std::thread::sleep(PLAYLIST_SAVE_DEBOUNCE);
                    while save_rx.try_recv().is_ok() {}
                    let st = handle.state::<AppState>();
                    let job = st.playlist_save_job();
                    st.writer.send("playlist", move |c| job.run(c));
                }
            })?;

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
                    let st = handle.state::<AppState>();
                    *lock(&st.status) = Some(status);
                    let _ = handle.emit("connection-status", StatusPayload::from(status));
                    // At login the VPN may come up later: resume once the share answers,
                    // or the saved track would fail and be passed over.
                    if resume_due(&st.resume_pending, status) {
                        st.resume_playlist();
                    }
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

    /// A provider over the playlist, for `Play`/`Cue`.
    pub fn provider(&self) -> Box<CatalogProvider> {
        Box::new(CatalogProvider::new(self.playback(), self.playlist.clone()))
    }

    /// Saves the playlist soon (debounced, through the writer).
    pub fn save_playlist_soon(&self) {
        *lock(&self.playlist_saved_at) = Instant::now();
        let _ = lock(&self.save_playlist).send(());
    }

    fn playlist_save_job(&self) -> playlist::SaveJob {
        let position = lock(&self.position).as_ref().map(|p| (p.id, p.position_ms));
        playlist::save_job(&self.playlist, position)
    }

    /// Saves the playlist now and waits for it, briefly (on quit: a scan holding the
    /// database must not hold up quitting).
    pub fn save_playlist_now(&self) {
        let job = self.playlist_save_job();
        if let Err(e) = self
            .writer
            .call_timeout(QUIT_SAVE_WAIT, move |c| job.run(c))
        {
            log::warn!("cannot save the playlist: {e}");
        }
    }

    /// Plays the playlist from what is due now (fading out what plays), as the user
    /// asked; a held-back resume is dropped.
    pub fn play_playlist(&self) {
        self.resume_pending.store(false, Ordering::SeqCst);
        self.player.send(Command::Play(self.provider()));
    }

    /// The playlist changed: tells the UI and saves it.
    pub fn playlist_changed(&self, app: &AppHandle) {
        let _ = app.emit(PLAYLIST_EVENT, ());
        self.save_playlist_soon();
    }

    /// On start: loads the saved playlist's current track (or the next one) where it
    /// stopped, paused unless "Resume playing automatically on start" is on.
    pub fn resume_playlist(&self) {
        let due = {
            let mut l = lock(&self.playlist);
            !l.ended && l.queue.successor().is_some()
        };
        if !due {
            return;
        }
        let command = resume_command(&self.settings.get(), self.provider());
        log::info!(
            "playlist restored, {}",
            if matches!(command, Command::Cue(_)) {
                "paused"
            } else {
                "playing"
            }
        );
        self.player.send(command);
    }

    pub fn wake_monitor(&self) {
        let _ = lock(&self.wake_monitor).send(());
    }

    /// Schedules an `unreadable-changed` event (debounced).
    pub fn unreadable_changed(&self) {
        let _ = lock(&self.unreadable_changed).send(());
    }

    /// After the servers changed: reconnects, or with no server set up (no username)
    /// drops the connection and reports no status, as on a first start.
    pub fn forget_or_reconnect(&self, app: &AppHandle, settings: &Settings) {
        if settings.primary.username.trim().is_empty() {
            self.source.set(None);
            *lock(&self.status) = None;
            let _ = app.emit("connection-status", None::<StatusPayload>);
        } else {
            self.reconnect(settings);
        }
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
            active_server: {
                let source = self.source.clone();
                Arc::new(move || source.servers().map(|s| s.failover.active()))
            },
        }
    }

    pub fn scan_status(&self) -> ScanStatus {
        let scanning =
            self.scanning.load(Ordering::SeqCst) && !self.clearing.load(Ordering::SeqCst);
        ScanStatus {
            scanning,
            progress: lock(&self.scan_progress).clone().filter(|_| scanning),
        }
    }
}

/// A restored playlist waits paused (`Cue`) unless the user asked for it to play on
/// start (start at login should not start music unasked).
pub fn resume_command(s: &Settings, provider: Box<dyn TrackProvider>) -> Command {
    if s.resume_autoplay {
        Command::Play(provider)
    } else {
        Command::Cue(provider)
    }
}

/// Whether the held-back resume goes now: once, when the share is reachable.
pub fn resume_due(pending: &AtomicBool, status: ConnStatus) -> bool {
    matches!(status, ConnStatus::Ok | ConnStatus::Secondary)
        && pending.swap(false, Ordering::SeqCst)
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
    let title = playlist::track_title(&row);
    Ok(NowPlaying {
        duration_ms: row.meta.duration_ms,
        id,
        artist: row.meta.artist.unwrap_or_default(),
        title,
        album: row.meta.album.unwrap_or_default(),
        year: row.meta.year,
        genre_display: catalog
            .track_genres(id)?
            .into_iter()
            .map(|g| {
                if g == UNKNOWN_DISPLAY {
                    i18n::t("genres.unknown")
                } else {
                    g
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
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
            let paused = st.paused.load(Ordering::SeqCst);
            let np = match now_playing_of(&lock(&st.query), id, paused) {
                Ok(np) => Some(np),
                Err(e) => {
                    log::warn!("track {id} not in catalog: {e}");
                    None
                }
            };
            *lock(&st.now_playing) = np.clone();
            emit_now_playing(app, &np);
            st.save_playlist_soon();
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
            st.unreadable_changed();
            // A bad file leaves the playlist.
            st.playlist_changed(app);
            // Possibly the network: re-check the connection.
            st.wake_monitor();
        }
        Event::Paused(paused) => {
            st.paused.store(paused, Ordering::SeqCst);
            st.save_playlist_soon();
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
        Event::Position {
            id,
            ms,
            duration_ms,
        } => {
            let known = lock(&st.now_playing)
                .as_ref()
                .filter(|n| n.id == id)
                .and_then(|n| n.duration_ms);
            let payload = PositionPayload {
                id,
                position_ms: ms,
                duration_ms: known.or(duration_ms),
            };
            *lock(&st.position) = Some(payload.clone());
            let _ = app.emit(POSITION_EVENT, payload);
            let due = lock(&st.playlist_saved_at).elapsed() >= POSITION_SAVE_EVERY;
            if due && !st.paused.load(Ordering::SeqCst) {
                st.save_playlist_soon();
            }
        }
        Event::Idle => {
            *lock(&st.now_playing) = None;
            *lock(&st.position) = None;
            emit_now_playing(app, &None);
            // Ran out (not stopped by failures): nothing to resume next time.
            let mut l = lock(&st.playlist);
            l.ended = l.queue.successor().is_none();
            drop(l);
            st.save_playlist_soon();
        }
    }
}

/// Starts a scan on its own thread with its own catalog connection.
pub fn start_scan(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    if st.source.servers().is_none() {
        return Err(i18n::t("err.save_first"));
    }
    if st.clearing.load(Ordering::SeqCst) {
        return Err(i18n::t("err.clearing"));
    }
    if st.scanning.swap(true, Ordering::SeqCst) {
        return Err(if st.clearing.load(Ordering::SeqCst) {
            i18n::t("err.clearing")
        } else {
            i18n::t("err.scan_running")
        });
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
        return Err(i18n::tf("err.scan_start", &[("error", &e.to_string())]));
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

/// Text for a failed scan, in `lang`: connection problems in the words the status uses.
fn scan_error_message(lang: &str, e: &smp_core::Error) -> String {
    match e {
        smp_core::Error::Source(SourceError::Auth) => i18n::tr(lang, ConnStatus::Auth.text_key()),
        smp_core::Error::Source(_) => i18n::tr(lang, ConnStatus::Unreachable.text_key()),
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
                "scan finished in {:.1} s: {} files, {} read, {} unchanged, {} failed, {} removed, {} folders skipped{}",
                started.elapsed().as_secs_f32(),
                r.total,
                r.read,
                r.unchanged,
                r.failed,
                r.removed,
                r.skipped_dirs.len(),
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
                skipped_dirs: r.skipped_dirs.len(),
            }
        }
        Err(e) => {
            log::warn!("scan failed: {e}");
            st.wake_monitor();
            ScanDonePayload {
                cancelled,
                cancel_reason,
                error: Some(scan_error_message(&i18n::current(), &e)),
                ..Default::default()
            }
        }
    };
    st.scanning.store(false, Ordering::SeqCst);
    *lock(&st.scan_progress) = None;
    let _ = app.emit("genres-changed", ());
    let _ = app.emit("scan-done", payload);
    st.unreadable_changed();
}

/// Takes the scan slot (`scanning`) for the caller, calling `cancel` while a scan
/// still holds it; `None` if it is not free within `wait`. No scan can start while
/// the returned slot is held.
fn take_scan_slot<'a>(
    scanning: &'a AtomicBool,
    cancel: impl Fn(),
    wait: Duration,
) -> Option<ScanSlot<'a>> {
    let deadline = Instant::now() + wait;
    while scanning
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        cancel();
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(ScanSlot(scanning))
}

/// Clears the flag when dropped (also on an early return).
struct ScanSlot<'a>(&'a AtomicBool);

impl Drop for ScanSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// "Clear library": stops any scan and the music, then empties the catalog through
/// the writer (one transaction, which every connection sees at once). The scan flag
/// is held meanwhile, so no scan starts until the catalog is empty. Settings stay;
/// only the last genre selection is forgotten.
pub fn clear_library(app: &AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    st.clearing.store(true, Ordering::SeqCst);
    let _clearing = ScanSlot(&st.clearing);
    let reason = i18n::t("scan.cancelled_clear");
    let _slot = take_scan_slot(
        &st.scanning,
        || {
            cancel_scan(&st, Some(&reason));
        },
        CLEAR_SCAN_WAIT,
    )
    .ok_or_else(|| i18n::t("err.clear_busy"))?;
    st.player.send(Command::Stop);
    let cleared = st.writer.call(|c| c.clear());
    lock(&st.banned).clear();
    // Replaced, not reset: its version keeps counting up.
    lock(&st.playlist).replace(
        playlist::Selection::Manual,
        smp_core::queue::Queue::fixed(&[], smp_core::shuffle::entropy_rng()),
    );
    *lock(&st.position) = None;
    let _ = app.emit(PLAYLIST_EVENT, ());
    st.settings.update(|s| s.last_selection.clear(), |_| {});
    *lock(&st.now_playing) = None;
    emit_now_playing(app, &None);
    let _ = app.emit("genres-changed", ());
    let _ = app.emit(UNREADABLE_EVENT, ());
    match cleared {
        Ok(()) => {
            log::info!("library cleared");
            Ok(())
        }
        Err(e) => {
            log::warn!("clearing the library failed: {e}");
            Err(i18n::tf("err.database", &[("error", &e.to_string())]))
        }
    }
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
            year: Some(1997),
            ..Default::default()
        };
        let id = c.upsert_track("x/Some Song.mp3", 1, 1, &meta).unwrap();
        c.rebuild_genres().unwrap();
        let np = now_playing_of(&c, id, true).unwrap();
        assert_eq!(np.title, "Some Song");
        assert_eq!(np.artist, "Artist");
        assert_eq!(np.album, "");
        assert_eq!(np.genre_display, "Chillout, Trance");
        assert_eq!(np.year, Some(1997));
        assert_eq!(np.duration_ms, None);
        assert!(np.paused);
    }

    struct Nothing;
    impl smp_core::audio::player::TrackProvider for Nothing {
        fn next_track(&mut self) -> Option<smp_core::audio::player::PlayItem> {
            None
        }
        fn on_failed(&mut self, _: TrackId, _: &smp_core::failure::Failure) {}
        fn on_played(&mut self, _: TrackId, _: Option<f32>) {}
    }

    #[test]
    fn a_restored_playlist_waits_paused_unless_asked_to_play() {
        let s = Settings::default();
        assert!(!s.resume_autoplay, "off by default");
        assert!(matches!(
            resume_command(&s, Box::new(Nothing)),
            Command::Cue(_)
        ));
        let on = Settings {
            resume_autoplay: true,
            ..s
        };
        assert!(matches!(
            resume_command(&on, Box::new(Nothing)),
            Command::Play(_)
        ));
    }

    #[test]
    fn the_resume_waits_for_the_share_and_happens_once() {
        let pending = AtomicBool::new(true);
        assert!(
            !resume_due(&pending, ConnStatus::Unreachable),
            "VPN not up yet"
        );
        assert!(!resume_due(&pending, ConnStatus::Auth));
        assert!(resume_due(&pending, ConnStatus::Secondary));
        assert!(!resume_due(&pending, ConnStatus::Ok), "only once");
        let played_meanwhile = AtomicBool::new(false);
        assert!(!resume_due(&played_meanwhile, ConnStatus::Ok));
    }

    /// A scan of a slow source on its own thread, keeping `scanning` the way
    /// `start_scan`/`run_scan` do.
    #[test]
    fn taking_the_scan_slot_cancels_a_running_scan_and_keeps_new_ones_out() {
        use smp_core::source::{FileEntry, ReadSeek, WalkReport};
        use std::sync::atomic::AtomicUsize;

        struct Slow {
            bytes: Vec<u8>,
            opened: AtomicUsize,
        }
        impl MusicSource for Slow {
            fn walk(&self, f: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
                for i in 0..200 {
                    f(FileEntry {
                        rel_path: format!("Rock/{i}.mp3"),
                        size: self.bytes.len() as u64,
                        mtime: 1,
                    });
                }
                Ok(WalkReport::default())
            }
            fn open(&self, _: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
                self.opened.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
            }
            fn describe(&self) -> String {
                "slow".into()
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        let writer = CatalogWriter::spawn(Catalog::open(&db).unwrap()).unwrap();
        let src = Slow {
            bytes: std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../crates/core/tests/fixtures/tone.mp3"),
            )
            .unwrap(),
            opened: AtomicUsize::new(0),
        };
        let scanning = AtomicBool::new(true);
        let cancel = AtomicBool::new(false);
        std::thread::scope(|s| {
            let scan = s.spawn(|| {
                let mut cat = Catalog::open(&db).unwrap();
                let opts = ScanOptions {
                    workers: 2,
                    ..ScanOptions::default()
                };
                let r = scan_with(&src, &mut cat, &mut |_| {}, &cancel, &opts);
                scanning.store(false, Ordering::SeqCst);
                r.unwrap()
            });
            while src.opened.load(Ordering::SeqCst) < 20 {
                std::thread::sleep(Duration::from_millis(5));
            }
            let slot = take_scan_slot(
                &scanning,
                || cancel.store(true, Ordering::SeqCst),
                Duration::from_secs(10),
            )
            .expect("the scan stops");
            let report = scan.join().unwrap();
            assert!(report.read < 200, "cancelled: {report:?}");
            assert!(
                take_scan_slot(&scanning, || {}, Duration::from_millis(50)).is_none(),
                "held: no second taker"
            );
            writer.call(|c| c.clear()).unwrap();
            drop(slot);
        });
        assert!(!scanning.load(Ordering::SeqCst), "released");
        let cat = Catalog::open(&db).unwrap();
        assert!(cat.is_empty().unwrap(), "nothing written after the clear");
    }

    #[test]
    fn scan_errors_use_the_matching_status_text() {
        let auth = smp_core::Error::Source(SourceError::Auth);
        assert_eq!(
            scan_error_message("en", &auth),
            "Wrong username or password"
        );
        assert_eq!(
            scan_error_message("hr", &auth),
            "Pogrešno korisničko ime ili lozinka"
        );
        for e in [SourceError::Unreachable, SourceError::Timeout] {
            let e = smp_core::Error::Source(e);
            assert_eq!(
                scan_error_message("en", &e),
                i18n::tr("en", "conn.unreachable")
            );
        }
        let other = smp_core::Error::Other("disk full".into());
        assert_eq!(scan_error_message("en", &other), "disk full");
    }
}
