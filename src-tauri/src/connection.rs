//! Connection to the music servers: status, lazily connected SMB sources and the
//! replaceable source the player and scanner read from.
//!
//! Everything here blocks on the network and must run on plain threads, never on an
//! async runtime thread.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use smp_core::source::{
    FailoverSource, FileEntry, MusicSource, ReadSeek, SmbConfig, SmbSource, SourceError,
};

use crate::credentials::CredentialStore;
use crate::settings::{ServerConfig, Settings};

/// How often an unreachable server is retried.
pub const RETRY: Duration = Duration::from_secs(15);

/// Connection state shown in the UI (`connection-status` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnStatus {
    /// Primary server reachable.
    Ok,
    /// Primary down, playing from the secondary.
    Secondary,
    Unreachable,
    /// Wrong username or password.
    Auth,
}

impl ConnStatus {
    pub fn message(self) -> &'static str {
        match self {
            ConnStatus::Ok => "Spojeno",
            ConnStatus::Secondary => "Spojeno na rezervni server",
            ConnStatus::Unreachable => "Disk nije dostupan. Je li uključen WireGuard/VPN?",
            ConnStatus::Auth => "Pogrešno korisničko ime ili lozinka",
        }
    }
}

/// Payload of the `connection-status` event.
#[derive(Debug, Clone, Serialize)]
pub struct StatusPayload {
    pub status: ConnStatus,
    pub message: &'static str,
}

impl From<ConnStatus> for StatusPayload {
    fn from(status: ConnStatus) -> Self {
        Self {
            status,
            message: status.message(),
        }
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

fn source_error(e: smp_core::Error) -> SourceError {
    match e {
        smp_core::Error::Source(e) => e,
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

/// Connects to a server once, as "Testiraj vezu" does.
pub fn test_server(cfg: SmbConfig) -> Result<(), SourceError> {
    smb_connect(cfg).map(drop)
}

/// What "Testiraj vezu" checks for `which` ("primary"/"secondary"): the form's unsaved
/// `server` when given, else the saved one; the form's `password` when non-empty,
/// else the keychain's for that host and user.
pub fn test_target(
    settings: &Settings,
    creds: &dyn CredentialStore,
    which: &str,
    server: Option<ServerConfig>,
    password: Option<String>,
) -> Result<SmbConfig, String> {
    let server = match (which, server) {
        ("primary" | "secondary", Some(s)) => s,
        ("primary", None) => settings.primary.clone(),
        ("secondary", None) => settings
            .secondary
            .clone()
            .ok_or("Rezervni server nije postavljen.")?,
        (other, _) => return Err(format!("Nepoznat server: {other}")),
    };
    let password = password
        .filter(|p| !p.is_empty())
        .or_else(|| creds.get(server.host.trim(), server.username.trim()))
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
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<(), SourceError> {
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

/// The configured servers: primary, optional secondary, and the failover pair over them.
pub struct Servers {
    pub primary: LazySmb,
    pub secondary: Option<LazySmb>,
    pub failover: FailoverSource,
}

impl Servers {
    /// Builds the servers from settings, with passwords from `creds`. Does not connect.
    pub fn from_settings(settings: &Settings, creds: &dyn CredentialStore) -> Self {
        let lazy = |s: &ServerConfig| {
            let password = creds
                .get(s.host.trim(), s.username.trim())
                .unwrap_or_default();
            LazySmb::new(smb_config(s, password))
        };
        let primary = lazy(&settings.primary);
        let secondary = settings.secondary.as_ref().map(lazy);
        let failover = FailoverSource::new(
            Box::new(primary.clone()),
            secondary
                .clone()
                .map(|s| Box::new(s) as Box<dyn MusicSource>),
        );
        Self {
            primary,
            secondary,
            failover,
        }
    }

    /// Checks the primary and, if it fails, the secondary.
    pub fn probe(&self) -> ConnStatus {
        let primary = self.primary.check();
        let secondary = match (&primary, &self.secondary) {
            (Err(_), Some(s)) => Some(s.check()),
            _ => None,
        };
        status_of(primary, secondary)
    }
}

/// The source the player and scanner use; replaced when the settings change.
#[derive(Default)]
pub struct AppSource {
    servers: RwLock<Option<Arc<Servers>>>,
}

impl AppSource {
    pub fn set(&self, servers: Option<Servers>) {
        *self.servers.write().unwrap_or_else(|e| e.into_inner()) = servers.map(Arc::new);
    }

    pub fn servers(&self) -> Option<Arc<Servers>> {
        self.servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn current(&self) -> Result<Arc<Servers>, SourceError> {
        self.servers().ok_or(SourceError::Unreachable)
    }
}

impl MusicSource for AppSource {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<(), SourceError> {
        self.current()?.failover.walk(on_entry)
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        self.current()?.failover.open(rel_path)
    }

    fn describe(&self) -> String {
        self.servers()
            .map(|s| s.failover.describe())
            .unwrap_or_else(|| "(not configured)".into())
    }
}

/// Keeps the connection status current. Probes when woken (settings saved, a track
/// failed, playback moved to the secondary) and, while the primary is down, every
/// [`RETRY`]. A wrong password is not retried until the settings change, so the
/// account is not locked out. Every status from a wake is reported; timed retries
/// report only changes. Returns when all wake senders are gone.
pub fn monitor(source: Arc<AppSource>, wake: Receiver<()>, mut report: impl FnMut(ConnStatus)) {
    let mut last: Option<ConnStatus> = None;
    let mut woken = true;
    loop {
        if let Some(servers) = source.servers() {
            let status = servers.probe();
            if woken || last != Some(status) {
                report(status);
            }
            last = Some(status);
        }
        let retry = matches!(last, Some(ConnStatus::Unreachable | ConnStatus::Secondary));
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
        creds.set("10.111.100.2", "music", "saved-pw").unwrap();
        let settings = Settings {
            primary: ServerConfig {
                username: "music".into(),
                ..ServerConfig::default()
            },
            ..Settings::default()
        };

        // Saved settings, keychain password.
        let t = test_target(&settings, &creds, "primary", None, None).unwrap();
        assert_eq!(
            (t.host.as_str(), t.password.as_str()),
            ("10.111.100.2", "saved-pw")
        );
        let t = test_target(&settings, &creds, "primary", None, Some(String::new())).unwrap();
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
        };
        let t = test_target(
            &settings,
            &creds,
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
        let t = test_target(&settings, &creds, "secondary", Some(form), None).unwrap();
        assert_eq!(t.password, "", "nothing stored for that host/user");

        assert!(test_target(&settings, &creds, "secondary", None, None).is_err());
        assert!(test_target(&settings, &creds, "other", None, None).is_err());
    }

    #[test]
    fn status_payload_carries_croatian_text() {
        let json = serde_json::to_string(&StatusPayload::from(ConnStatus::Unreachable)).unwrap();
        assert_eq!(
            json,
            r#"{"status":"unreachable","message":"Disk nije dostupan. Je li uključen WireGuard/VPN?"}"#
        );
    }

    #[test]
    fn servers_use_keychain_password_per_server() {
        let creds = MemoryStore::default();
        creds.set("10.111.100.2", "music", "pw1").unwrap();
        creds.set("10.111.0.2", "backup", "pw2").unwrap();
        let settings = Settings {
            primary: ServerConfig {
                username: " music ".into(),
                domain: Some("  ".into()),
                ..ServerConfig::default()
            },
            secondary: Some(ServerConfig {
                host: "10.111.0.2".into(),
                share: "music".into(),
                root: "/MP3".into(),
                username: "backup".into(),
                domain: Some("WORKGROUP".into()),
            }),
            ..Settings::default()
        };
        let servers = Servers::from_settings(&settings, &creds);
        let p = servers.primary.config();
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
        };
        creds
            .set(&primary.host, &primary.username, &p("PASS"))
            .unwrap();
        let secondary = env.get("SMB_TEST2_HOST").map(|host| ServerConfig {
            host: host.clone(),
            share: s("SHARE"),
            root: s("PATH"),
            username: s("USER"),
            domain: env.get("SMB_TEST2_DOMAIN").cloned(),
        });
        if let Some(sec) = &secondary {
            creds.set(&sec.host, &sec.username, &s("PASS")).unwrap();
        }
        let settings = Settings {
            primary,
            secondary,
            ..Settings::default()
        };
        (settings, creds)
    }

    /// Read-only. Run with `cargo test -p smb-music-player live_ -- --ignored`.
    #[test]
    #[ignore]
    fn live_probe_reports_ok_secondary_and_auth() {
        let (settings, creds) = live_settings();
        let servers = Servers::from_settings(&settings, &creds);
        assert_eq!(servers.probe(), ConnStatus::Ok);
        assert_eq!(
            servers.probe(),
            ConnStatus::Ok,
            "probe on a live connection"
        );

        // Primary unreachable (TEST-NET-1, like a VPN that is down): the secondary takes over.
        if let Some(sec) = &settings.secondary {
            let mut down = settings.clone();
            down.primary.host = "192.0.2.1".into();
            let servers = Servers::from_settings(&down, &creds);
            assert_eq!(servers.probe(), ConnStatus::Secondary);
            let pw = creds.get(&sec.host, &sec.username).unwrap();
            let entry = SmbSource::connect(smb_config(sec, pw))
                .unwrap()
                .walk_limited(1, &mut |_| true)
                .unwrap()
                .pop()
                .expect("secondary has files");
            servers.failover.open(&entry.rel_path).unwrap();
            assert_eq!(
                servers.failover.active(),
                smp_core::source::Which::Secondary
            );
        }

        let wrong = MemoryStore::default();
        wrong
            .set(
                &settings.primary.host,
                &settings.primary.username,
                "definitely-wrong",
            )
            .unwrap();
        let mut primary_only = settings.clone();
        primary_only.secondary = None;
        let servers = Servers::from_settings(&primary_only, &wrong);
        assert_eq!(servers.probe(), ConnStatus::Auth);
    }
}
