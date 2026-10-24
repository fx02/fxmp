pub mod shuffle;
pub mod genre;
pub mod catalog;
pub mod source;
pub mod tags;
pub mod scan;

/// Identifier of a track in the catalog.
pub type TrackId = i64;

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
