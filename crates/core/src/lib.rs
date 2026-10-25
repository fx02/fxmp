pub mod audio;
pub mod catalog;
pub mod failure;
pub mod genre;
pub mod m3u;
pub mod queue;
pub mod scan;
pub mod shuffle;
pub mod source;
pub mod tags;

/// Identifier of a track in the catalog.
pub type TrackId = i64;

/// Identifier of a library: one music source (an SMB server with its optional
/// backup, or a local folder) whose tracks the catalog keeps apart from the others'.
pub type LibraryId = i64;

/// The first library: the SMB server set up in the first-run settings. Tracks
/// from before libraries existed belong to it.
pub const DEFAULT_LIBRARY: LibraryId = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("source error: {0}")]
    Source(#[from] source::SourceError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Sets the flag when dropped, including while unwinding from a panic.
pub(crate) struct SetOnDrop<'a>(pub(crate) &'a std::sync::atomic::AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
