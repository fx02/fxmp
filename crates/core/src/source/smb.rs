//! Music source on an SMB share, using the pure-Rust `smb` crate (async mode)
//! driven from a private tokio runtime, so the [`MusicSource`] API stays blocking.
//! One authenticated connection is shared by the walk and all open files; a
//! dropped connection is re-established once per operation before the error is
//! returned. Every network operation is capped at [`NET_TIMEOUT`], except
//! folder listings, which get [`LIST_TIMEOUT`].
//!
//! The blocking methods must not be called from inside an async runtime
//! (use `spawn_blocking` there).

use std::fmt;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use smb::{
    Client, ClientConfig, ConnectionConfig, DirAccessMask, Directory, FileAccessMask,
    FileCreateArgs, FileFullDirectoryInformation, GetLen, ReadAt, Resource, Status, UncPath,
};
use tokio::runtime::Runtime;

use super::{is_supported, FileEntry, MusicSource, ReadSeek, SourceError, WalkReport};
use crate::SetOnDrop;

/// Timeout for every network operation (connect, open, read a chunk) except listing.
const NET_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for listing one folder: big folders on a busy server answer slowly.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);
/// Folders listed at once during a walk (on the one shared connection).
const WALK_CONCURRENCY: usize = 8;
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
    block_on_within(NET_TIMEOUT, fut)
}

/// Runs `fut` to completion, failing with a timeout after `limit`.
fn block_on_within<T>(
    limit: Duration,
    fut: impl Future<Output = smb::Result<T>>,
) -> smb::Result<T> {
    runtime().block_on(async {
        tokio::time::timeout(limit, fut)
            .await
            .unwrap_or(Err(smb::Error::OperationTimeout(
                smb::error::TimedOutTask::ReceiveNextMessage,
                limit,
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
            self.walk_dir(&mut Vec::new(), &mut |e| {
                if keep(&e) {
                    out.push(e);
                }
                out.len() < max
            })?;
        }
        Ok(out)
    }

    fn walk_dir(
        &self,
        skipped: &mut Vec<String>,
        on_entry: &mut dyn FnMut(FileEntry) -> bool,
    ) -> Result<bool, SourceError> {
        walk_tree(
            &|rel| self.list_dir(rel),
            WALK_CONCURRENCY,
            skipped,
            on_entry,
        )
    }

    fn list_dir(&self, rel: &str) -> Result<Vec<DirItem>, SourceError> {
        let path = self.shared.unc(rel);
        let access = DirAccessMask::new()
            .with_list_directory(true)
            .with_synchronize(true);
        let args = FileCreateArgs::make_open_existing(access.into());
        let (path, args) = (&path, &args);
        self.shared
            .with_retry(LIST_TIMEOUT, |conn| async move {
                let dir = match conn.client().create_file(path, args).await? {
                    Resource::Directory(dir) => Arc::new(dir),
                    other => {
                        close_resource(other).await;
                        return Err(smb::Error::NotFound(format!("{rel} is not a folder")));
                    }
                };
                let items: smb::Result<Vec<FileFullDirectoryInformation>> =
                    match Directory::query::<FileFullDirectoryInformation>(&dir, "*").await {
                        Ok(stream) => stream.collect::<Vec<_>>().await.into_iter().collect(),
                        Err(e) => Err(e),
                    };
                dir.close().await.ok();
                items
            })
            .map(|(_, items)| items.into_iter().map(DirItem::from).collect())
    }
}

/// One folder entry, as the walk needs it.
#[derive(Debug, Clone)]
struct DirItem {
    name: String,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

impl From<FileFullDirectoryInformation> for DirItem {
    fn from(i: FileFullDirectoryInformation) -> Self {
        Self {
            name: i.file_name.to_string(),
            is_dir: i.file_attributes.directory(),
            size: i.end_of_file,
            mtime: i
                .last_write_time
                .since_epoch()
                .as_secs()
                .saturating_sub(FILETIME_TO_UNIX_SECS) as i64,
        }
    }
}

/// Concurrent walk: root first, then every listed subfolder as it is found.
/// `on_entry` returns `false` to stop the walk; returns `Ok(false)` once stopped.
/// Up to `workers` folders are listed at once, each on its own thread; entries
/// are emitted on the calling thread, in no particular order.
///
/// A subfolder that fails for its own reasons (access denied, vanished, odd I/O)
/// is logged and skipped. A failure on the root, or a lost/timed-out connection
/// anywhere, aborts the walk with `Err` (already-emitted entries must then be
/// discarded): reporting a partial tree as complete would make the scanner
/// delete the missing tracks.
fn walk_tree(
    list: &(dyn Fn(&str) -> Result<Vec<DirItem>, SourceError> + Sync),
    workers: usize,
    skipped: &mut Vec<String>,
    on_entry: &mut dyn FnMut(FileEntry) -> bool,
) -> Result<bool, SourceError> {
    let (job_tx, job_rx) = mpsc::channel::<String>();
    let job_rx = Mutex::new(job_rx);
    let (done_tx, done_rx) = mpsc::channel();
    // Tells workers to drop queued folders once the walk is over.
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..workers.max(1) {
            let (job_rx, done_tx, stop) = (&job_rx, done_tx.clone(), &stop);
            s.spawn(move || loop {
                let rel = match job_rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                    Ok(rel) => rel,
                    Err(_) => break,
                };
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // A panicking lister is re-raised on the calling thread.
                let items = std::panic::catch_unwind(AssertUnwindSafe(|| list(&rel)));
                if done_tx.send((rel, items)).is_err() {
                    break;
                }
            });
        }
        drop(done_tx);
        // Owned here so an unwinding caller disconnects the workers before joining them.
        let job_tx = job_tx;
        // Dropped before `job_tx` (on return or panic): workers finish the
        // listing in hand, skip what is queued and exit.
        let _stop_on_exit = SetOnDrop(&stop);

        (|| {
            job_tx.send(String::new()).expect("workers alive");
            let mut pending = 1usize;
            while pending > 0 {
                let (rel, items) = done_rx.recv().expect("a worker owes a result");
                pending -= 1;
                let items = match items.unwrap_or_else(|p| std::panic::resume_unwind(p)) {
                    Ok(items) => items,
                    Err(e @ (SourceError::Auth | SourceError::NotFound | SourceError::Io(_)))
                        if !rel.is_empty() =>
                    {
                        log::warn!("SMB: skipping unreadable folder '{rel}': {e}");
                        skipped.push(rel);
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                for item in items {
                    if item.name.starts_with('.') {
                        continue;
                    }
                    let rel_path = if rel.is_empty() {
                        item.name
                    } else {
                        format!("{rel}/{}", item.name)
                    };
                    if item.is_dir {
                        job_tx.send(rel_path).expect("workers alive");
                        pending += 1;
                    } else if is_supported(&rel_path) {
                        let entry = FileEntry {
                            rel_path,
                            size: item.size,
                            mtime: item.mtime,
                        };
                        if !on_entry(entry) {
                            return Ok(false);
                        }
                    }
                }
            }
            Ok(true)
        })()
    })
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

    /// Runs `op` on the shared connection; if the connection was lost or timed
    /// out (e.g. half-open after sleep), reconnects once and retries.
    /// Returns the generation of the connection that produced the result.
    /// Each attempt is capped at `limit`.
    fn with_retry<T, F, Fut>(&self, limit: Duration, op: F) -> Result<(u64, T), SourceError>
    where
        F: Fn(Arc<Conn>) -> Fut,
        Fut: Future<Output = smb::Result<T>>,
    {
        let (generation, conn) = self.current();
        match block_on_within(limit, op(conn)).map_err(|e| map_err(&e)) {
            Ok(v) => Ok((generation, v)),
            Err(e) if needs_reconnect(&e) => {
                let (generation, conn) = self.reconnect(generation)?;
                let v = block_on_within(limit, op(conn)).map_err(|e| map_err(&e))?;
                Ok((generation, v))
            }
            Err(e) => Err(e),
        }
    }

    /// Opens `rel` for reading; returns the connection generation, the handle and its size.
    fn open_file(&self, rel: &str) -> Result<(u64, smb::File, u64), SourceError> {
        let path = self.unc(rel);
        let args =
            FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
        let (path, args) = (&path, &args);
        let (generation, (file, size)) = self.with_retry(NET_TIMEOUT, |conn| async move {
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
        Ok((generation, file, size))
    }
}

fn open_conn(cfg: &SmbConfig, share: &UncPath) -> Result<Conn, SourceError> {
    let client = Client::new(ClientConfig {
        connection: ConnectionConfig {
            // The library's own per-response cap; must allow the longest
            // operation. Shorter ones are capped by `block_on_within`.
            timeout: Some(LIST_TIMEOUT),
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

/// Errors after which the connection is suspect and worth one reconnect.
fn needs_reconnect(e: &SourceError) -> bool {
    matches!(e, SourceError::Unreachable | SourceError::Timeout)
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
            Err(e) if needs_reconnect(&e) => self.reopen().and_then(|()| self.read_chunk(pos)),
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
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
        let mut skipped_dirs = Vec::new();
        self.walk_dir(&mut skipped_dirs, &mut |e| {
            on_entry(e);
            true
        })?;
        Ok(WalkReport { skipped_dirs })
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
    use std::sync::atomic::AtomicUsize;

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

    fn item(name: &str, is_dir: bool) -> DirItem {
        DirItem {
            name: name.into(),
            is_dir,
            size: 1,
            mtime: 1,
        }
    }

    /// Lists a fake tree: root has `ok/` (one song), `bad/` (fails with
    /// `bad_err`) and `z.mp3`.
    fn fake_list(
        bad_err: SourceError,
    ) -> impl Fn(&str) -> Result<Vec<DirItem>, SourceError> + Sync {
        move |rel: &str| match rel {
            "" => Ok(vec![
                item("ok", true),
                item("bad", true),
                item("z.mp3", false),
            ]),
            "ok" => Ok(vec![item("a.mp3", false), item(".hidden.mp3", false)]),
            "bad" => Err(bad_err.clone()),
            other => panic!("unexpected folder {other}"),
        }
    }

    /// Walks with `workers` concurrent listings; returns the result and the
    /// emitted paths, sorted (emission order is unspecified).
    fn walk_collect(
        list: &(dyn Fn(&str) -> Result<Vec<DirItem>, SourceError> + Sync),
        workers: usize,
    ) -> (Result<bool, SourceError>, Vec<String>, Vec<String>) {
        let mut seen = Vec::new();
        let mut skipped = Vec::new();
        let res = walk_tree(list, workers, &mut skipped, &mut |e| {
            seen.push(e.rel_path);
            true
        });
        seen.sort();
        (res, seen, skipped)
    }

    #[test]
    fn walk_skips_folder_specific_errors() {
        for workers in [1, WALK_CONCURRENCY] {
            for err in [
                SourceError::Auth,
                SourceError::NotFound,
                SourceError::Io("x".into()),
            ] {
                let (res, seen, skipped) = walk_collect(&fake_list(err.clone()), workers);
                assert_eq!(res, Ok(true), "{err:?}");
                assert_eq!(seen, ["ok/a.mp3", "z.mp3"], "{err:?}");
                assert_eq!(skipped, ["bad"], "reported: {err:?}");
            }
        }
    }

    #[test]
    fn walk_aborts_on_connection_errors() {
        for workers in [1, WALK_CONCURRENCY] {
            for err in [SourceError::Unreachable, SourceError::Timeout] {
                let (res, _, _) = walk_collect(&fake_list(err.clone()), workers);
                assert_eq!(res, Err(err));
            }
        }
    }

    #[test]
    fn walk_fails_when_root_unreadable() {
        let (res, _, _) = walk_collect(&|_: &str| Err(SourceError::NotFound), WALK_CONCURRENCY);
        assert_eq!(res, Err(SourceError::NotFound));
    }

    /// Fake share: root holds `d0`..`d15`, each with two songs and a nested
    /// folder `sub` with one more. Each listing sleeps, and the lister records
    /// the peak number of concurrent listings. `failing` folders fail with `err`.
    struct WideTree {
        active: AtomicUsize,
        peak: AtomicUsize,
        failing: Vec<(&'static str, SourceError)>,
    }

    impl WideTree {
        fn new(failing: Vec<(&'static str, SourceError)>) -> Self {
            Self {
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                failing,
            }
        }

        fn list(&self, rel: &str) -> Result<Vec<DirItem>, SourceError> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            self.active.fetch_sub(1, Ordering::SeqCst);
            if let Some((_, err)) = self.failing.iter().find(|(f, _)| *f == rel) {
                return Err(err.clone());
            }
            Ok(if rel.is_empty() {
                (0..16).map(|i| item(&format!("d{i}"), true)).collect()
            } else if rel.ends_with("/sub") {
                vec![item("c.ogg", false)]
            } else {
                vec![
                    item("a.mp3", false),
                    item("sub", true),
                    item("b.flac", false),
                ]
            })
        }
    }

    #[test]
    fn walk_lists_folders_concurrently() {
        let tree = WideTree::new(vec![]);
        let (res, seen, _) = walk_collect(&|rel: &str| tree.list(rel), WALK_CONCURRENCY);
        assert_eq!(res, Ok(true));
        assert_eq!(seen.len(), 16 * 3);
        assert!(seen.contains(&"d7/sub/c.ogg".to_string()));
        let peak = tree.peak.load(Ordering::SeqCst);
        assert!(peak > 1 && peak <= WALK_CONCURRENCY, "peak {peak}");
    }

    #[test]
    fn single_worker_walk_lists_one_folder_at_a_time() {
        let tree = WideTree::new(vec![]);
        let (res, seen, _) = walk_collect(&|rel: &str| tree.list(rel), 1);
        assert_eq!(res, Ok(true));
        assert_eq!(seen.len(), 16 * 3);
        assert_eq!(tree.peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_walk_keeps_error_semantics() {
        // Folder-specific errors deep in the tree are skipped...
        let tree = WideTree::new(vec![
            ("d3", SourceError::Auth),
            ("d5/sub", SourceError::NotFound),
            ("d9", SourceError::Io("odd".into())),
        ]);
        let (res, seen, _) = walk_collect(&|rel: &str| tree.list(rel), WALK_CONCURRENCY);
        assert_eq!(res, Ok(true));
        assert_eq!(seen.len(), 16 * 3 - 3 - 1 - 3);
        assert!(!seen
            .iter()
            .any(|p| p.starts_with("d3/") || p.starts_with("d9/")));
        assert!(!seen.contains(&"d5/sub/c.ogg".to_string()));
        // ...while a lost or timed-out connection anywhere aborts the walk.
        for err in [SourceError::Unreachable, SourceError::Timeout] {
            let tree = WideTree::new(vec![("d11/sub", err.clone())]);
            let (res, _, _) = walk_collect(&|rel: &str| tree.list(rel), WALK_CONCURRENCY);
            assert_eq!(res, Err(err));
        }
    }

    #[test]
    fn walk_stops_when_callback_says_so() {
        let tree = WideTree::new(vec![]);
        let mut n = 0;
        let res = walk_tree(
            &|rel: &str| tree.list(rel),
            WALK_CONCURRENCY,
            &mut Vec::new(),
            &mut |_| {
                n += 1;
                n < 5
            },
        );
        assert_eq!(res, Ok(false));
        assert_eq!(n, 5);
    }

    #[test]
    fn walk_panic_stops_queued_listings() {
        let listed = AtomicUsize::new(0);
        let list = |rel: &str| -> Result<Vec<DirItem>, SourceError> {
            listed.fetch_add(1, Ordering::SeqCst);
            match rel {
                "" => Ok((0..50).map(|i| item(&format!("d{i}"), true)).collect()),
                "d0" => panic!("simulated lister bug"),
                _ => {
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(vec![])
                }
            }
        };
        let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
            walk_tree(&list, 2, &mut Vec::new(), &mut |_| true)
        }));
        assert!(res.is_err(), "panic must propagate");
        // Root + d0 + at most the folders in hand / just picked up; not all 50.
        let n = listed.load(Ordering::SeqCst);
        assert!(n < 10, "listed {n} folders after the panic");
    }

    #[test]
    fn listing_gets_a_longer_timeout_than_file_ops() {
        assert_eq!(LIST_TIMEOUT, Duration::from_secs(15));
        assert_eq!(NET_TIMEOUT, Duration::from_secs(10));
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        };
        assert!(matches!(
            block_on_within(Duration::from_millis(20), slow()),
            Err(smb::Error::OperationTimeout(..))
        ));
        assert!(block_on_within(Duration::from_millis(500), slow()).is_ok());
    }

    #[test]
    fn retry_policy_covers_timeouts() {
        assert!(needs_reconnect(&SourceError::Unreachable));
        assert!(needs_reconnect(&SourceError::Timeout));
        assert!(!needs_reconnect(&SourceError::NotFound));
        assert!(!needs_reconnect(&SourceError::Auth));
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
