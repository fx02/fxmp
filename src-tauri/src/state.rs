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
use smp_core::{LibraryId, TrackId};
use tauri::{AppHandle, Emitter, Listener, Manager};

use crate::connection::{monitor, AppSource, ConnStatus, Libraries, StatusPayload};
use crate::credentials::{self, CredentialStore, Keychain};
use crate::devdata;
use crate::i18n;
use crate::playlist::{self, SharedPlaylist};
use crate::provider::{BannedSet, CatalogProvider, LibraryHealth, Playback};
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
/// A resume that waited longer than this for the share does not start playing on
/// its own (it is cued, paused).
const LATE_RESUME: Duration = Duration::from_secs(120);
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

/// A library as the scan reports it (the UI names a blank one).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LibraryRef {
    pub id: LibraryId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgressPayload {
    seen: usize,
    read: usize,
    failed: usize,
    current: String,
    /// The library being scanned, and which of how many it is.
    library: Option<LibraryRef>,
    library_index: usize,
    library_count: usize,
}

impl From<&ScanProgress> for ScanProgressPayload {
    fn from(p: &ScanProgress) -> Self {
        Self {
            seen: p.seen,
            read: p.read,
            failed: p.failed,
            current: p.current.clone(),
            library: None,
            library_index: 0,
            library_count: 0,
        }
    }
}

/// A library a scan could not read (offline), and why.
#[derive(Debug, Clone, Serialize)]
struct LibraryProblem {
    #[serde(flatten)]
    library: LibraryRef,
    error: String,
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
    /// Libraries skipped because they could not be read (their tracks were kept).
    offline: Vec<LibraryProblem>,
    /// SMB libraries listed by their backup server: nothing of theirs was removed.
    from_backup: Vec<LibraryRef>,
    /// Libraries whose folder listed no file although they have tracks (an empty
    /// mount point, say): nothing of theirs was removed.
    looks_empty: Vec<LibraryRef>,
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
    /// The libraries' last status (shared with the providers).
    pub status: Arc<Mutex<Option<StatusPayload>>>,
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
    /// The restored playlist waiting for the share, and Play presses meanwhile.
    /// Held while the resume, or a press, decides and sends its command.
    pub resume: Mutex<ResumeGate>,
    /// What the engine has (see [`Engine`]): loading from a sent Play/Cue, then
    /// the started track (also one the catalog no longer has) until it goes idle.
    pub engine: Mutex<Engine>,
    launched_at: Instant,
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
        let dev_dir = devdata::current();
        let (config_dir, data_dir) = match &dev_dir {
            Some(dir) => {
                log::warn!(
                    "debug build: data in {} ({}), passwords under keychain service {}",
                    dir.display(),
                    devdata::ENV,
                    devdata::DEV_SERVICE
                );
                (dir.clone(), dir.clone())
            }
            None => (app.path().app_config_dir()?, app.path().app_data_dir()?),
        };
        let settings_path = config_dir.join(settings::FILE_NAME);
        std::fs::create_dir_all(&data_dir)?;
        let catalog_path = data_dir.join("catalog.db");

        let loaded = Settings::load(&settings_path);
        // Saved settings with a username or another library: something is set up.
        let configured = loaded
            .as_ref()
            .is_some_and(|s| !s.primary.username.trim().is_empty() || !s.libraries.is_empty());
        let mut settings = loaded.unwrap_or_default();
        i18n::set_current(&settings.language);
        // Genre keys renamed by newer genre rules (saved with the next settings write).
        settings.last_selection = migrate_selection(&settings.last_selection);
        let query = Catalog::open(&catalog_path)?;
        // Libraries that are off (or that the settings lost) are marked before
        // anything is read, so the restored playlist leaves their tracks out.
        if let Err(e) = query.set_disabled_libraries(&libraries_off(&settings, &query)) {
            log::warn!("cannot mark the libraries that are off: {e}");
        }
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
        let creds: Arc<dyn CredentialStore> = Arc::new(Keychain {
            service: devdata::keychain_service(dev_dir.is_some()),
        });

        // Libraries are set once their passwords are read (see below).
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
            status: Arc::new(Mutex::new(None)),
            wake_monitor: Mutex::new(wake_tx),
            now_playing: Mutex::new(None),
            paused: AtomicBool::new(false),
            position: Mutex::new(None),
            playlist: Arc::new(Mutex::new(saved_playlist)),
            save_playlist: Mutex::new(save_tx),
            playlist_saved_at: Mutex::new(Instant::now()),
            resume: Mutex::new(ResumeGate::waiting()),
            engine: Mutex::new(Engine::Empty),
            launched_at: Instant::now(),
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
                    *lock(&st.status) = Some(status.clone());
                    let _ = handle.emit("connection-status", status);
                    // At login the VPN may come up later: resume once the track's
                    // library answers, or the saved track would fail and be passed over.
                    st.resume_playlist(status);
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
                    let settings = st.settings.get();
                    // One start's reads are remembered: an old entry the user
                    // refused is not asked for twice.
                    let start = credentials::Remembered::new(st.creds.as_ref());
                    credentials::migrate_legacy(&start, &settings);
                    let libraries = Libraries::from_settings(&settings, &start);
                    if !st.source.set_if_unset(libraries) {
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
        start_with(&self.resume, &self.engine, || {
            self.player.send(Command::Play(self.provider()))
        });
    }

    /// The playlist changed: tells the UI and saves it.
    pub fn playlist_changed(&self, app: &AppHandle) {
        let _ = app.emit(PLAYLIST_EVENT, ());
        self.save_playlist_soon();
    }

    /// On start, once the library of the track to resume answers (`status`): loads
    /// the saved playlist's current track (or the next one) where it stopped,
    /// paused unless "Resume playing automatically on start" is on or Play was
    /// pressed meanwhile. If that library is offline while another one answers,
    /// it does not wait: the track is no longer resumed where it stopped, and the
    /// playlist goes on (an offline track is passed over, not counted as failed).
    pub fn resume_playlist(&self, status: &StatusPayload) {
        let due_track = {
            let mut l = lock(&self.playlist);
            l.resume_at.map(|r| r.0).or_else(|| l.queue.successor())
        };
        let library = due_track
            .and_then(|id| lock(&self.query).track(id).ok())
            .map(|r| r.library_id);
        let (status, drop_target) = resume_decision(status, library);
        resume_with(
            &self.resume,
            &self.engine,
            status,
            |pressed| {
                let due = {
                    let mut l = lock(&self.playlist);
                    if drop_target {
                        log::info!("the track to resume is in a library that is offline");
                        l.resume_at = None;
                    }
                    !l.ended && l.queue.successor().is_some()
                };
                if !due {
                    return None;
                }
                let late = resume_is_late(self.launched_at, Instant::now());
                let command = resume_command(&self.settings.get(), self.provider(), late, pressed);
                log::info!(
                    "playlist restored, {}",
                    if matches!(command, Command::Cue(_)) {
                        "paused"
                    } else {
                        "playing"
                    }
                );
                Some(command)
            },
            |command| self.player.send(command),
        );
    }

    pub fn wake_monitor(&self) {
        let _ = lock(&self.wake_monitor).send(());
    }

    /// Schedules an `unreadable-changed` event (debounced).
    pub fn unreadable_changed(&self) {
        let _ = lock(&self.unreadable_changed).send(());
    }

    /// After the libraries changed: reconnects, or with none set up (no username,
    /// no other library) drops the connection and reports no status, as on a first
    /// start.
    pub fn forget_or_reconnect(&self, app: &AppHandle, settings: &Settings) {
        self.reconnect(settings);
        if self.source.libraries().is_none() {
            *lock(&self.status) = None;
            let _ = app.emit("connection-status", None::<StatusPayload>);
        }
    }

    /// Replaces the libraries (fresh connections, forgetting a rejected password),
    /// tells the catalog which are off, and re-checks the connections.
    pub fn reconnect(&self, settings: &Settings) {
        self.source.set(Some(Libraries::from_settings(
            settings,
            self.creds.as_ref(),
        )));
        let disabled = libraries_off(settings, &lock(&self.query));
        if let Err(e) = self
            .writer
            .call(move |c| c.set_disabled_libraries(&disabled))
        {
            log::warn!("cannot mark the libraries that are off: {e}");
        }
        self.wake_monitor();
    }

    /// What a new track provider needs.
    pub fn playback(&self) -> Playback {
        Playback {
            db: self.catalog.clone(),
            writer: self.writer.clone(),
            banned: self.banned.clone(),
            libraries: Arc::new(LiveLibraries {
                source: self.source.clone(),
                status: self.status.clone(),
            }),
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

/// The libraries that are off: switched off in the settings, or no longer there
/// (tracks of a library the settings lost are kept, but hidden).
pub fn libraries_off(settings: &Settings, catalog: &Catalog) -> Vec<LibraryId> {
    let known: Vec<LibraryId> = settings.all_libraries().iter().map(|l| l.id).collect();
    let mut off = settings.disabled_libraries();
    match catalog.library_track_counts() {
        Ok(counts) => off.extend(counts.into_keys().filter(|id| !known.contains(id))),
        Err(e) => log::warn!("cannot count the libraries' tracks: {e}"),
    }
    off.sort_unstable();
    off
}

/// What the resume waits for, and whether it drops its target. It waits for the
/// library of the track to resume (`library`): an SMB server that does not
/// answer yet (the VPN comes up later at login) keeps the saved track and
/// position. Only a local folder that is missing (a disk not plugged in, or not
/// mounted) is not waited for while another library answers: the target is
/// dropped and the playlist goes on. A library unknown or not in use: any that
/// answers. With none answering, the worst status (it waits).
pub fn resume_decision(status: &StatusPayload, library: Option<LibraryId>) -> (ConnStatus, bool) {
    let any = status
        .libraries
        .iter()
        .map(|l| l.status)
        .find(|s| s.reachable());
    match library.and_then(|l| status.of(l)) {
        Some(s) if s.reachable() => (s, false),
        Some(ConnStatus::Missing) => match any {
            Some(up) => (up, true),
            None => (status.status, false),
        },
        Some(s) => (s, false),
        None => (any.unwrap_or(status.status), false),
    }
}

/// The libraries as the app knows them while playing.
struct LiveLibraries {
    source: Arc<AppSource>,
    status: Arc<Mutex<Option<StatusPayload>>>,
}

impl LibraryHealth for LiveLibraries {
    fn active_server(&self, library: LibraryId) -> Option<Which> {
        self.source.libraries()?.get(library)?.active_server()
    }

    /// A local folder is checked now; an SMB library is offline while its last
    /// status says so (a missing share folder on the server reads as "not found").
    fn offline(&self, library: LibraryId) -> bool {
        let Some(libs) = self.source.libraries() else {
            return false;
        };
        let Some(lib) = libs.get(library) else {
            return false;
        };
        let last = lock(&self.status).as_ref().and_then(|s| s.of(library));
        lib.offline_now() || last.is_some_and(|s| !s.reachable())
    }
    fn any_online(&self) -> bool {
        lock(&self.status)
            .as_ref()
            .is_some_and(|s| s.libraries.iter().any(|l| l.status.reachable()))
    }
}

/// A restored playlist waits paused (`Cue`) unless the user asked for it to play on
/// start (start at login should not start music unasked) and the share answered
/// soon enough, or Play was pressed while it waited (`pressed`).
pub fn resume_command(
    s: &Settings,
    provider: Box<dyn TrackProvider>,
    late: bool,
    pressed: bool,
) -> Command {
    if pressed || (s.resume_autoplay && !late) {
        Command::Play(provider)
    } else {
        Command::Cue(provider)
    }
}

/// The share answered too long after launch for music to start on its own.
pub fn resume_is_late(launched: Instant, now: Instant) -> bool {
    now.saturating_duration_since(launched) > LATE_RESUME
}

/// A Play/Pause press, as far as a waiting resume cares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Press {
    /// Play/Pause.
    Toggle,
    Play,
    Pause,
}

/// The restored playlist waiting for the share (`pending`), and whether the
/// user's presses meanwhile add up to "play" (`play_requested`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeGate {
    pub pending: bool,
    pub play_requested: bool,
}

impl ResumeGate {
    /// At start: the resume waits for the share.
    pub fn waiting() -> Self {
        Self {
            pending: true,
            play_requested: false,
        }
    }

    /// Whether the resume goes now (once, when the share is reachable): then
    /// `Some(whether Play was pressed meanwhile)`.
    pub fn due(&mut self, status: ConnStatus) -> Option<bool> {
        if !self.pending || !matches!(status, ConnStatus::Ok | ConnStatus::Secondary) {
            return None;
        }
        self.pending = false;
        Some(std::mem::take(&mut self.play_requested))
    }

    /// The user played something else: the resume is dropped.
    pub fn cancel(&mut self) {
        self.pending = false;
        self.play_requested = false;
    }

    /// While the resume waits, a press is remembered (the last one counts: a
    /// second Play/Pause or a Pause takes a Play back) and `true` is returned;
    /// otherwise `false` and the press goes on to the player.
    pub fn press(&mut self, press: Press) -> bool {
        if !self.pending {
            return false;
        }
        self.play_requested = match press {
            Press::Toggle => !self.play_requested,
            Press::Play => true,
            Press::Pause => false,
        };
        true
    }
}

/// What the engine has, as far as Play/Pause cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// Nothing loaded (at start, or idle).
    #[default]
    Empty,
    /// A Play or Cue was sent and its track has not started yet.
    Loading,
    /// This track started (playing, paused or cued).
    Track(TrackId),
}

impl Engine {
    /// Whether Play/Pause goes to the player (rather than starting the playlist).
    pub fn loaded(self) -> bool {
        self != Engine::Empty
    }

    /// A Play or Cue was sent: loading until a track starts.
    pub fn sent(self) -> Self {
        Engine::Loading
    }
}

/// The engine after event `ev`: a start sets the track (whatever the catalog
/// knows), going idle clears it, and a failure while loading ends the loading (a
/// prepared next track failing leaves the playing one).
pub fn engine_after(current: Engine, ev: &Event) -> Engine {
    match ev {
        Event::TrackStarted { id } => Engine::Track(*id),
        Event::Idle => Engine::Empty,
        Event::TrackSkipped { .. } if current == Engine::Loading => Engine::Empty,
        _ => current,
    }
}

/// Starts playing (`send`, a Play) as the user asked: a waiting resume is dropped
/// and the engine counts as loading. Sent while the resume gate is held.
pub fn start_with(gate: &Mutex<ResumeGate>, engine: &Mutex<Engine>, send: impl FnOnce()) {
    let mut gate = lock(gate);
    gate.cancel();
    {
        let mut e = lock(engine);
        *e = e.sent();
    }
    send();
}

/// The held-back resume, once the share answers (`status`): `make` gets whether
/// Play was pressed meanwhile and gives the command (none if nothing is due),
/// which is sent while the gate is still held, so a press either comes before
/// (and is taken along) or after (and finds the track loading).
pub fn resume_with(
    gate: &Mutex<ResumeGate>,
    engine: &Mutex<Engine>,
    status: ConnStatus,
    make: impl FnOnce(bool) -> Option<Command>,
    send: impl FnOnce(Command),
) {
    let mut gate = lock(gate);
    let Some(pressed) = gate.due(status) else {
        return;
    };
    let Some(command) = make(pressed) else {
        return;
    };
    {
        let mut e = lock(engine);
        *e = e.sent();
    }
    send(command);
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
    {
        let mut engine = lock(&st.engine);
        *engine = engine_after(*engine, &ev);
    }
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
            // Playback of its library moved between servers (or the server is back):
            // let the monitor confirm and report it.
            let library = lock(&st.query).track(id).map(|r| r.library_id).ok();
            let active = library
                .and_then(|lib| Some((lib, st.source.libraries()?.get(lib)?.active_server()?)));
            if let Some((lib, active)) = active {
                let expected = match active {
                    Which::Primary => ConnStatus::Ok,
                    Which::Secondary => ConnStatus::Secondary,
                };
                let last = lock(&st.status).as_ref().and_then(|s| s.of(lib));
                if last != Some(expected) {
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
    if st.source.libraries().is_none() {
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

/// What one library's scan added up to (see [`add_up_scans`]).
type LibraryScan = (LibraryRef, smp_core::Result<smp_core::scan::ScanReport>);

/// The `scan-done` payload of a refresh of several libraries: the reports added up,
/// libraries that could not be read listed (their tracks were kept), and an error
/// only when none could be scanned.
fn add_up_scans(lang: &str, scans: Vec<LibraryScan>) -> ScanDonePayload {
    let mut done = ScanDonePayload::default();
    let mut scanned = 0;
    for (library, result) in scans {
        match result {
            Ok(r) => {
                scanned += 1;
                done.total += r.total;
                done.read += r.read;
                done.unchanged += r.unchanged;
                done.failed += r.failed;
                done.removed += r.removed;
                done.skipped_dirs += r.skipped_dirs.len();
                if r.from_backup {
                    done.from_backup.push(library.clone());
                }
                if r.looks_empty {
                    done.looks_empty.push(library);
                }
            }
            Err(e) => done.offline.push(LibraryProblem {
                library,
                error: scan_error_message(lang, &e),
            }),
        }
    }
    done.ok = scanned > 0;
    if !done.ok {
        done.error = done.offline.first().map(|p| p.error.clone());
    }
    done
}

/// Why `lib` is not scanned now, if it is not: a local folder that is not there
/// (its disk not mounted, see `connection::folder_status`), or any library but
/// the first while the catalog is not `ready` for libraries.
/// The check of a local folder is bounded (a hung network volume) and given up
/// on Cancel; one that does not answer counts as missing.
fn not_scannable(
    lib: &Arc<crate::connection::LibraryConn>,
    ready: bool,
    cancel: &AtomicBool,
) -> Option<String> {
    use crate::connection::{wait_check, LibraryKind, PROBE_TIMEOUT};
    if !ready && lib.id != smp_core::DEFAULT_LIBRARY {
        return Some(i18n::t("err.catalog_pending"));
    }
    if !matches!(lib.kind, LibraryKind::Local { .. }) {
        return None;
    }
    let status = lib
        .start_check()
        .and_then(|rx| wait_check(&rx, Instant::now() + PROBE_TIMEOUT, cancel));
    match status {
        Some(s) if s.reachable() => None,
        _ => Some(i18n::t(ConnStatus::Missing.text_key())),
    }
}

/// "Refresh library": scans every library in use, one after another. One that
/// cannot be read is skipped (its tracks are kept) and the next one is scanned.
fn run_scan(app: &AppHandle) {
    let st = app.state::<AppState>();
    let started = Instant::now();
    let _ = app.emit(
        "scan-progress",
        ScanProgressPayload::from(&ScanProgress::default()),
    );
    let libraries: Vec<_> = st
        .source
        .libraries()
        .map(|l| l.iter().cloned().collect())
        .unwrap_or_default();
    let count = libraries.len();
    // Before the catalog's update for libraries, a path may be in one library only.
    let ready = lock(&st.query).libraries_ready();
    let mut last_genres = Instant::now();
    let mut scans: Vec<LibraryScan> = Vec::new();
    for (index, lib) in libraries.iter().enumerate() {
        if st.cancel_scan.load(Ordering::SeqCst) {
            break;
        }
        let library = LibraryRef {
            id: lib.id,
            name: lib.name.clone(),
        };
        let mut progress = |p: &ScanProgress| {
            let payload = ScanProgressPayload {
                library: Some(library.clone()),
                library_index: index + 1,
                library_count: count,
                ..ScanProgressPayload::from(p)
            };
            *lock(&st.scan_progress) = Some(payload.clone());
            let _ = app.emit("scan-progress", payload);
            if last_genres.elapsed() >= GENRES_REFRESH {
                last_genres = Instant::now();
                let _ = app.emit("genres-changed", ());
            }
        };
        progress(&ScanProgress::default());
        if let Some(reason) = not_scannable(lib, ready, &st.cancel_scan) {
            log::warn!("library {} not scanned: {reason}", lib.id);
            scans.push((library, Err(smp_core::Error::Other(reason))));
            continue;
        }
        let result = Catalog::open(&st.catalog_path).and_then(|mut catalog| {
            scan_with(
                lib.id,
                lib.source(),
                &mut catalog,
                &mut progress,
                &st.cancel_scan,
                &ScanOptions::default(),
            )
        });
        match &result {
            Ok(r) => log::info!(
                "scan of library {} finished: {} files, {} read, {} unchanged, {} failed, \
                 {} removed, {} folders skipped{}",
                lib.id,
                r.total,
                r.read,
                r.unchanged,
                r.failed,
                r.removed,
                r.skipped_dirs.len(),
                if r.from_backup {
                    " (from the backup server)"
                } else {
                    ""
                }
            ),
            Err(e) => {
                log::warn!(
                    "scan of library {} failed, its tracks are kept: {e}",
                    lib.id
                );
                st.wake_monitor();
            }
        }
        scans.push((library, result));
    }
    let cancelled = st.cancel_scan.load(Ordering::SeqCst);
    let cancel_reason = lock(&st.cancel_reason).take().filter(|_| cancelled);
    let payload = ScanDonePayload {
        cancelled,
        cancel_reason,
        ..add_up_scans(&i18n::current(), scans)
    };
    log::info!(
        "refresh of {count} libraries finished in {:.1} s{}",
        started.elapsed().as_secs_f32(),
        if cancelled { " (cancelled)" } else { "" }
    );
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

/// Takes out of the playlist the tracks that are gone or whose library is off;
/// if that changed the track due next, the player drops the one it prepared.
pub fn prune_playlist(app: &AppHandle, st: &AppState) -> smp_core::Result<()> {
    let ids: Vec<TrackId> = lock(&st.playlist)
        .queue
        .order()
        .iter()
        .map(|e| e.id)
        .collect();
    let keep: std::collections::HashSet<TrackId> = {
        let q = lock(&st.query);
        let off = q.disabled_libraries()?;
        q.tracks_by_ids(&ids)?
            .into_iter()
            .filter(|r| !off.contains(&r.library_id))
            .map(|r| r.id)
            .collect()
    };
    let due_changed = {
        let mut l = lock(&st.playlist);
        let before = l.queue.successor();
        l.queue.forget_where(|id| !keep.contains(&id));
        l.queue.successor() != before
    };
    if due_changed {
        st.player.send(Command::Requeue);
    }
    st.playlist_changed(app);
    Ok(())
}

/// Deletes the tracks of libraries `ids` (no longer in the settings), stopping a
/// scan first and keeping new ones out meanwhile, so none writes them back.
pub fn delete_library_tracks(st: &AppState, ids: &[LibraryId]) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let reason = i18n::t("scan.cancelled_settings");
    let _slot = take_scan_slot(
        &st.scanning,
        || {
            cancel_scan(st, Some(&reason));
        },
        CLEAR_SCAN_WAIT,
    )
    .ok_or_else(|| i18n::t("err.remove_busy"))?;
    for &id in ids {
        if let Err(e) = st.writer.call(move |c| c.remove_library(id)) {
            log::warn!("tracks of library {id} not deleted (they stay hidden): {e}");
        }
    }
    Ok(())
}

/// Removes library `id` (not the first one): stops a scan (no scan starts until
/// it is done), drops the library from the settings, deletes its tracks and takes
/// them out of the playlist, and forgets its passwords.
pub fn remove_library(app: &AppHandle, id: LibraryId) -> Result<(), String> {
    let st = app.state::<AppState>();
    if id == smp_core::DEFAULT_LIBRARY {
        return Err(i18n::t("err.remove_default"));
    }
    let Some(library) = st.settings.get().library(id) else {
        return Err(i18n::t("err.no_library"));
    };
    let reason = i18n::t("scan.cancelled_settings");
    let _slot = take_scan_slot(
        &st.scanning,
        || {
            cancel_scan(&st, Some(&reason));
        },
        CLEAR_SCAN_WAIT,
    )
    .ok_or_else(|| i18n::t("err.remove_busy"))?;
    let mut new = st.settings.get();
    new.libraries.retain(|l| l.id != id);
    let saved = st
        .settings
        .replace_and_save(new, |_, _| {})
        .map_err(|e| i18n::tf("err.save_settings", &[("error", &e.to_string())]))?;
    st.forget_or_reconnect(app, &saved);
    let removed = st.writer.call(move |c| c.remove_library(id));
    if let crate::settings::LibrarySource::Smb { primary, secondary } = &library.source {
        for server in std::iter::once(primary).chain(secondary.as_ref()) {
            if let Err(e) = credentials::delete_server_password(st.creds.as_ref(), id, server) {
                log::warn!("library {id}: password not deleted: {e}");
            }
        }
    }
    if let Err(e) = prune_playlist(app, &st) {
        log::warn!("playlist after removing library {id}: {e}");
    }
    let _ = app.emit("genres-changed", ());
    let _ = app.emit(UNREADABLE_EVENT, ());
    match removed {
        Ok(n) => {
            log::info!("library {id} removed with its {n} tracks");
            Ok(())
        }
        Err(e) => {
            log::warn!("deleting the tracks of library {id} failed: {e}");
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
        let id = c
            .upsert_track(smp_core::DEFAULT_LIBRARY, "x/Some Song.mp3", 1, 1, &meta)
            .unwrap();
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
        let cmd = |s: &Settings, late, pressed| resume_command(s, Box::new(Nothing), late, pressed);
        assert!(matches!(cmd(&s, false, false), Command::Cue(_)));
        let on = Settings {
            resume_autoplay: true,
            ..s.clone()
        };
        assert!(matches!(cmd(&on, false, false), Command::Play(_)));
        // The share answered long after launch: no music unasked.
        assert!(matches!(cmd(&on, true, false), Command::Cue(_)));
        // Play was pressed while waiting for the share: it plays.
        assert!(matches!(cmd(&s, false, true), Command::Play(_)));
        assert!(matches!(cmd(&s, true, true), Command::Play(_)));
    }

    #[test]
    fn a_resume_is_late_after_two_minutes() {
        let t0 = Instant::now();
        assert!(!resume_is_late(t0, t0 + Duration::from_secs(119)));
        assert!(resume_is_late(t0, t0 + Duration::from_secs(121)));
    }

    #[test]
    fn the_resume_waits_for_the_share_and_happens_once() {
        let mut gate = ResumeGate::waiting();
        assert_eq!(gate.due(ConnStatus::Unreachable), None, "VPN not up yet");
        assert_eq!(gate.due(ConnStatus::Auth), None);
        assert_eq!(gate.due(ConnStatus::Secondary), Some(false));
        assert_eq!(gate.due(ConnStatus::Ok), None, "only once");
        let mut played_meanwhile = ResumeGate::waiting();
        played_meanwhile.cancel();
        assert_eq!(played_meanwhile.due(ConnStatus::Ok), None);
    }

    #[test]
    fn a_remembered_press_follows_the_last_press() {
        let mut gate = ResumeGate::waiting();
        assert!(gate.press(Press::Toggle), "remembered while waiting");
        assert!(gate.press(Press::Toggle), "a second toggle takes it back");
        assert_eq!(gate.clone().due(ConnStatus::Ok), Some(false));
        assert!(gate.press(Press::Play));
        assert!(gate.press(Press::Play), "Play twice is still Play");
        assert_eq!(gate.clone().due(ConnStatus::Ok), Some(true));
        assert!(gate.press(Press::Pause), "Pause cancels it");
        assert_eq!(gate.clone().due(ConnStatus::Ok), Some(false));
        assert!(gate.press(Press::Toggle));
        assert_eq!(gate.due(ConnStatus::Ok), Some(true));
        // Resumed: presses go to the player again, and leave nothing behind.
        assert!(!gate.press(Press::Toggle));
        assert!(!gate.press(Press::Play));
        assert!(!gate.play_requested);
    }

    #[test]
    fn the_engine_is_loaded_from_the_send_until_a_start_or_a_failure() {
        let skipped = || Event::TrackSkipped {
            id: 3,
            reason: "x".into(),
        };
        assert!(!Engine::Empty.loaded());
        let sent = Engine::Empty.sent();
        assert_eq!(sent, Engine::Loading);
        assert!(sent.loaded(), "a press while it loads pauses, not reopens");
        let started = engine_after(sent, &Event::TrackStarted { id: 7 });
        assert_eq!(
            started,
            Engine::Track(7),
            "also for a track the catalog lost"
        );
        let pos = Event::Position {
            id: 7,
            ms: 10,
            duration_ms: None,
        };
        assert_eq!(engine_after(started, &pos), Engine::Track(7));
        assert_eq!(
            engine_after(started, &Event::Paused(true)),
            Engine::Track(7)
        );
        assert_eq!(
            engine_after(started, &skipped()),
            Engine::Track(7),
            "a prepared next track failing leaves the playing one"
        );
        assert_eq!(engine_after(Engine::Loading, &skipped()), Engine::Empty);
        assert_eq!(engine_after(started, &Event::Idle), Engine::Empty);
        assert_eq!(engine_after(Engine::Loading, &Event::Idle), Engine::Empty);
        assert_eq!(Engine::Track(7).sent(), Engine::Loading);
    }

    #[test]
    fn the_resume_and_a_start_send_while_holding_the_gate() {
        let gate = Mutex::new(ResumeGate::waiting());
        let engine = Mutex::new(Engine::Empty);
        let mut sent = 0;
        resume_with(
            &gate,
            &engine,
            ConnStatus::Ok,
            |pressed| {
                assert!(!pressed);
                Some(Command::Cue(Box::new(Nothing)))
            },
            |_| {
                assert!(gate.try_lock().is_err(), "sent under the gate");
                sent += 1;
            },
        );
        assert_eq!(sent, 1);
        assert_eq!(*lock(&engine), Engine::Loading);
        assert_eq!(*lock(&gate), ResumeGate::default());
        // Only once.
        resume_with(
            &gate,
            &engine,
            ConnStatus::Ok,
            |_| unreachable!(),
            |_| unreachable!(),
        );
        let gate = Mutex::new(ResumeGate::waiting());
        let engine = Mutex::new(Engine::Empty);
        start_with(&gate, &engine, || {
            assert!(gate.try_lock().is_err(), "sent under the gate");
            sent += 1;
        });
        assert_eq!(sent, 2);
        assert!(!lock(&gate).pending, "a start drops the waiting resume");
        assert_eq!(*lock(&engine), Engine::Loading);
        // Nothing due: nothing sent, nothing loading.
        let gate = Mutex::new(ResumeGate::waiting());
        let engine = Mutex::new(Engine::Empty);
        resume_with(&gate, &engine, ConnStatus::Ok, |_| None, |_| unreachable!());
        assert_eq!(*lock(&engine), Engine::Empty);
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
                let r = scan_with(
                    smp_core::DEFAULT_LIBRARY,
                    &src,
                    &mut cat,
                    &mut |_| {},
                    &cancel,
                    &opts,
                );
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

    fn statuses(list: &[(LibraryId, ConnStatus)]) -> StatusPayload {
        StatusPayload::new(
            list.iter()
                .map(|&(id, status)| crate::connection::LibraryStatus {
                    id,
                    name: String::new(),
                    status,
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn the_resume_waits_for_its_server_but_not_for_an_unplugged_disk() {
        let both = statuses(&[(1, ConnStatus::Ok), (2, ConnStatus::Ok)]);
        assert_eq!(resume_decision(&both, Some(2)), (ConnStatus::Ok, false));
        // At login the USB library is up but the main server is not (VPN not up
        // yet): the resume keeps its track and position and waits for the server.
        let login = statuses(&[(1, ConnStatus::Unreachable), (2, ConnStatus::Ok)]);
        let (status, drop) = resume_decision(&login, Some(1));
        assert_eq!((status, drop), (ConnStatus::Unreachable, false));
        let mut gate = ResumeGate::waiting();
        assert_eq!(gate.due(status), None, "still waiting");
        // The server answers: resumed, with its target.
        let up = statuses(&[(1, ConnStatus::Ok), (2, ConnStatus::Ok)]);
        let (status, drop) = resume_decision(&up, Some(1));
        assert!(!drop);
        assert_eq!(gate.due(status), Some(false));
        // The disk with the resume track is not plugged in (a local folder that is
        // missing or unmounted) while the server is up: no wait for a disk that
        // may never come; the target is dropped and the playlist goes on.
        let disk_out = statuses(&[(1, ConnStatus::Ok), (2, ConnStatus::Missing)]);
        assert_eq!(resume_decision(&disk_out, Some(2)), (ConnStatus::Ok, true));
        assert_eq!(resume_decision(&disk_out, Some(1)), (ConnStatus::Ok, false));
        // A wrong password is not a missing disk either: it waits (and says why).
        let auth = statuses(&[(1, ConnStatus::Auth), (2, ConnStatus::Ok)]);
        assert_eq!(resume_decision(&auth, Some(1)), (ConnStatus::Auth, false));
        // Its library not in use (switched off) or unknown: any library that answers.
        assert_eq!(resume_decision(&disk_out, Some(7)), (ConnStatus::Ok, false));
        assert_eq!(resume_decision(&disk_out, None), (ConnStatus::Ok, false));
        // Nothing answers: it waits.
        let down = statuses(&[(1, ConnStatus::Unreachable), (2, ConnStatus::Missing)]);
        assert_eq!(
            resume_decision(&down, Some(2)),
            (ConnStatus::Unreachable, false)
        );
        // The backup server counts as up.
        let backup = statuses(&[(1, ConnStatus::Secondary), (2, ConnStatus::Missing)]);
        assert_eq!(
            resume_decision(&backup, Some(2)),
            (ConnStatus::Secondary, true)
        );
    }

    #[test]
    fn libraries_off_are_the_switched_off_and_the_lost_ones() {
        let c = Catalog::open_in_memory().unwrap();
        for lib in [1, 2, 3, 9] {
            c.upsert_track(lib, "a.mp3", 1, 1, &TrackMeta::default())
                .unwrap();
        }
        let dir = std::path::PathBuf::from("/music");
        let lib = |id, enabled| crate::settings::Library {
            id,
            name: String::new(),
            enabled,
            source: crate::settings::LibrarySource::Local {
                path: dir.display().to_string(),
            },
        };
        let settings = Settings {
            default_library_enabled: false,
            libraries: vec![lib(2, true), lib(3, false)],
            ..Settings::default()
        };
        // 9 is in the catalog but no longer in the settings: hidden, not deleted.
        assert_eq!(libraries_off(&settings, &c), [1, 3, 9]);
    }

    #[test]
    fn libraries_that_cannot_be_scanned_now_are_skipped_with_a_reason() {
        use crate::connection::LibraryConn;
        let dir = tempfile::tempdir().unwrap();
        let there = Arc::new(LibraryConn::local(
            2,
            String::new(),
            &dir.path().display().to_string(),
        ));
        let gone = Arc::new(LibraryConn::local(3, String::new(), "/no/such/disk"));
        let go = AtomicBool::new(false);
        assert_eq!(not_scannable(&there, true, &go), None);
        assert_eq!(
            not_scannable(&gone, true, &go),
            Some(i18n::t("conn.missing")),
            "not touched, its tracks kept"
        );
        // The catalog's update for libraries still to come: only the first library.
        assert_eq!(
            not_scannable(&there, false, &go),
            Some(i18n::t("err.catalog_pending"))
        );
        let main = Arc::new(
            crate::connection::LibraryConn::from_library(
                &Settings {
                    primary: crate::settings::ServerConfig {
                        username: "u".into(),
                        ..Default::default()
                    },
                    ..Settings::default()
                }
                .all_libraries()[0],
                &crate::credentials::MemoryStore::default(),
            )
            .unwrap(),
        );
        assert_eq!(not_scannable(&main, false, &go), None);
    }

    #[test]
    fn a_refresh_reports_libraries_that_looked_empty() {
        let lib = |id| LibraryRef {
            id,
            name: String::new(),
        };
        let empty = smp_core::scan::ScanReport {
            looks_empty: true,
            ..Default::default()
        };
        let fine = smp_core::scan::ScanReport {
            total: 3,
            ..Default::default()
        };
        let done = add_up_scans("en", vec![(lib(2), Ok(empty)), (lib(1), Ok(fine))]);
        assert!(done.ok);
        assert_eq!(done.total, 3);
        assert_eq!(done.looks_empty, [lib(2)]);
    }
}
