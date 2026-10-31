//! Connection to the music libraries: status, lazily connected SMB sources, local
//! folders, and the replaceable source the player and scanner read from.
//!
//! Everything here blocks on the network or the disk and must run on plain
//! threads, never on an async runtime thread.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use fxmp_core::source::{
    FailoverSource, FileEntry, LocalSource, MusicSource, ReadSeek, SmbConfig, SmbSource,
    SourceError, WalkReport, Which,
};
use fxmp_core::{LibraryId, DEFAULT_LIBRARY};
use serde::Serialize;

use crate::credentials::{server_password, CredentialStore};
use crate::i18n;
use crate::settings::{Library, LibrarySource, ServerConfig, Settings};

/// How often an unreachable server or a missing folder is checked again.
pub const RETRY: Duration = Duration::from_secs(15);

/// Connection state of a library, shown in the UI (`connection-status` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnStatus {
    /// Primary server reachable, or the local folder readable.
    Ok,
    /// Primary down, playing from the secondary.
    Secondary,
    Unreachable,
    /// Wrong username or password.
    Auth,
    /// A local folder that is not there or cannot be read (a disk not plugged in).
    Missing,
}

impl ConnStatus {
    /// Translation key of the status text (`ui/locales`).
    pub fn text_key(self) -> &'static str {
        match self {
            ConnStatus::Ok => "conn.ok",
            ConnStatus::Secondary => "conn.secondary",
            ConnStatus::Unreachable => "conn.unreachable",
            ConnStatus::Auth => "conn.auth",
            ConnStatus::Missing => "conn.missing",
        }
    }

    /// The music can be read (perhaps from the backup server).
    pub fn reachable(self) -> bool {
        matches!(self, ConnStatus::Ok | ConnStatus::Secondary)
    }

    /// How bad it is, for the summary of several libraries.
    fn severity(self) -> u8 {
        match self {
            ConnStatus::Ok => 0,
            ConnStatus::Secondary => 1,
            ConnStatus::Missing => 2,
            ConnStatus::Unreachable => 3,
            ConnStatus::Auth => 4,
        }
    }
}

/// The worst of the statuses (`None` for none): what the status dot shows.
pub fn worst(statuses: &[ConnStatus]) -> Option<ConnStatus> {
    statuses.iter().copied().max_by_key(|s| s.severity())
}

/// One library's status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LibraryStatus {
    pub id: LibraryId,
    /// As set (blank: the UI shows a default name).
    pub name: String,
    pub status: ConnStatus,
}

/// Payload of the `connection-status` event: the worst status (the dot) and each
/// library's (its tooltip). The UI words them in its language.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusPayload {
    pub status: ConnStatus,
    pub libraries: Vec<LibraryStatus>,
}

impl StatusPayload {
    /// `None` without libraries.
    pub fn new(libraries: Vec<LibraryStatus>) -> Option<Self> {
        let statuses: Vec<ConnStatus> = libraries.iter().map(|l| l.status).collect();
        Some(Self {
            status: worst(&statuses)?,
            libraries,
        })
    }

    /// The status of library `id`, if it is in use.
    pub fn of(&self, id: LibraryId) -> Option<ConnStatus> {
        self.libraries.iter().find(|l| l.id == id).map(|l| l.status)
    }
}

/// Status from the outcome of checking the primary and (if configured) the secondary.
/// A wrong password is reported even when the other server works: it needs fixing.
pub fn status_of(
    primary: Result<(), SourceError>,
    secondary: Option<Result<(), SourceError>>,
) -> ConnStatus {
    match primary {
        Ok(()) => ConnStatus::Ok,
        Err(SourceError::Auth) => ConnStatus::Auth,
        Err(_) => match secondary {
            Some(Ok(())) => ConnStatus::Secondary,
            Some(Err(SourceError::Auth)) => ConnStatus::Auth,
            _ => ConnStatus::Unreachable,
        },
    }
}

/// Where macOS mounts disks and network volumes.
const VOLUMES: &str = "/Volumes";

/// A local music folder's status: it must be a folder that can be listed, and
/// one on a disk under `/Volumes` must be on that disk (see [`unmounted_volume`]).
pub fn folder_status(path: &std::path::Path) -> ConnStatus {
    folder_status_in(path, std::path::Path::new(VOLUMES))
}

/// [`folder_status`] with the folder disks are mounted in.
pub fn folder_status_in(path: &std::path::Path, volumes: &std::path::Path) -> ConnStatus {
    if unmounted_volume(path, volumes) {
        log::debug!("music folder {}: its disk is not mounted", path.display());
        return ConnStatus::Missing;
    }
    match std::fs::read_dir(path) {
        Ok(_) => ConnStatus::Ok,
        Err(e) => {
            log::debug!("music folder {}: {e}", path.display());
            ConnStatus::Missing
        }
    }
}

/// `path` is on `volumes/<name>`, and that is a plain folder on the same device
/// as `volumes` rather than a mounted disk: an empty folder left after an unclean
/// eject (the disk then mounts as `<name> 1`), which must not count as the disk.
/// A link there (the boot disk's own entry) is not such a folder.
pub fn unmounted_volume(path: &std::path::Path, volumes: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(rest) = path.strip_prefix(volumes) else {
            return false;
        };
        let Some(name) = rest.components().next() else {
            return false;
        };
        let mount = volumes.join(name);
        let (Ok(md), Ok(parent)) = (
            std::fs::symlink_metadata(&mount),
            std::fs::metadata(volumes),
        ) else {
            // Not there at all: listing the folder says so.
            return false;
        };
        !md.file_type().is_symlink() && md.dev() == parent.dev()
    }
    #[cfg(not(unix))]
    {
        let _ = (path, volumes);
        false
    }
}

fn source_error(e: fxmp_core::Error) -> SourceError {
    match e {
        fxmp_core::Error::Source(e) => e,
        other => SourceError::Io(other.to_string()),
    }
}

pub fn smb_config(server: &ServerConfig, password: String) -> SmbConfig {
    SmbConfig {
        host: server.host.trim().to_string(),
        share: server.share.trim().to_string(),
        root: server.root.trim().to_string(),
        username: server.username.trim().to_string(),
        password,
        domain: server
            .domain
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
    }
}

/// Connects to a server once, as "Test connection" does.
pub fn test_server(cfg: SmbConfig) -> Result<(), SourceError> {
    smb_connect(cfg).map(drop)
}

/// What "Test connection" checks for server `which` ("primary"/"secondary") of
/// library `library`: the form's unsaved `server` when given, else the saved one;
/// the form's `password` when non-empty, else the keychain's for that library,
/// host and user.
pub fn test_target(
    settings: &Settings,
    creds: &dyn CredentialStore,
    library: LibraryId,
    which: &str,
    server: Option<ServerConfig>,
    password: Option<String>,
) -> Result<SmbConfig, String> {
    let saved = match settings.library(library).map(|l| l.source) {
        Some(LibrarySource::Smb { primary, secondary }) => Some((primary, secondary)),
        _ => None,
    };
    let server = match (which, server, saved) {
        ("primary" | "secondary", Some(s), _) => s,
        ("primary", None, Some((primary, _))) => primary,
        ("secondary", None, Some((_, secondary))) => {
            secondary.ok_or_else(|| i18n::t("err.no_backup"))?
        }
        ("primary" | "secondary", None, None) => return Err(i18n::t("err.no_server")),
        (other, ..) => return Err(i18n::tf("err.unknown_server", &[("name", other)])),
    };
    let password = password
        .filter(|p| !p.is_empty())
        .or_else(|| server_password(creds, library, &server))
        .unwrap_or_default();
    Ok(smb_config(&server, password))
}

/// An SMB server that connects on first use. After a failed attempt it reports the
/// same error without touching the network until [`RETRY`] has passed, so a dead
/// primary does not delay every track before the failover to the secondary. A wrong
/// password is never retried on its own (that could lock the account): only after
/// [`LazySmb::reconnect`].
#[derive(Clone)]
pub struct LazySmb(Arc<LazyInner>);

type Connector = fn(SmbConfig) -> Result<SmbSource, SourceError>;

struct LazyInner {
    cfg: SmbConfig,
    connector: Connector,
    state: Mutex<LazyState>,
}

#[derive(Default)]
struct LazyState {
    conn: Option<Arc<SmbSource>>,
    failed: Option<(Instant, SourceError)>,
}

impl LazyState {
    /// The remembered failure, if it still stands.
    fn standing_failure(&self, honour_retry: bool) -> Option<SourceError> {
        match &self.failed {
            Some((_, SourceError::Auth)) => Some(SourceError::Auth),
            Some((at, e)) if honour_retry && at.elapsed() < RETRY => Some(e.clone()),
            _ => None,
        }
    }
}

fn smb_connect(cfg: SmbConfig) -> Result<SmbSource, SourceError> {
    SmbSource::connect(cfg).map_err(source_error)
}

impl LazySmb {
    #[cfg(test)]
    fn host(&self) -> &str {
        &self.0.cfg.host
    }

    pub fn new(cfg: SmbConfig) -> Self {
        Self::with_connector(cfg, smb_connect)
    }

    fn with_connector(cfg: SmbConfig, connector: Connector) -> Self {
        Self(Arc::new(LazyInner {
            cfg,
            connector,
            state: Mutex::default(),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LazyState> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn connect(&self, st: &mut LazyState) -> Result<Arc<SmbSource>, SourceError> {
        match (self.0.connector)(self.0.cfg.clone()) {
            Ok(src) => {
                log::info!("connected to {}", src.describe());
                let src = Arc::new(src);
                st.conn = Some(src.clone());
                st.failed = None;
                Ok(src)
            }
            Err(e) => {
                log::warn!("cannot connect to {}: {e}", self.0.cfg.host);
                st.failed = Some((Instant::now(), e.clone()));
                Err(e)
            }
        }
    }

    fn get(&self) -> Result<Arc<SmbSource>, SourceError> {
        let mut st = self.state();
        if let Some(src) = &st.conn {
            return Ok(src.clone());
        }
        if let Some(e) = st.standing_failure(true) {
            return Err(e);
        }
        self.connect(&mut st)
    }

    /// Checks the server now: connects if not connected yet (unless the password was
    /// rejected), otherwise looks for one file.
    pub fn check(&self) -> Result<(), SourceError> {
        let existing = self.state().conn.clone();
        match existing {
            Some(src) => src.walk_limited(1, &mut |_| true).map(drop),
            None => {
                let mut st = self.state();
                if let Some(e) = st.standing_failure(false) {
                    return Err(e);
                }
                self.connect(&mut st).map(drop)
            }
        }
    }

    /// Forgets a remembered failure (including a rejected password), so the next use
    /// connects again.
    pub fn reconnect(&self) {
        self.state().failed = None;
    }

    #[cfg(test)]
    pub fn config(&self) -> &SmbConfig {
        &self.0.cfg
    }
}

impl MusicSource for LazySmb {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
        self.get()?.walk(on_entry)
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        self.get()?.open(rel_path)
    }

    fn describe(&self) -> String {
        let c = &self.0.cfg;
        format!("smb://{}/{}/{}", c.host, c.share, c.root)
    }
}

/// One SMB library's servers: primary, optional secondary, and the failover pair
/// over them. A server switched off in the settings is left out.
pub struct Servers {
    /// `None`: switched off.
    pub primary: Option<LazySmb>,
    pub secondary: Option<LazySmb>,
    pub failover: Arc<FailoverSource>,
}

/// Once the secondary answers, how much longer a check waits for the primary
/// before it reports the backup: a working primary is usually that quick, a dead
/// one takes its whole connect timeout. A primary answering later (a disk that
/// had to spin up) still takes over at once.
pub const PRIMARY_GRACE: Duration = Duration::from_secs(3);

impl Servers {
    /// Builds the servers of library `library`, with passwords from `creds`. Does
    /// not connect. `None` when both are switched off.
    pub fn new(
        library: LibraryId,
        primary: &ServerConfig,
        secondary: Option<&ServerConfig>,
        creds: &dyn CredentialStore,
    ) -> Option<Self> {
        let lazy = |s: &ServerConfig| {
            let password = server_password(creds, library, s).unwrap_or_default();
            LazySmb::new(smb_config(s, password))
        };
        let primary = Some(primary).filter(|s| s.enabled).map(lazy);
        let secondary = secondary.filter(|s| s.enabled).map(lazy);
        let boxed = |s: &LazySmb| Box::new(s.clone()) as Box<dyn MusicSource>;
        let failover = match (&primary, &secondary) {
            (Some(p), s) => FailoverSource::new(boxed(p), s.as_ref().map(boxed)),
            (None, Some(s)) => FailoverSource::backup_only(boxed(s)),
            (None, None) => return None,
        };
        Some(Self {
            primary,
            secondary,
            failover: Arc::new(failover),
        })
    }

    /// Checks both servers at once, so a dead primary's connect timeout does not
    /// hold up the backup (see [`check_pair`]), and opens files from the one that
    /// answered from now on: the primary whenever it does. A library whose primary
    /// is switched off is [`ConnStatus::Ok`] while its backup answers: that is the
    /// owner's choice, not a fault.
    pub fn probe(&self) -> ConnStatus {
        let (primary, secondary) = match (&self.primary, &self.secondary) {
            (Some(p), Some(s)) => {
                let (p, s) = check_pair(p.clone(), s.clone(), &self.failover, PRIMARY_GRACE);
                (Some(p), s)
            }
            (Some(p), None) => (Some(p.check()), None),
            (None, Some(s)) => (None, Some(s.check())),
            (None, None) => (None, None),
        };
        if let Some(which) = answered(primary.as_ref(), secondary.as_ref()) {
            self.failover.prefer(which);
        }
        match primary {
            Some(primary) => status_of(primary, secondary),
            None => status_of(secondary.unwrap_or(Err(SourceError::Unreachable)), None),
        }
    }
}

/// The server to open files from after a check: the primary whenever it answered.
fn answered(
    primary: Option<&Result<(), SourceError>>,
    secondary: Option<&Result<(), SourceError>>,
) -> Option<Which> {
    if primary.is_some_and(Result::is_ok) {
        Some(Which::Primary)
    } else if secondary.is_some_and(Result::is_ok) {
        Some(Which::Secondary)
    } else {
        None
    }
}

/// Checks `primary` and `secondary` on their own threads. Waits for the primary;
/// the secondary's answer counts only when the primary fails (`None` otherwise).
/// Once the secondary works, the primary gets `grace` more to answer, after which
/// it counts as timed out. Its check goes on: when it answers, `failover` opens
/// files from it again.
pub fn check_pair(
    primary: LazySmb,
    secondary: LazySmb,
    failover: &Arc<FailoverSource>,
    grace: Duration,
) -> (Result<(), SourceError>, Option<Result<(), SourceError>>) {
    let failover = failover.clone();
    let primary = move || {
        let r = primary.check();
        if r.is_ok() {
            failover.prefer(Which::Primary);
        }
        r
    };
    check_pair_with(primary, move || secondary.check(), grace)
}

fn check_pair_with(
    primary: impl FnOnce() -> Result<(), SourceError> + Send + 'static,
    secondary: impl FnOnce() -> Result<(), SourceError> + Send + 'static,
    grace: Duration,
) -> (Result<(), SourceError>, Option<Result<(), SourceError>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    for (which, check) in [
        (
            Which::Primary,
            Box::new(primary) as Box<dyn FnOnce() -> _ + Send>,
        ),
        (Which::Secondary, Box::new(secondary)),
    ] {
        let thread_tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name("server-check".into())
            .spawn(move || {
                let _ = thread_tx.send((which, check()));
            });
        if let Err(e) = spawned {
            log::warn!("cannot start a server check: {e}");
            let _ = tx.send((which, Err(SourceError::Unreachable)));
        }
    }
    drop(tx);
    let mut primary = None;
    let mut secondary: Option<Result<(), SourceError>> = None;
    let mut deadline: Option<Instant> = None;
    loop {
        let got = match deadline {
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(got) => got,
                Err(_) => break,
            },
            None => match rx.recv() {
                Ok(got) => got,
                Err(_) => break,
            },
        };
        match got {
            (Which::Primary, Ok(())) => return (Ok(()), None),
            (Which::Primary, Err(SourceError::Auth)) => return (Err(SourceError::Auth), None),
            (Which::Primary, r) => primary = Some(r),
            (Which::Secondary, r) => {
                if r.is_ok() {
                    deadline = Some(Instant::now() + grace);
                }
                secondary = Some(r);
            }
        }
        if primary.is_some() && secondary.is_some() {
            break;
        }
    }
    (
        primary.unwrap_or(Err(SourceError::Timeout)),
        Some(secondary.unwrap_or(Err(SourceError::Unreachable))),
    )
}

/// A library's source.
pub enum LibraryKind {
    Smb(Servers),
    Local {
        root: PathBuf,
        source: LocalSource,
    },
    /// A library for tests whose check runs `probe` (reads an empty folder).
    #[cfg(test)]
    Fake {
        probe: Box<dyn Fn() -> ConnStatus + Send + Sync>,
        source: LocalSource,
    },
}

/// A library in use: its id, name and source.
pub struct LibraryConn {
    pub id: LibraryId,
    pub name: String,
    pub kind: LibraryKind,
    /// Its check in flight, if any (see [`CheckSlot`]).
    check: Arc<CheckSlot>,
}

/// At most one check of a library at a time: a check that hangs (a network volume
/// whose server stopped answering can block a folder listing for good) is not
/// joined by a new thread every round.
#[derive(Default)]
pub struct CheckSlot {
    running: std::sync::atomic::AtomicBool,
}

impl CheckSlot {
    /// Runs `probe` on its own thread, unless the previous check is still running
    /// (then `None`). The result comes through the returned channel.
    pub fn start(
        self: &Arc<Self>,
        probe: impl FnOnce() -> ConnStatus + Send + 'static,
    ) -> Option<Receiver<ConnStatus>> {
        use std::sync::atomic::Ordering;
        if self.running.swap(true, Ordering::SeqCst) {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = self.clone();
        let spawned = std::thread::Builder::new()
            .name("library-check".into())
            .spawn(move || {
                let status = probe();
                slot.running.store(false, Ordering::SeqCst);
                let _ = tx.send(status);
            });
        match spawned {
            Ok(_) => Some(rx),
            Err(e) => {
                log::warn!("cannot start a library check: {e}");
                self.running.store(false, Ordering::SeqCst);
                None
            }
        }
    }
}

/// Waits for a started check until `deadline`, giving up at once when `cancel`
/// is set; `None` if it did not answer.
pub fn wait_check(
    rx: &Receiver<ConnStatus>,
    deadline: Instant,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<ConnStatus> {
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        // An answer already there counts, even past the deadline (that may have
        // gone on waiting for another library).
        if let Ok(status) = rx.try_recv() {
            return Some(status);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(status) => return Some(status),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

impl LibraryConn {
    /// A local-folder library (no password, no keychain).
    pub fn local(id: LibraryId, name: String, path: &str) -> Self {
        let root = PathBuf::from(path.trim());
        Self {
            id,
            name,
            kind: LibraryKind::Local {
                source: LocalSource::new(root.clone()),
                root,
            },
            check: Arc::default(),
        }
    }

    /// The library as set in `library`, if it is set up: an SMB server with a
    /// user and at least one server switched on, or a folder. Only an SMB library reads `creds`.
    pub fn from_library(library: &Library, creds: &dyn CredentialStore) -> Option<Self> {
        match &library.source {
            LibrarySource::Smb { primary, secondary } => {
                if primary.username.trim().is_empty() {
                    return None;
                }
                Some(Self {
                    id: library.id,
                    name: library.name.clone(),
                    kind: LibraryKind::Smb(Servers::new(
                        library.id,
                        primary,
                        secondary.as_ref(),
                        creds,
                    )?),
                    check: Arc::default(),
                })
            }
            LibrarySource::Local { path } if !path.trim().is_empty() => {
                Some(Self::local(library.id, library.name.clone(), path))
            }
            LibrarySource::Local { .. } => None,
        }
    }

    /// What the scanner and the player read.
    pub fn source(&self) -> &dyn MusicSource {
        match &self.kind {
            LibraryKind::Smb(s) => s.failover.as_ref(),
            LibraryKind::Local { source, .. } => source,
            #[cfg(test)]
            LibraryKind::Fake { source, .. } => source,
        }
    }

    /// Starts a check on its own thread, unless the previous one still runs.
    pub fn start_check(self: &Arc<Self>) -> Option<Receiver<ConnStatus>> {
        let lib = self.clone();
        self.check.start(move || lib.probe())
    }

    /// Checks the library now.
    pub fn probe(&self) -> ConnStatus {
        match &self.kind {
            LibraryKind::Smb(s) => s.probe(),
            LibraryKind::Local { root, .. } => folder_status(root),
            #[cfg(test)]
            LibraryKind::Fake { probe, .. } => probe(),
        }
    }

    /// The server that served the last read (an SMB library's).
    pub fn active_server(&self) -> Option<Which> {
        match &self.kind {
            LibraryKind::Smb(s) => Some(s.failover.active()),
            _ => None,
        }
    }

    pub fn servers(&self) -> Option<&Servers> {
        match &self.kind {
            LibraryKind::Smb(s) => Some(s),
            _ => None,
        }
    }

    /// A library whose check runs `probe` (tests).
    #[cfg(test)]
    pub fn fake(id: LibraryId, probe: impl Fn() -> ConnStatus + Send + Sync + 'static) -> Self {
        Self {
            id,
            name: String::new(),
            kind: LibraryKind::Fake {
                probe: Box::new(probe),
                source: LocalSource::new(std::env::temp_dir().join("smp-fake-library")),
            },
            check: Arc::default(),
        }
    }
}

/// Longest wait for the libraries' checks: above an SMB library's worst case
/// (a listing that times out, a reconnect, then the backup's connect). One that
/// has not answered by then counts as unreachable (its check goes on).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(45);

/// The libraries' statuses from their started checks (`None`: the previous check
/// still runs), waiting at most `timeout` in all, so a dead server's connect
/// timeout does not hold up the others (a disk plugged back in is seen at once).
/// A check that did not answer, or was still running, is
/// [`ConnStatus::Unreachable`]. In the given order.
pub fn probe_all(
    checks: Vec<(LibraryId, String, Option<Receiver<ConnStatus>>)>,
    timeout: Duration,
) -> Vec<LibraryStatus> {
    let deadline = Instant::now() + timeout;
    let never = std::sync::atomic::AtomicBool::new(false);
    checks
        .into_iter()
        .map(|(id, name, rx)| {
            let status = match rx {
                Some(rx) => wait_check(&rx, deadline, &never).unwrap_or_else(|| {
                    log::warn!("library {id}: check did not answer within {timeout:?}");
                    ConnStatus::Unreachable
                }),
                None => {
                    log::warn!("library {id}: the previous check still runs");
                    ConnStatus::Unreachable
                }
            };
            LibraryStatus { id, name, status }
        })
        .collect()
}

/// The libraries in use: switched on and set up, in the settings' order.
pub struct Libraries {
    list: Vec<Arc<LibraryConn>>,
}

impl Libraries {
    /// Builds the libraries from settings; only SMB ones read their passwords from
    /// `creds`. Does not connect.
    pub fn from_settings(settings: &Settings, creds: &dyn CredentialStore) -> Self {
        Self {
            list: settings
                .all_libraries()
                .iter()
                .filter(|l| l.enabled)
                .filter_map(|l| LibraryConn::from_library(l, creds))
                .map(Arc::new)
                .collect(),
        }
    }

    pub fn get(&self, id: LibraryId) -> Option<&Arc<LibraryConn>> {
        self.list.iter().find(|l| l.id == id)
    }

    /// These libraries (tests).
    #[cfg(test)]
    pub fn from_conns(list: Vec<LibraryConn>) -> Self {
        Self {
            list: list.into_iter().map(Arc::new).collect(),
        }
    }

    #[cfg(test)]
    pub fn ids(&self) -> Vec<LibraryId> {
        self.list.iter().map(|l| l.id).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<LibraryConn>> {
        self.list.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Checks every library at once (see [`probe_all`]).
    pub fn probe(&self) -> Option<StatusPayload> {
        let checks = self
            .list
            .iter()
            .map(|l| (l.id, l.name.clone(), l.start_check()))
            .collect();
        StatusPayload::new(probe_all(checks, PROBE_TIMEOUT))
    }
}

/// The source the player reads from (each track through its library) and the
/// libraries the scanner walks; replaced when the settings change.
#[derive(Default)]
pub struct AppSource {
    libraries: RwLock<Option<Arc<Libraries>>>,
}

impl AppSource {
    /// `None` (or no library in use): nothing is set up.
    pub fn set(&self, libraries: Option<Libraries>) {
        *self.libraries.write().unwrap_or_else(|e| e.into_inner()) =
            libraries.filter(|l| !l.is_empty()).map(Arc::new);
    }

    /// Sets `libraries` unless some are already set (settings saved meanwhile are
    /// newer). Returns whether they were taken.
    pub fn set_if_unset(&self, libraries: Libraries) -> bool {
        let mut slot = self.libraries.write().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(Arc::new(libraries)).filter(|l| !l.is_empty());
        true
    }

    pub fn libraries(&self) -> Option<Arc<Libraries>> {
        self.libraries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn library(&self, id: LibraryId) -> Result<Arc<LibraryConn>, SourceError> {
        self.libraries()
            .and_then(|l| l.get(id).cloned())
            .ok_or(SourceError::Unreachable)
    }
}

impl MusicSource for AppSource {
    /// The first library's (the scanner walks each library itself).
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
        self.library(DEFAULT_LIBRARY)?.source().walk(on_entry)
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        self.open_in(DEFAULT_LIBRARY, rel_path)
    }

    /// Through the track's library; a library not in use (switched off meanwhile,
    /// removed) is unreachable.
    fn open_in(
        &self,
        library: LibraryId,
        rel_path: &str,
    ) -> Result<Box<dyn ReadSeek>, SourceError> {
        self.library(library)?.source().open(rel_path)
    }

    fn describe(&self) -> String {
        match self.libraries() {
            Some(l) => l
                .iter()
                .map(|l| l.source().describe())
                .collect::<Vec<_>>()
                .join(", "),
            None => "(not configured)".into(),
        }
    }
}

/// Keeps the libraries' status current. Probes every library in use when woken
/// (settings saved, a track failed, playback moved to the secondary) and, while one
/// is down or on its backup, every [`RETRY`] (a disk plugged back in is seen within
/// that). A wrong password is not retried until the settings change, so the account
/// is not locked out. Every status from a wake is reported; timed retries report
/// only changes. Returns when all wake senders are gone.
pub fn monitor(source: Arc<AppSource>, wake: Receiver<()>, mut report: impl FnMut(&StatusPayload)) {
    let mut last: Option<StatusPayload> = None;
    let mut woken = true;
    loop {
        last = match source.libraries().and_then(|l| l.probe()) {
            Some(status) => {
                if woken || last.as_ref() != Some(&status) {
                    report(&status);
                }
                Some(status)
            }
            None => None,
        };
        let retry = last.as_ref().is_some_and(|p| {
            p.libraries
                .iter()
                .any(|l| !matches!(l.status, ConnStatus::Ok | ConnStatus::Auth))
        });
        let got = if retry {
            wake.recv_timeout(RETRY)
        } else {
            wake.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        woken = match got {
            Ok(()) => true,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Several wakes in a row need only one probe.
        while wake.try_recv().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;

    fn primary_host(src: &AppSource) -> String {
        let libs = src.libraries().unwrap();
        let lib = libs.get(DEFAULT_LIBRARY).unwrap();
        lib.servers()
            .unwrap()
            .primary
            .as_ref()
            .unwrap()
            .host()
            .to_string()
    }

    #[test]
    fn set_if_unset_never_replaces_newer_servers() {
        let creds = MemoryStore::default();
        let host = |h: &str| Settings {
            primary: ServerConfig {
                host: h.into(),
                username: "music".into(),
                ..ServerConfig::default()
            },
            ..Settings::default()
        };
        let src = AppSource::default();
        // Startup credentials arrive first: taken.
        assert!(src.set_if_unset(Libraries::from_settings(&host("a"), &creds)));
        assert_eq!(primary_host(&src), "a");
        // Settings were saved meanwhile: the late startup value is dropped.
        src.set(Some(Libraries::from_settings(&host("b"), &creds)));
        assert!(!src.set_if_unset(Libraries::from_settings(&host("a"), &creds)));
        assert_eq!(primary_host(&src), "b");
    }

    fn after(
        delay_ms: u64,
        r: Result<(), SourceError>,
    ) -> impl FnOnce() -> Result<(), SourceError> + Send + 'static {
        move || {
            std::thread::sleep(Duration::from_millis(delay_ms));
            r
        }
    }

    #[test]
    fn a_dead_primary_does_not_hold_up_a_working_backup() {
        let start = Instant::now();
        let got = check_pair_with(
            after(5_000, Err(SourceError::Timeout)),
            after(0, Ok(())),
            Duration::from_millis(100),
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(got, (Err(SourceError::Timeout), Some(Ok(()))));
        assert_eq!(status_of(got.0, got.1), ConnStatus::Secondary);
    }

    #[test]
    fn a_primary_answering_within_the_grace_wins() {
        let got = check_pair_with(after(50, Ok(())), after(0, Ok(())), Duration::from_secs(2));
        assert_eq!(got, (Ok(()), None));
    }

    #[test]
    fn a_failed_primary_waits_for_the_backup() {
        let got = check_pair_with(
            after(0, Err(SourceError::Unreachable)),
            after(100, Ok(())),
            Duration::from_millis(10),
        );
        assert_eq!(got, (Err(SourceError::Unreachable), Some(Ok(()))));
    }

    #[test]
    fn a_failed_backup_waits_for_the_primary() {
        let got = check_pair_with(
            after(100, Ok(())),
            after(0, Err(SourceError::Unreachable)),
            Duration::from_millis(10),
        );
        assert_eq!(got, (Ok(()), None));
        let got = check_pair_with(
            after(100, Err(SourceError::Timeout)),
            after(0, Err(SourceError::Unreachable)),
            Duration::from_millis(10),
        );
        assert_eq!(
            got,
            (
                Err(SourceError::Timeout),
                Some(Err(SourceError::Unreachable))
            )
        );
    }

    #[test]
    fn a_rejected_primary_password_is_reported_at_once() {
        let got = check_pair_with(
            after(0, Err(SourceError::Auth)),
            after(5_000, Ok(())),
            Duration::from_millis(10),
        );
        assert_eq!(got, (Err(SourceError::Auth), None));
    }

    #[test]
    fn files_come_from_the_server_that_answered() {
        let (ok, down) = (Ok(()), Err(SourceError::Timeout));
        assert_eq!(answered(Some(&ok), None), Some(Which::Primary));
        assert_eq!(answered(Some(&ok), Some(&ok)), Some(Which::Primary));
        assert_eq!(answered(Some(&down), Some(&ok)), Some(Which::Secondary));
        assert_eq!(answered(None, Some(&ok)), Some(Which::Secondary));
        assert_eq!(answered(Some(&down), Some(&down)), None, "keep the last");
        assert_eq!(answered(Some(&down), None), None);
    }

    fn mirrors(primary_on: bool, secondary_on: bool) -> (ServerConfig, ServerConfig) {
        let server = |host: &str, enabled| ServerConfig {
            host: host.into(),
            username: "music".into(),
            enabled,
            ..ServerConfig::default()
        };
        (server("a", primary_on), server("b", secondary_on))
    }

    #[test]
    fn a_switched_off_server_is_left_out() {
        let creds = MemoryStore::default();
        let (p, s) = mirrors(false, true);
        let servers = Servers::new(1, &p, Some(&s), &creds).unwrap();
        assert!(servers.primary.is_none());
        assert_eq!(servers.secondary.as_ref().unwrap().host(), "b");
        assert_eq!(servers.failover.active(), Which::Secondary);

        let (p, s) = mirrors(true, false);
        let servers = Servers::new(1, &p, Some(&s), &creds).unwrap();
        assert_eq!(servers.primary.as_ref().unwrap().host(), "a");
        assert!(servers.secondary.is_none());
        assert_eq!(servers.failover.active(), Which::Primary);

        let (p, s) = mirrors(false, false);
        assert!(Servers::new(1, &p, Some(&s), &creds).is_none());
        // Without a backup, a switched-off primary leaves nothing to read.
        assert!(Servers::new(1, &p, None, &creds).is_none());
    }

    #[test]
    fn old_settings_have_their_servers_switched_on() {
        let s: ServerConfig = serde_json::from_str(r#"{"host":"h","username":"u"}"#).unwrap();
        assert!(s.enabled);
    }

    #[test]
    fn connection_status_maps_errors() {
        use SourceError::*;
        assert_eq!(status_of(Ok(()), None), ConnStatus::Ok);
        assert_eq!(status_of(Ok(()), Some(Err(Unreachable))), ConnStatus::Ok);
        assert_eq!(status_of(Err(Unreachable), None), ConnStatus::Unreachable);
        assert_eq!(status_of(Err(Timeout), None), ConnStatus::Unreachable);
        assert_eq!(
            status_of(Err(Io("x".into())), None),
            ConnStatus::Unreachable
        );
        assert_eq!(status_of(Err(NotFound), None), ConnStatus::Unreachable);
        assert_eq!(status_of(Err(Auth), None), ConnStatus::Auth);
        assert_eq!(status_of(Err(Auth), Some(Ok(()))), ConnStatus::Auth);
        assert_eq!(
            status_of(Err(Unreachable), Some(Ok(()))),
            ConnStatus::Secondary
        );
        assert_eq!(status_of(Err(Timeout), Some(Ok(()))), ConnStatus::Secondary);
        assert_eq!(
            status_of(Err(Unreachable), Some(Err(Unreachable))),
            ConnStatus::Unreachable
        );
        assert_eq!(
            status_of(Err(Unreachable), Some(Err(Auth))),
            ConnStatus::Auth
        );
    }

    fn cfg() -> SmbConfig {
        smb_config(&ServerConfig::default(), String::new())
    }

    static AUTH_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn auth_fails(_: SmbConfig) -> Result<SmbSource, SourceError> {
        AUTH_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(SourceError::Auth)
    }

    #[test]
    fn rejected_password_is_not_retried_until_reconnect() {
        let calls = || AUTH_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let lazy = LazySmb::with_connector(cfg(), auth_fails);
        assert_eq!(lazy.open("a.mp3").err(), Some(SourceError::Auth));
        assert_eq!(lazy.open("a.mp3").err(), Some(SourceError::Auth));
        assert_eq!(
            lazy.check(),
            Err(SourceError::Auth),
            "a probe does not retry"
        );
        // Even after the retry interval.
        lazy.state().failed = Some((Instant::now() - RETRY * 2, SourceError::Auth));
        assert_eq!(lazy.walk(&mut |_| {}), Err(SourceError::Auth));
        assert_eq!(calls(), 1);
        lazy.reconnect();
        assert_eq!(lazy.open("a.mp3").err(), Some(SourceError::Auth));
        assert_eq!(calls(), 2);
    }

    static DOWN_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn unreachable(_: SmbConfig) -> Result<SmbSource, SourceError> {
        DOWN_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(SourceError::Unreachable)
    }

    #[test]
    fn unreachable_server_is_retried_after_interval_or_by_probe() {
        let calls = || DOWN_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let lazy = LazySmb::with_connector(cfg(), unreachable);
        assert_eq!(lazy.open("a.mp3").err(), Some(SourceError::Unreachable));
        assert_eq!(lazy.open("a.mp3").err(), Some(SourceError::Unreachable));
        assert_eq!(calls(), 1, "cached within the retry interval");
        assert_eq!(lazy.check(), Err(SourceError::Unreachable));
        assert_eq!(calls(), 2, "a probe retries");
        lazy.state().failed = Some((Instant::now() - RETRY * 2, SourceError::Unreachable));
        assert_eq!(lazy.open("a.mp3").err(), Some(SourceError::Unreachable));
        assert_eq!(calls(), 3, "retried after the interval");
    }

    #[test]
    fn test_target_uses_form_values_or_saved_settings() {
        let creds = MemoryStore::default();
        creds.put(1, "10.111.100.2", "music", "saved-pw");
        let settings = Settings {
            primary: ServerConfig {
                host: "10.111.100.2".into(),
                share: "F".into(),
                root: "MP3".into(),
                username: "music".into(),
                ..ServerConfig::default()
            },
            ..Settings::default()
        };

        // Saved settings, keychain password.
        let t = test_target(&settings, &creds, 1, "primary", None, None).unwrap();
        assert_eq!(
            (t.host.as_str(), t.password.as_str()),
            ("10.111.100.2", "saved-pw")
        );
        let t = test_target(&settings, &creds, 1, "primary", None, Some(String::new())).unwrap();
        assert_eq!(
            t.password, "saved-pw",
            "an empty form password keeps the stored one"
        );

        // Unsaved form values, typed password.
        let form = ServerConfig {
            host: "10.111.0.2".into(),
            share: "music".into(),
            root: "MP3".into(),
            username: "backup".into(),
            domain: None,
            enabled: true,
        };
        let t = test_target(
            &settings,
            &creds,
            1,
            "secondary",
            Some(form.clone()),
            Some("typed".into()),
        )
        .unwrap();
        assert_eq!((t.host.as_str(), t.share.as_str()), ("10.111.0.2", "music"));
        assert_eq!(
            (t.username.as_str(), t.password.as_str()),
            ("backup", "typed")
        );
        let t = test_target(&settings, &creds, 1, "secondary", Some(form), None).unwrap();
        assert_eq!(t.password, "", "nothing stored for that host/user");

        assert!(test_target(&settings, &creds, 1, "secondary", None, None).is_err());
        assert!(test_target(&settings, &creds, 1, "other", None, None).is_err());
    }

    #[test]
    fn status_payload_carries_the_status_code_and_texts_are_translated() {
        let one = StatusPayload::new(vec![LibraryStatus {
            id: 1,
            name: String::new(),
            status: ConnStatus::Unreachable,
        }]);
        let json = serde_json::to_string(&one).unwrap();
        assert_eq!(
            json,
            r#"{"status":"unreachable","libraries":[{"id":1,"name":"","status":"unreachable"}]}"#
        );
        let text = |s: ConnStatus, lang| crate::i18n::tr(lang, s.text_key());
        assert_eq!(text(ConnStatus::Ok, "en"), "Connected");
        assert_eq!(
            text(ConnStatus::Secondary, "en"),
            "Connected to the backup server"
        );
        assert_eq!(text(ConnStatus::Auth, "en"), "Wrong username or password");
        assert_eq!(
            text(ConnStatus::Unreachable, "hr"),
            "Disk nije dostupan. Je li uključen WireGuard/VPN?"
        );
    }

    #[test]
    fn servers_use_keychain_password_per_server() {
        let creds = MemoryStore::default();
        creds.put(1, "10.111.100.2", "music", "pw1");
        creds.put(1, "10.111.0.2", "backup", "pw2");
        let settings = Settings {
            primary: ServerConfig {
                host: "10.111.100.2".into(),
                share: "F".into(),
                root: "MP3".into(),
                username: " music ".into(),
                domain: Some("  ".into()),
                enabled: true,
            },
            secondary: Some(ServerConfig {
                host: "10.111.0.2".into(),
                share: "music".into(),
                root: "/MP3".into(),
                username: "backup".into(),
                domain: Some("WORKGROUP".into()),
                enabled: true,
            }),
            ..Settings::default()
        };
        let libs = Libraries::from_settings(&settings, &creds);
        let servers = libs.get(1).unwrap().servers().unwrap();
        let p = servers.primary.as_ref().unwrap().config();
        assert_eq!(
            (p.host.as_str(), p.username.as_str()),
            ("10.111.100.2", "music")
        );
        assert_eq!(p.password, "pw1");
        assert_eq!(p.domain, None);
        let s = servers.secondary.as_ref().unwrap().config();
        assert_eq!((s.share.as_str(), s.root.as_str()), ("music", "/MP3"));
        assert_eq!(s.password, "pw2");
        assert_eq!(s.domain.as_deref(), Some("WORKGROUP"));
    }

    #[test]
    fn unconfigured_app_source_is_unreachable() {
        let src = AppSource::default();
        assert_eq!(src.open("a.mp3").err(), Some(SourceError::Unreachable));
        assert_eq!(src.walk(&mut |_| {}), Err(SourceError::Unreachable));
        // No library in use counts as not set up.
        src.set(Some(Libraries::from_settings(
            &Settings::default(),
            &MemoryStore::default(),
        )));
        assert!(src.libraries().is_none());
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            libraries: vec![local_library(2, dir.path())],
            ..Settings::default()
        };
        src.set(Some(Libraries::from_settings(
            &settings,
            &MemoryStore::default(),
        )));
        assert_eq!(
            src.open_in(5, "a.mp3").err(),
            Some(SourceError::Unreachable),
            "a library not in use"
        );
    }

    fn local_library(id: LibraryId, path: &std::path::Path) -> Library {
        Library {
            id,
            name: format!("Disk {id}"),
            enabled: true,
            source: LibrarySource::Local {
                path: path.display().to_string(),
            },
        }
    }

    #[test]
    fn local_libraries_never_touch_the_keychain() {
        let creds = MemoryStore::default();
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../crates/core/tests/fixtures/tone.mp3"),
            dir.path().join("a.mp3"),
        )
        .unwrap();
        // The first library not set up (no user), so only a local one is in use.
        let settings = Settings {
            libraries: vec![local_library(2, dir.path())],
            ..Settings::default()
        };
        let libs = Libraries::from_settings(&settings, &creds);
        assert_eq!(libs.ids(), [2]);
        let lib = libs.get(2).unwrap();
        assert_eq!(lib.probe(), ConnStatus::Ok);
        let src = AppSource::default();
        src.set(Some(libs));
        src.open_in(2, "a.mp3").unwrap();
        src.libraries()
            .unwrap()
            .get(2)
            .unwrap()
            .source()
            .walk(&mut |_| {})
            .unwrap();
        let test = test_target(&settings, &creds, 2, "primary", None, None);
        assert!(test.is_err(), "no server to test");
        assert_eq!(creds.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_missing_folder_is_reported_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("Volumes/USB disk");
        let lib = LibraryConn::local(3, "USB".into(), &disk.display().to_string());
        assert_eq!(lib.probe(), ConnStatus::Missing, "not plugged in");
        std::fs::create_dir_all(&disk).unwrap();
        assert_eq!(lib.probe(), ConnStatus::Ok, "plugged in");
        // A file where the folder should be is no music folder either.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let lib = LibraryConn::local(4, String::new(), &file.display().to_string());
        assert_eq!(lib.probe(), ConnStatus::Missing);
        assert_eq!(
            crate::i18n::tr("en", ConnStatus::Missing.text_key()),
            "Music folder not found. Is the disk connected?"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_folder_on_a_volume_that_is_not_mounted_is_missing() {
        // A plain folder where a disk would be mounted (left after an unclean
        // eject): same device as the volumes folder, so the disk is not there.
        let volumes = tempfile::tempdir().unwrap();
        let stale = volumes.path().join("USB/Music");
        std::fs::create_dir_all(&stale).unwrap();
        assert!(unmounted_volume(&stale, volumes.path()));
        assert_eq!(
            folder_status_in(&stale, volumes.path()),
            ConnStatus::Missing
        );
        // A real mount point: /dev is a file system of its own under /.
        assert!(!unmounted_volume(
            std::path::Path::new("/dev"),
            std::path::Path::new("/")
        ));
        // A folder not under the volumes folder is only checked for being listable.
        let home = tempfile::tempdir().unwrap();
        assert!(!unmounted_volume(home.path(), volumes.path()));
        assert_eq!(
            folder_status_in(home.path(), volumes.path()),
            ConnStatus::Ok
        );
        // A link in the volumes folder (the boot disk's own entry) is not a stale folder.
        std::os::unix::fs::symlink(home.path(), volumes.path().join("Boot")).unwrap();
        assert!(!unmounted_volume(
            &volumes.path().join("Boot"),
            volumes.path()
        ));
    }

    #[test]
    fn a_slow_library_does_not_hold_up_the_others() {
        let slot = || Arc::new(CheckSlot::default());
        let checks = vec![
            (
                1,
                "NAS".to_string(),
                slot().start(|| {
                    std::thread::sleep(Duration::from_secs(3));
                    ConnStatus::Ok
                }),
            ),
            (2, "USB".into(), slot().start(|| ConnStatus::Missing)),
            (3, String::new(), slot().start(|| ConnStatus::Ok)),
        ];
        let started = Instant::now();
        let got = probe_all(checks, Duration::from_millis(300));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        let got: Vec<(LibraryId, ConnStatus)> = got.iter().map(|l| (l.id, l.status)).collect();
        assert_eq!(
            got,
            [
                (1, ConnStatus::Unreachable),
                (2, ConnStatus::Missing),
                (3, ConnStatus::Ok)
            ],
            "in order; the one that did not answer in time is unreachable"
        );
        assert!(
            PROBE_TIMEOUT >= Duration::from_secs(45),
            "covers a failover check"
        );
    }

    #[test]
    fn a_hung_check_is_not_joined_by_more_and_waiting_can_be_cancelled() {
        let slot = Arc::new(CheckSlot::default());
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let rx = slot
            .start(move || {
                let _ = hold.recv();
                ConnStatus::Ok
            })
            .unwrap();
        // The next rounds start no thread while it hangs, and say it is unreachable.
        assert!(slot.start(|| ConnStatus::Ok).is_none());
        let got = probe_all(vec![(4, String::new(), None)], Duration::from_secs(1));
        assert_eq!(got[0].status, ConnStatus::Unreachable);
        // A refresh waiting on it is let go by Cancel at once.
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let t = Instant::now();
        assert_eq!(wait_check(&rx, t + Duration::from_secs(30), &cancel), None);
        assert!(t.elapsed() < Duration::from_millis(500));
        // It ends: the next round checks again.
        release.send(()).unwrap();
        let never = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(
            wait_check(&rx, Instant::now() + Duration::from_secs(5), &never),
            Some(ConnStatus::Ok)
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let again = loop {
            if let Some(rx) = slot.start(|| ConnStatus::Missing) {
                break rx;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(again.recv().unwrap(), ConnStatus::Missing);
    }

    #[test]
    fn the_summary_is_the_worst_library_status() {
        use ConnStatus::*;
        assert_eq!(worst(&[]), None);
        assert_eq!(worst(&[Ok, Ok]), Some(Ok));
        assert_eq!(worst(&[Ok, Secondary]), Some(Secondary));
        assert_eq!(worst(&[Missing, Secondary]), Some(Missing));
        assert_eq!(worst(&[Unreachable, Missing]), Some(Unreachable));
        assert_eq!(worst(&[Auth, Unreachable, Ok]), Some(Auth));
        let payload = StatusPayload::new(vec![
            LibraryStatus {
                id: 1,
                name: String::new(),
                status: Ok,
            },
            LibraryStatus {
                id: 2,
                name: "USB".into(),
                status: Missing,
            },
        ])
        .unwrap();
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["status"], "missing");
        assert_eq!(json["libraries"][1]["name"], "USB");
        assert_eq!(payload.of(2), Some(Missing));
        assert_eq!(payload.of(9), None);
        assert!(StatusPayload::new(Vec::new()).is_none());
    }

    #[test]
    fn libraries_in_use_are_the_set_up_ones_that_are_on() {
        let creds = MemoryStore::default();
        let dir = tempfile::tempdir().unwrap();
        let off = Library {
            enabled: false,
            ..local_library(3, dir.path())
        };
        let blank = Library {
            source: LibrarySource::Local { path: " ".into() },
            ..local_library(4, dir.path())
        };
        let mut settings = Settings {
            primary: ServerConfig {
                username: "music".into(),
                ..ServerConfig::default()
            },
            libraries: vec![local_library(2, dir.path()), off, blank],
            ..Settings::default()
        };
        assert_eq!(Libraries::from_settings(&settings, &creds).ids(), [1, 2]);
        settings.default_library_enabled = false;
        assert_eq!(Libraries::from_settings(&settings, &creds).ids(), [2]);
    }

    #[test]
    fn the_monitor_checks_every_library_and_sees_a_disk_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk");
        let settings = Settings {
            libraries: vec![local_library(2, dir.path()), local_library(3, &disk)],
            ..Settings::default()
        };
        let src = Arc::new(AppSource::default());
        src.set(Some(Libraries::from_settings(
            &settings,
            &MemoryStore::default(),
        )));
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let monitor_src = src.clone();
        let t = std::thread::spawn(move || {
            monitor(monitor_src, wake_rx, |p: &StatusPayload| {
                let _ = seen_tx.send(p.clone());
            })
        });
        let first = seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.status, ConnStatus::Missing);
        assert_eq!(first.of(2), Some(ConnStatus::Ok));
        assert_eq!(first.of(3), Some(ConnStatus::Missing));
        std::fs::create_dir(&disk).unwrap();
        wake_tx.send(()).unwrap();
        let again = seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(again.status, ConnStatus::Ok);
        drop(wake_tx);
        t.join().unwrap();
    }

    /// The first library's servers.
    fn smb_servers(settings: &Settings, creds: &MemoryStore) -> Servers {
        Servers::new(1, &settings.primary, settings.secondary.as_ref(), creds).unwrap()
    }

    /// Settings and a credential store from `.env.test` (primary `SMB_TEST_*`,
    /// secondary `SMB_TEST2_*`, blank values fall back to the primary's).
    fn live_settings() -> (Settings, MemoryStore) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.env.test");
        let text = std::fs::read_to_string(path).expect(".env.test missing");
        let env: std::collections::HashMap<String, String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().into(), v.trim().trim_matches('"').into()))
            .filter(|(_, v): &(String, String)| !v.is_empty())
            .collect();
        let p = |f: &str| {
            env.get(&format!("SMB_TEST_{f}"))
                .cloned()
                .unwrap_or_default()
        };
        let s = |f: &str| {
            env.get(&format!("SMB_TEST2_{f}"))
                .cloned()
                .unwrap_or_else(|| p(f))
        };
        let creds = MemoryStore::default();
        let primary = ServerConfig {
            host: p("HOST"),
            share: p("SHARE"),
            root: p("PATH"),
            username: p("USER"),
            domain: env.get("SMB_TEST_DOMAIN").cloned(),
            enabled: true,
        };
        creds.put(1, &primary.host, &primary.username, &p("PASS"));
        let secondary = env.get("SMB_TEST2_HOST").map(|host| ServerConfig {
            host: host.clone(),
            share: s("SHARE"),
            root: s("PATH"),
            username: s("USER"),
            domain: env.get("SMB_TEST2_DOMAIN").cloned(),
            enabled: true,
        });
        if let Some(sec) = &secondary {
            creds.put(1, &sec.host, &sec.username, &s("PASS"));
        }
        let settings = Settings {
            primary,
            secondary,
            ..Settings::default()
        };
        (settings, creds)
    }

    /// Read-only. Run with `cargo test -p fxmp live_ -- --ignored`.
    #[test]
    #[ignore]
    fn live_probe_reports_ok_secondary_and_auth() {
        let (settings, creds) = live_settings();
        let servers = smb_servers(&settings, &creds);
        // A primary whose disk has to spin up may answer after the backup's grace.
        assert!(servers.probe().reachable());
        assert_eq!(
            servers.probe(),
            ConnStatus::Ok,
            "probe on a live connection"
        );
        assert_eq!(servers.failover.active(), fxmp_core::source::Which::Primary);

        // Primary unreachable (TEST-NET-1, like a VPN that is down): the secondary takes over.
        if let Some(sec) = &settings.secondary {
            let mut down = settings.clone();
            down.primary.host = "192.0.2.1".into();
            let servers = smb_servers(&down, &creds);
            let start = Instant::now();
            assert_eq!(servers.probe(), ConnStatus::Secondary);
            // The dead primary's connect timeout is not waited for.
            assert!(
                start.elapsed() < PRIMARY_GRACE + Duration::from_secs(5),
                "{:?}",
                start.elapsed()
            );
            let pw = server_password(&creds, 1, sec).unwrap();
            let entry = SmbSource::connect(smb_config(sec, pw))
                .unwrap()
                .walk_limited(1, &mut |_| true)
                .unwrap()
                .pop()
                .expect("secondary has files");
            let start = Instant::now();
            servers.failover.open(&entry.rel_path).unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "{:?}",
                start.elapsed()
            );
            assert_eq!(
                servers.failover.active(),
                fxmp_core::source::Which::Secondary
            );

            // The primary switched off: only the backup is read, and that is fine.
            let mut off = settings.clone();
            off.primary.enabled = false;
            let servers = smb_servers(&off, &creds);
            assert_eq!(servers.probe(), ConnStatus::Ok);
            servers.failover.open(&entry.rel_path).unwrap();
            assert_eq!(
                servers.failover.active(),
                fxmp_core::source::Which::Secondary
            );
        }

        let wrong = MemoryStore::default();
        wrong.put(
            1,
            &settings.primary.host,
            &settings.primary.username,
            "definitely-wrong",
        );
        let mut primary_only = settings.clone();
        primary_only.secondary = None;
        let servers = smb_servers(&primary_only, &wrong);
        assert_eq!(servers.probe(), ConnStatus::Auth);
    }
}
