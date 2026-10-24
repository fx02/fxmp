//! Music source on an SMB share, using the pure-Rust `smb` crate (async mode)
//! driven from a private tokio runtime, so the [`MusicSource`] API stays blocking.
//! One authenticated connection is shared by the walk and all open files; a
//! dropped connection is re-established once per operation before the error is
//! returned. Every network operation is capped at [`NET_TIMEOUT`].
//!
//! The blocking methods must not be called from inside an async runtime
//! (use `spawn_blocking` there).

use std::fmt;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use smb::{
    Client, ClientConfig, ConnectionConfig, DirAccessMask, Directory, FileAccessMask,
    FileCreateArgs, FileFullDirectoryInformation, GetLen, ReadAt, Resource, Status, UncPath,
};
use tokio::runtime::Runtime;

use super::{is_supported, FileEntry, MusicSource, ReadSeek, SourceError};

/// Timeout for every network operation (connect, list a folder, read a chunk).
const NET_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes requested per SMB READ and kept in the reader's buffer.
const READ_CHUNK: usize = 256 * 1024;
/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_TO_UNIX_SECS: u64 = 11_644_473_600;

// NTSTATUS codes not named by the `smb` crate.
const STATUS_NO_SUCH_FILE: u32 = 0xC000_000F;
const STATUS_NO_SUCH_USER: u32 = 0xC000_0064;
const STATUS_WRONG_PASSWORD: u32 = 0xC000_006A;
const STATUS_ACCOUNT_RESTRICTION: u32 = 0xC000_006E;
const STATUS_INVALID_LOGON_HOURS: u32 = 0xC000_006F;
const STATUS_PASSWORD_EXPIRED: u32 = 0xC000_0071;
const STATUS_ACCOUNT_DISABLED: u32 = 0xC000_0072;
const STATUS_ACCOUNT_EXPIRED: u32 = 0xC000_0193;
const STATUS_PASSWORD_MUST_CHANGE: u32 = 0xC000_0224;

/// Where and as whom to connect. `root` is the music folder inside the share,
/// `/` or `\` separated.
#[derive(Clone, PartialEq, Eq)]
pub struct SmbConfig {
    pub host: String,
    pub share: String,
    pub root: String,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
}

/// Never prints the password or the username.
impl fmt::Debug for SmbConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmbConfig")
            .field("host", &self.host)
            .field("share", &self.share)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Runtime shared by all SMB sources. Never dropped, so dropping a source is safe anywhere.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("smb-io")
            .enable_all()
            .build()
            .expect("failed to start the SMB runtime")
    })
}

/// Runs `fut` to completion with the [`NET_TIMEOUT`] cap.
fn block_on<T>(fut: impl Future<Output = smb::Result<T>>) -> smb::Result<T> {
    runtime().block_on(async {
        tokio::time::timeout(NET_TIMEOUT, fut)
            .await
            .unwrap_or(Err(smb::Error::OperationTimeout(
                smb::error::TimedOutTask::ReceiveNextMessage,
                NET_TIMEOUT,
            )))
    })
}

/// An authenticated client. The library's drop logic spawns tokio tasks, so
/// the client is always dropped inside the runtime.
struct Conn(Option<Client>);

impl Conn {
    fn client(&self) -> &Client {
        self.0.as_ref().expect("client present until drop")
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _rt = runtime().enter();
        drop(self.0.take());
    }
}

/// The live connection plus a counter that changes on every reconnect, so
/// concurrent failures trigger only one reconnect.
struct Session {
    generation: u64,
    conn: Arc<Conn>,
}

struct Shared {
    cfg: SmbConfig,
    share: UncPath,
    /// `root` with `\` separators and no leading/trailing separator.
    root: String,
    session: Mutex<Session>,
}

pub struct SmbSource {
    shared: Arc<Shared>,
}

impl SmbSource {
    /// Connects, authenticates and checks that the music root is a readable folder.
    pub fn connect(cfg: SmbConfig) -> crate::Result<Self> {
        let share = UncPath::new(&cfg.host)
            .and_then(|u| u.with_share(&cfg.share))
            .map_err(|_| SourceError::Unreachable)?;
        let root = cfg
            .root
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\\");
        let conn = open_conn(&cfg, &share)?;
        let shared = Arc::new(Shared {
            cfg,
            share,
            root,
            session: Mutex::new(Session {
                generation: 0,
                conn: Arc::new(conn),
            }),
        });
        let src = Self { shared };
        src.list_dir("")?;
        Ok(src)
    }

    /// Walks like [`MusicSource::walk`] but stops after `max` entries accepted by
    /// `keep`, returning them. Handy for sampling a large share.
    pub fn walk_limited(
        &self,
        max: usize,
        keep: &mut dyn FnMut(&FileEntry) -> bool,
    ) -> Result<Vec<FileEntry>, SourceError> {
        let mut out = Vec::new();
        if max > 0 {
            self.walk_dir("", &mut |e| {
                if keep(&e) {
                    out.push(e);
                }
                out.len() < max
            })?;
        }
        Ok(out)
    }

    /// Recurses into `rel`; `on_entry` returns `false` to stop the walk.
    /// Returns `Ok(false)` once stopped. Only a failure to list the root is an error.
    fn walk_dir(
        &self,
        rel: &str,
        on_entry: &mut dyn FnMut(FileEntry) -> bool,
    ) -> Result<bool, SourceError> {
        let items = match self.list_dir(rel) {
            Ok(items) => items,
            Err(e) if rel.is_empty() => return Err(e),
            Err(e) => {
                log::warn!("SMB: skipping unreadable folder '{rel}': {e}");
                return Ok(true);
            }
        };
        for item in items {
            let name = item.file_name.to_string();
            if name.starts_with('.') {
                continue;
            }
            let rel_path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if item.file_attributes.directory() {
                if !self.walk_dir(&rel_path, on_entry)? {
                    return Ok(false);
                }
            } else if is_supported(&rel_path) {
                let mtime = item
                    .last_write_time
                    .since_epoch()
                    .as_secs()
                    .saturating_sub(FILETIME_TO_UNIX_SECS) as i64;
                let entry = FileEntry {
                    rel_path,
                    size: item.end_of_file,
                    mtime,
                };
                if !on_entry(entry) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn list_dir(&self, rel: &str) -> Result<Vec<FileFullDirectoryInformation>, SourceError> {
        let path = self.shared.unc(rel);
        let access = DirAccessMask::new()
            .with_list_directory(true)
            .with_synchronize(true);
        let args = FileCreateArgs::make_open_existing(access.into());
        let (path, args) = (&path, &args);
        self.shared.with_retry(|conn| async move {
            let dir = match conn.client().create_file(path, args).await? {
                Resource::Directory(dir) => Arc::new(dir),
                other => {
                    close_resource(other).await;
                    return Err(smb::Error::NotFound(format!("{rel} is not a folder")));
                }
            };
            let items = match Directory::query::<FileFullDirectoryInformation>(&dir, "*").await {
                Ok(stream) => stream.collect::<Vec<_>>().await.into_iter().collect(),
                Err(e) => Err(e),
            };
            dir.close().await.ok();
            items
        })
    }
}

impl Shared {
    fn unc(&self, rel: &str) -> UncPath {
        let rel = rel.replace('/', "\\");
        let path = match (self.root.is_empty(), rel.is_empty()) {
            (true, _) => rel,
            (false, true) => self.root.clone(),
            (false, false) => format!("{}\\{rel}", self.root),
        };
        self.share.clone().with_path(&path)
    }

    fn current(&self) -> (u64, Arc<Conn>) {
        let s = self.session.lock().unwrap_or_else(|e| e.into_inner());
        (s.generation, s.conn.clone())
    }

    /// Replaces the connection unless someone already did since `seen_generation`.
    fn reconnect(&self, seen_generation: u64) -> Result<(u64, Arc<Conn>), SourceError> {
        let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if s.generation == seen_generation {
            log::warn!("SMB: connection to {} lost, reconnecting", self.cfg.host);
            s.conn = Arc::new(open_conn(&self.cfg, &self.share)?);
            s.generation += 1;
        }
        Ok((s.generation, s.conn.clone()))
    }

    /// Runs `op` on the shared connection; if the connection was lost,
    /// reconnects once and retries.
    fn with_retry<T, F, Fut>(&self, op: F) -> Result<T, SourceError>
    where
        F: Fn(Arc<Conn>) -> Fut,
        Fut: Future<Output = smb::Result<T>>,
    {
        let (generation, conn) = self.current();
        match block_on(op(conn)).map_err(|e| map_err(&e)) {
            Err(SourceError::Unreachable) => {
                let (_, conn) = self.reconnect(generation)?;
                block_on(op(conn)).map_err(|e| map_err(&e))
            }
            other => other,
        }
    }

    /// Opens `rel` for reading; returns the connection generation, the handle and its size.
    fn open_file(&self, rel: &str) -> Result<(u64, smb::File, u64), SourceError> {
        let path = self.unc(rel);
        let args =
            FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
        let (path, args) = (&path, &args);
        let (file, size) = self.with_retry(|conn| async move {
            match conn.client().create_file(path, args).await? {
                Resource::File(f) => match f.get_len().await {
                    Ok(size) => Ok((f, size)),
                    Err(e) => {
                        f.close().await.ok();
                        Err(e)
                    }
                },
                other => {
                    close_resource(other).await;
                    Err(smb::Error::NotFound(format!("{rel} is not a file")))
                }
            }
        })?;
        Ok((self.current().0, file, size))
    }
}

fn open_conn(cfg: &SmbConfig, share: &UncPath) -> Result<Conn, SourceError> {
    let client = Client::new(ClientConfig {
        connection: ConnectionConfig {
            timeout: Some(NET_TIMEOUT),
            ..Default::default()
        },
        ..Default::default()
    });
    let user = match cfg.domain.as_deref() {
        Some(d) if !d.is_empty() => format!("{d}\\{}", cfg.username),
        _ => cfg.username.clone(),
    };
    // Wrap first so the client is dropped inside the runtime even on failure.
    let conn = Conn(Some(client));
    block_on(
        conn.client()
            .share_connect(share, &user, cfg.password.clone()),
    )
    .map_err(|e| {
        match map_err(&e) {
            // Refused access to the share itself is a credentials problem.
            SourceError::Io(_) if status_of(&e) == Some(Status::U32_ACCESS_DENIED) => {
                SourceError::Auth
            }
            other => other,
        }
    })?;
    Ok(conn)
}

async fn close_resource(r: Resource) {
    let _ = match r {
        Resource::File(f) => f.close().await,
        Resource::Directory(d) => d.close().await,
        Resource::Pipe(p) => p.close().await,
    };
}

fn status_of(e: &smb::Error) -> Option<u32> {
    match e {
        smb::Error::UnexpectedMessageStatus(s) | smb::Error::ReceivedErrorMessage(s, _) => Some(*s),
        _ => None,
    }
}

/// Maps a library error. `Unreachable` also stands for "connection lost", which
/// callers answer with one reconnect.
fn map_err(e: &smb::Error) -> SourceError {
    use smb::transport::TransportError;
    if let Some(status) = status_of(e) {
        return match status {
            Status::U32_LOGON_FAILURE
            | Status::U32_USER_ACCOUNT_LOCKED_OUT
            | STATUS_NO_SUCH_USER
            | STATUS_WRONG_PASSWORD
            | STATUS_ACCOUNT_RESTRICTION
            | STATUS_INVALID_LOGON_HOURS
            | STATUS_PASSWORD_EXPIRED
            | STATUS_ACCOUNT_DISABLED
            | STATUS_ACCOUNT_EXPIRED
            | STATUS_PASSWORD_MUST_CHANGE => SourceError::Auth,
            Status::U32_OBJECT_NAME_NOT_FOUND
            | Status::U32_OBJECT_PATH_NOT_FOUND
            | Status::U32_BAD_NETWORK_NAME
            | Status::U32_OBJECT_NAME_INVALID
            | STATUS_NO_SUCH_FILE => SourceError::NotFound,
            Status::U32_IO_TIMEOUT => SourceError::Timeout,
            Status::U32_NETWORK_NAME_DELETED
            | Status::U32_USER_SESSION_DELETED
            | Status::U32_NETWORK_SESSION_EXPIRED => SourceError::Unreachable,
            _ => SourceError::Io(e.to_string()),
        };
    }
    match e {
        smb::Error::NotFound(_) => SourceError::NotFound,
        smb::Error::OperationTimeout(..) => SourceError::Timeout,
        smb::Error::ConnectionStopped => SourceError::Unreachable,
        smb::Error::IoError(io) => map_io(io),
        smb::Error::TransportError(t) => match t {
            TransportError::Timeout(_) => SourceError::Timeout,
            TransportError::NotConnected | TransportError::InvalidAddress(_) => {
                SourceError::Unreachable
            }
            TransportError::IoError(io) => map_io(io),
            other => SourceError::Io(other.to_string()),
        },
        other => SourceError::Io(other.to_string()),
    }
}

fn map_io(e: &io::Error) -> SourceError {
    use io::ErrorKind::*;
    match e.kind() {
        TimedOut | WouldBlock => SourceError::Timeout,
        NotFound => SourceError::NotFound,
        ConnectionRefused | ConnectionReset | ConnectionAborted | NotConnected | BrokenPipe
        | UnexpectedEof | HostUnreachable | NetworkUnreachable | NetworkDown | AddrNotAvailable => {
            SourceError::Unreachable
        }
        _ => {
            // The library flattens some errors (e.g. on READ) into a message string.
            let msg = e.to_string();
            if msg.contains("timed out") {
                SourceError::Timeout
            } else if msg.contains("connection is stopped")
                || msg.contains("Not connected")
                || msg.contains("Transport error")
            {
                SourceError::Unreachable
            } else {
                SourceError::Io(msg)
            }
        }
    }
}

/// Blocking, buffered, ranged reader over one remote file.
struct SmbReader {
    shared: Arc<Shared>,
    rel_path: String,
    /// Connection generation the handle was opened on.
    generation: u64,
    /// `None` only while being dropped.
    file: Option<smb::File>,
    size: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
}

impl SmbReader {
    /// Reads up to `READ_CHUNK` bytes at `pos` into the buffer, reopening the
    /// file once on a lost connection.
    fn fill(&mut self, pos: u64) -> io::Result<()> {
        let want = (self.size - pos).min(READ_CHUNK as u64) as usize;
        self.buf.resize(want, 0);
        self.buf_start = pos;
        let result = match self.read_chunk(pos) {
            Err(SourceError::Unreachable) => self.reopen().and_then(|()| self.read_chunk(pos)),
            other => other,
        };
        match result {
            Ok(got) => {
                self.buf.truncate(got);
                Ok(())
            }
            Err(e) => {
                self.buf.clear();
                Err(to_io(e))
            }
        }
    }

    fn read_chunk(&mut self, pos: u64) -> Result<usize, SourceError> {
        let file = self.file.as_ref().expect("file present until drop");
        let buf = &mut self.buf;
        block_on(async {
            let mut got = 0;
            while got < buf.len() {
                let n = file.read_at(&mut buf[got..], pos + got as u64).await?;
                if n == 0 {
                    break;
                }
                got += n;
            }
            Ok(got)
        })
        .map_err(|e| map_err(&e))
    }

    fn reopen(&mut self) -> Result<(), SourceError> {
        self.shared.reconnect(self.generation)?;
        let (generation, file, size) = self.shared.open_file(&self.rel_path)?;
        if let Some(old) = self.file.replace(file) {
            close_in_background(old);
        }
        self.generation = generation;
        self.size = size;
        Ok(())
    }
}

fn to_io(e: SourceError) -> io::Error {
    let kind = match e {
        SourceError::Timeout => io::ErrorKind::TimedOut,
        SourceError::NotFound => io::ErrorKind::NotFound,
        SourceError::Unreachable => io::ErrorKind::NotConnected,
        SourceError::Auth => io::ErrorKind::PermissionDenied,
        SourceError::Io(_) => io::ErrorKind::Other,
    };
    io::Error::new(kind, e)
}

/// Closing needs a network round trip; do not make the caller wait for it.
fn close_in_background(file: smb::File) {
    runtime().spawn(async move {
        file.close().await.ok();
    });
}

impl Read for SmbReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let buf_end = self.buf_start + self.buf.len() as u64;
        if self.pos < self.buf_start || self.pos >= buf_end {
            self.fill(self.pos)?;
            if self.buf.is_empty() {
                return Ok(0);
            }
        }
        let offset = (self.pos - self.buf_start) as usize;
        let n = out.len().min(self.buf.len() - offset);
        out[..n].copy_from_slice(&self.buf[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SmbReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        let target = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of file")
        })?;
        self.pos = target;
        Ok(target)
    }
}

impl Drop for SmbReader {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            close_in_background(file);
        }
    }
}

impl MusicSource for SmbSource {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<(), SourceError> {
        self.walk_dir("", &mut |e| {
            on_entry(e);
            true
        })
        .map(|_| ())
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        let (generation, file, size) = self.shared.open_file(rel_path)?;
        Ok(Box::new(SmbReader {
            shared: self.shared.clone(),
            rel_path: rel_path.to_string(),
            generation,
            file: Some(file),
            size,
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
        }))
    }

    fn describe(&self) -> String {
        let c = &self.shared.cfg;
        format!(
            "smb://{}/{}/{}",
            c.host,
            c.share,
            self.shared.root.replace('\\', "/")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smb::transport::TransportError;

    fn status(s: u32) -> smb::Error {
        smb::Error::UnexpectedMessageStatus(s)
    }

    #[test]
    fn maps_auth_statuses() {
        for s in [
            Status::U32_LOGON_FAILURE,
            STATUS_ACCOUNT_DISABLED,
            STATUS_PASSWORD_EXPIRED,
        ] {
            assert_eq!(map_err(&status(s)), SourceError::Auth);
        }
    }

    #[test]
    fn maps_not_found_statuses() {
        for s in [
            Status::U32_OBJECT_NAME_NOT_FOUND,
            Status::U32_OBJECT_PATH_NOT_FOUND,
            Status::U32_BAD_NETWORK_NAME,
        ] {
            assert_eq!(map_err(&status(s)), SourceError::NotFound);
        }
        assert_eq!(
            map_err(&smb::Error::NotFound("x".into())),
            SourceError::NotFound
        );
    }

    #[test]
    fn maps_timeouts() {
        let secs = Duration::from_secs(10);
        assert_eq!(
            map_err(&smb::Error::TransportError(TransportError::Timeout(secs))),
            SourceError::Timeout
        );
        assert_eq!(
            map_err(&smb::Error::OperationTimeout(
                smb::error::TimedOutTask::ReceiveNextMessage,
                secs
            )),
            SourceError::Timeout
        );
        // READ errors arrive flattened into a string.
        let flat = io::Error::other("Operation timed out: ReceiveNextMessage, took >10s");
        assert_eq!(map_err(&smb::Error::IoError(flat)), SourceError::Timeout);
    }

    #[test]
    fn maps_connection_loss_to_unreachable() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(
            map_err(&smb::Error::TransportError(TransportError::IoError(
                refused
            ))),
            SourceError::Unreachable
        );
        assert_eq!(
            map_err(&smb::Error::ConnectionStopped),
            SourceError::Unreachable
        );
        assert_eq!(
            map_err(&status(Status::U32_NETWORK_SESSION_EXPIRED)),
            SourceError::Unreachable
        );
        let flat = io::Error::other("Client connection is stopped");
        assert_eq!(
            map_err(&smb::Error::IoError(flat)),
            SourceError::Unreachable
        );
    }

    #[test]
    fn other_statuses_are_io() {
        assert!(matches!(
            map_err(&status(Status::U32_SHARING_VIOLATION)),
            SourceError::Io(_)
        ));
    }

    #[test]
    fn unreachable_host_is_unreachable() {
        // Port 1 on loopback refuses immediately.
        let cfg = SmbConfig {
            host: "127.0.0.1:1".into(),
            share: "x".into(),
            root: String::new(),
            username: "u".into(),
            password: "p".into(),
            domain: None,
        };
        match SmbSource::connect(cfg) {
            Err(crate::Error::Source(SourceError::Unreachable)) => {}
            Err(e) => panic!("expected Unreachable, got {e}"),
            Ok(_) => panic!("connected to nothing"),
        }
    }

    #[test]
    fn debug_hides_credentials() {
        let cfg = SmbConfig {
            host: "h".into(),
            share: "s".into(),
            root: "r".into(),
            username: "secret-user".into(),
            password: "secret-pass".into(),
            domain: None,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("secret"), "{s}");
    }
}
