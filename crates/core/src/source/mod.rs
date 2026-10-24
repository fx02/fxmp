//! Abstraction over where music files live (local folder, SMB share).

pub mod failover;
pub mod local;
pub mod smb;

pub use failover::{FailoverSource, Which};
pub use local::LocalSource;
pub use smb::{SmbConfig, SmbSource};

use std::io::{Read, Seek};
use std::path::Path;

const SUPPORTED_EXTS: [&str; 3] = ["mp3", "flac", "ogg"];

/// True for `.mp3`/`.flac`/`.ogg` paths (case-insensitive).
pub(crate) fn is_supported(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SUPPORTED_EXTS.iter().any(|s| e.eq_ignore_ascii_case(s)))
}

/// Errors a music source can report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    #[error("authentication failed")]
    Auth,
    #[error("source unreachable")]
    Unreachable,
    #[error("not found")]
    NotFound,
    #[error("i/o error: {0}")]
    Io(String),
    #[error("timed out")]
    Timeout,
}

/// Anything readable and seekable; SMB streams and local files both qualify.
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// A music file found while walking a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Always `/`-separated and relative to the music root.
    pub rel_path: String,
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: i64,
}

/// A copy of an I/O error (they are not `Clone`), keeping a [`SourceError`] it
/// carries (as `SmbReader` errors do), so the copy is classified like the original.
pub(crate) fn clone_io_error(e: &std::io::Error) -> std::io::Error {
    match e.get_ref().and_then(|i| i.downcast_ref::<SourceError>()) {
        Some(src) => std::io::Error::new(e.kind(), src.clone()),
        None => std::io::Error::new(e.kind(), e.to_string()),
    }
}

/// What a walk could not cover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkReport {
    /// Folders (`/`-separated, relative to the music root) whose listing failed and
    /// that were skipped: their files may still exist, they were just not seen.
    pub skipped_dirs: Vec<String>,
}

pub trait MusicSource: Send + Sync {
    /// Calls `on_entry` for every `.mp3`/`.flac`/`.ogg` file (case-insensitive),
    /// skipping hidden files and directories. A subfolder that cannot be listed is
    /// skipped and reported; failing to list the root is an error.
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError>;
    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError>;
    fn describe(&self) -> String;
}
