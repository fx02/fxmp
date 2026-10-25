//! SQLite catalog of tracks, genres and playback state.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::failure::Failure;
use crate::genre::{
    current_key, genre_key, known_genre, migrate_selection, split_genres, KnownGenre,
    UNKNOWN_DISPLAY, UNKNOWN_KEY,
};
use crate::queue::Entry;
use crate::source::Which;
use crate::{LibraryId, Result, TrackId};

/// Catalog version, kept in `PRAGMA user_version`; see [`MIGRATIONS`].
const CATALOG_VERSION: i64 = 5;
/// Consecutive failures after which a track leaves the shuffle until the next scan.
const MAX_FAILS: i64 = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMeta {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub track_no: Option<u32>,
    pub duration_ms: Option<u64>,
    pub raw_genre: Option<String>,
    pub rg_track_gain_db: Option<f32>,
    pub year: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRow {
    pub id: TrackId,
    pub library_id: LibraryId,
    /// Relative to its library's root.
    pub rel_path: String,
    pub meta: TrackMeta,
    pub measured_lufs: Option<f32>,
}

/// A track that failed to open, tag or decode (see [`Catalog::unreadable`]).
#[derive(Debug, Clone, PartialEq)]
pub struct UnreadableTrack {
    pub id: TrackId,
    pub library_id: LibraryId,
    pub rel_path: String,
    /// [`crate::failure::FailCode`] as text; `None` for failures recorded before
    /// reasons were stored.
    pub code: Option<String>,
    pub message: Option<String>,
    pub fail_count: u32,
    /// Unix seconds of the last failure.
    pub at: Option<i64>,
    /// The server the failure came from, if known.
    pub server: Option<Which>,
    /// Left out of the shuffle (too many failures in a row) until the next scan.
    pub excluded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct YearCount {
    pub year: u16,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlbumInfo {
    pub name: String,
    /// The earliest year of its tracks.
    pub year: Option<u16>,
}

/// Which playable tracks [`Catalog::filtered_tracks`] returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackFilter {
    /// Tracks in any of these genres; empty = all genres.
    pub genre_keys: Vec<String>,
    /// Only tracks whose year is within `from..=to` (tracks without a year drop out
    /// unless `unknown_year`).
    pub years: Option<(u16, u16)>,
    /// Also tracks without a year; without `years`, only those.
    pub unknown_year: bool,
    /// Artist or title contains this (ASCII case ignored); the file name stands in
    /// for a missing title. Blank = no text filter.
    pub text: Option<String>,
}

/// Genres and years of a set of tracks (see [`Catalog::facets`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facets {
    /// By display name, like [`Catalog::genres`]; counts are of the given tracks.
    pub genres: Vec<GenreCount>,
    /// Oldest first.
    pub years: Vec<YearCount>,
    /// Tracks without a year.
    pub unknown_year: usize,
}

/// Where the saved playlist is (see [`crate::queue::QueueState`]); written often.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaylistCursor {
    pub cursor: Option<usize>,
    /// Played tracks, oldest first, the current one included.
    pub history: Vec<TrackId>,
    pub history_pos: Option<usize>,
    /// Where the current track was, in ms.
    pub position_ms: u64,
    /// The current track is to be continued (false once the playlist has ended).
    pub resume: bool,
}

/// The saved playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedPlaylist {
    /// What it was made from, as the app wrote it.
    pub selection: String,
    pub shuffle: bool,
    pub order: Vec<Entry>,
    pub cursor: PlaylistCursor,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenreCount {
    pub key: String,
    pub display: String,
    pub count: usize,
}

/// Stable key for a genre selection: sorted keys joined by `|`, empty → `*`.
pub fn selection_key(genre_keys: &[String]) -> String {
    if genre_keys.is_empty() {
        return "*".to_string();
    }
    let mut keys: Vec<&str> = genre_keys.iter().map(String::as_str).collect();
    keys.sort_unstable();
    keys.join("|")
}

/// The columns of `tracks` as this version creates it: a path is unique within its
/// library (see the unique index made in `add_missing_columns`). Older catalogs get
/// the later columns added ([`ADDED_TRACK_COLUMNS`]) and the table rebuilt
/// (migration 5). `AUTOINCREMENT`: the id of a deleted track is never given to
/// another file (a ban or a playlist entry still holding it would hit that file).
const TRACKS_COLUMNS: &str = "
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rel_path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    artist TEXT, album TEXT, title TEXT,
    track_no INTEGER, duration_ms INTEGER,
    raw_genre TEXT,
    rg_track_gain REAL,
    measured_lufs REAL,
    fail_count INTEGER NOT NULL DEFAULT 0,
    banned INTEGER NOT NULL DEFAULT 0,
    present INTEGER NOT NULL DEFAULT 1,
    year INTEGER,
    needs_retag INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    last_error_msg TEXT,
    last_error_at INTEGER,
    last_error_server TEXT,
    library_id INTEGER NOT NULL DEFAULT 1
";

/// Indexes on `tracks` (also made again after the table is rebuilt).
const TRACKS_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS idx_tracks_artist_album ON tracks(artist, album);
CREATE INDEX IF NOT EXISTS idx_tracks_year ON tracks(year);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tracks_library_path ON tracks(library_id, rel_path);
";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS genres(
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    display TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS track_genres(
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    genre_id INTEGER NOT NULL REFERENCES genres(id) ON DELETE CASCADE,
    PRIMARY KEY(track_id, genre_id)
);
CREATE INDEX IF NOT EXISTS idx_track_genres_genre ON track_genres(genre_id);
CREATE TABLE IF NOT EXISTS genre_rules(
    from_key TEXT PRIMARY KEY,
    to_display TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS selection_state(
    selection_key TEXT PRIMARY KEY,
    last_first INTEGER NOT NULL
);
-- The playlist (one row): its order as a blob of 8-byte little-endian track ids,
-- negated for an entry added by hand. A blob, not a row per entry: All is some
-- 60k entries, and inserting or moving one would renumber them all.
CREATE TABLE IF NOT EXISTS playlist(
    id INTEGER PRIMARY KEY CHECK (id = 1),
    selection TEXT NOT NULL,
    shuffle INTEGER NOT NULL,
    entries BLOB NOT NULL,
    format INTEGER NOT NULL DEFAULT 1
);
-- Where the playlist is (one row, small: written every few seconds while playing).
CREATE TABLE IF NOT EXISTS playlist_cursor(
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cursor INTEGER,
    history BLOB NOT NULL,
    history_pos INTEGER,
    position_ms INTEGER NOT NULL,
    resume INTEGER NOT NULL DEFAULT 1
);
-- Libraries switched off in the settings: their tracks stay but are not shown or
-- played. A library not listed is on.
CREATE TABLE IF NOT EXISTS disabled_libraries(id INTEGER PRIMARY KEY);
";

fn ids_to_blob(ids: impl Iterator<Item = i64>) -> Vec<u8> {
    ids.flat_map(i64::to_le_bytes).collect()
}

fn blob_to_ids(blob: &[u8]) -> Vec<i64> {
    blob.as_chunks::<8>()
        .0
        .iter()
        .map(|b| i64::from_le_bytes(*b))
        .collect()
}

/// `text` as a LIKE pattern matching it anywhere; a backslash escapes `%`, `_` and itself.
fn like_contains(text: &str) -> String {
    let mut p = String::with_capacity(text.len() + 2);
    p.push('%');
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(ch);
    }
    p.push('%');
    p
}

/// Columns added to `tracks` after the first version, with their declarations. They
/// are added (cheaply, `ADD COLUMN`) whenever missing, on every open.
const ADDED_TRACK_COLUMNS: &[(&str, &str)] = &[
    ("year", "INTEGER"),
    // Tags are read again on the next scan, even if the file did not change.
    ("needs_retag", "INTEGER NOT NULL DEFAULT 0"),
    // Last failure: FailCode text, sanitized message, Unix seconds, and
    // 'primary'/'secondary' when known.
    ("last_error", "TEXT"),
    ("last_error_msg", "TEXT"),
    ("last_error_at", "INTEGER"),
    ("last_error_server", "TEXT"),
    // Tracks from before libraries are the first (SMB) library's.
    ("library_id", "INTEGER NOT NULL DEFAULT 1"),
];

/// Columns added to the playlist tables after they first shipped (as for `tracks`).
const ADDED_PLAYLIST_COLUMNS: &[(&str, &str, &str)] = &[
    ("playlist", "format", "INTEGER NOT NULL DEFAULT 1"),
    ("playlist_cursor", "resume", "INTEGER NOT NULL DEFAULT 1"),
];

/// Layout of the saved playlist's blobs; a playlist saved in another one is not read.
const PLAYLIST_FORMAT: i64 = 1;

/// A data migration: the catalog version it brings, what it does, and the step.
type Migration = (i64, &'static str, fn(&Catalog) -> Result<()>);

/// Data migrations, in order: the version each one brings the catalog to, what it
/// does, and the step. Each runs in its own transaction together with the version
/// bump; a failed step is logged and rolled back, and the later ones wait for the
/// next start (the app keeps working on the data as it is).
const MIGRATIONS: &[Migration] = &[
    (3, "genre keys renamed, genres re-derived", |c| {
        c.migrate_genre_keys()?;
        c.rebuild_genres()
    }),
    (4, "tags re-read on the next scan (years)", |c| {
        c.conn.execute("UPDATE tracks SET needs_retag=1", [])?;
        Ok(())
    }),
    (5, "a path is unique within its library", |c| {
        c.rebuild_tracks_table()
    }),
];

/// Catalog files (and the version) whose migration step failed in this run: the
/// other opens of this run do not try it again (migration 5 copies every track);
/// the next start does.
static FAILED_MIGRATIONS: std::sync::Mutex<Vec<(String, i64)>> = std::sync::Mutex::new(Vec::new());

fn failed_migrations() -> std::sync::MutexGuard<'static, Vec<(String, i64)>> {
    FAILED_MIGRATIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// As at a new start: failed migrations are tried again.
#[cfg(test)]
pub(crate) fn forget_failed_migrations() {
    failed_migrations().clear();
}

/// `upsert_track` keeps a stored failure only when the file did not change and the
/// failure was in decoding the audio, which a successful tag read does not disprove.
const KEEP_ERROR: &str = "size=excluded.size AND mtime=excluded.mtime \
                          AND last_error IN ('decode', 'unsupported')";

/// The track's library is switched on (a condition on `tracks`, unaliased).
const ENABLED: &str = "library_id NOT IN (SELECT id FROM disabled_libraries)";
/// [`ENABLED`] for `tracks t`.
const T_ENABLED: &str = "t.library_id NOT IN (SELECT id FROM disabled_libraries)";

/// Tracks the "Unreadable" list shows: not banned, in a library that is on, and
/// failed (network trouble stored by an older build aside).
const UNREADABLE: &str = "banned=0 AND library_id NOT IN (SELECT id FROM disabled_libraries) \
                          AND (fail_count>0 OR (last_error IS NOT NULL \
                          AND last_error NOT IN ('timeout', 'unreachable')))";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn which_str(w: Which) -> &'static str {
    match w {
        Which::Primary => "primary",
        Which::Secondary => "secondary",
    }
}

/// Playable (not banned, not excluded, library on) tracks in `f`'s genres and
/// years, as a condition on `tracks t` and its parameters.
fn filter_sql(f: &TrackFilter) -> (String, Vec<Value>) {
    let mut sql = format!("t.banned=0 AND t.fail_count<{MAX_FAILS} AND {T_ENABLED}");
    let mut args = Vec::new();
    if !f.genre_keys.is_empty() {
        let marks = vec!["?"; f.genre_keys.len()].join(",");
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM track_genres tg JOIN genres g ON g.id = tg.genre_id
                          WHERE tg.track_id = t.id AND g.key IN ({marks}))"
        ));
        args.extend(f.genre_keys.iter().cloned().map(Value::Text));
    }
    match (f.years, f.unknown_year) {
        (Some((from, to)), unknown) => {
            let or_unknown = if unknown { " OR t.year IS NULL" } else { "" };
            sql.push_str(&format!(" AND (t.year BETWEEN ? AND ?{or_unknown})"));
            args.extend([Value::Integer(from.into()), Value::Integer(to.into())]);
        }
        (None, true) => sql.push_str(" AND t.year IS NULL"),
        (None, false) => {}
    }
    if let Some(text) = f.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        sql.push_str(
            " AND (t.artist LIKE ? ESCAPE '\\' OR t.title LIKE ? ESCAPE '\\'
                   OR (COALESCE(t.title, '') = '' AND t.rel_path LIKE ? ESCAPE '\\'))",
        );
        let pattern = like_contains(text);
        args.extend(std::iter::repeat_n(Value::Text(pattern), 3));
    }
    (sql, args)
}

const TRACK_COLS: &str = "id, rel_path, artist, album, title, track_no, duration_ms, \
                          raw_genre, rg_track_gain, measured_lufs, year, library_id";

fn row_to_track(r: &Row) -> rusqlite::Result<TrackRow> {
    Ok(TrackRow {
        id: r.get(0)?,
        library_id: r.get(11)?,
        rel_path: r.get(1)?,
        meta: TrackMeta {
            artist: r.get(2)?,
            album: r.get(3)?,
            title: r.get(4)?,
            track_no: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
            duration_ms: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            raw_genre: r.get(7)?,
            rg_track_gain_db: r.get::<_, Option<f64>>(8)?.map(|v| v as f32),
            year: r.get::<_, Option<i64>>(10)?.map(|v| v as u16),
        },
        measured_lufs: r.get::<_, Option<f64>>(9)?.map(|v| v as f32),
    })
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS tracks({TRACKS_COLUMNS});"
        ))?;
        conn.execute_batch(SCHEMA)?;
        let catalog = Self { conn };
        // The app cannot run without these: a failure here is fatal.
        catalog.add_missing_columns()?;
        catalog.migrate();
        Ok(catalog)
    }

    /// Adds the [`ADDED_TRACK_COLUMNS`] a catalog lacks (and their index).
    fn add_missing_columns(&self) -> Result<()> {
        self.savepoint(|| {
            let have: HashSet<String> = self
                .conn
                .prepare("SELECT name FROM pragma_table_info('tracks')")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for (name, decl) in ADDED_TRACK_COLUMNS {
                if !have.contains(*name) {
                    self.conn
                        .execute_batch(&format!("ALTER TABLE tracks ADD COLUMN {name} {decl}"))?;
                    log::info!("catalog: added column tracks.{name}");
                }
            }
            // Every path was unique before libraries came, so this always works.
            self.conn.execute_batch(TRACKS_INDEXES)?;
            for (table, name, decl) in ADDED_PLAYLIST_COLUMNS {
                let have: bool = self.conn.query_row(
                    &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name=?1"),
                    [name],
                    |r| r.get(0),
                )?;
                if !have {
                    self.conn
                        .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"))?;
                    log::info!("catalog: added column {table}.{name}");
                }
            }
            Ok(())
        })
    }

    /// Runs the [`MIGRATIONS`] this catalog has not had yet (see there).
    fn migrate(&self) {
        let version: i64 = match self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)) {
            Ok(v) => v,
            Err(e) => {
                log::error!("catalog: cannot read its version: {e}");
                return;
            }
        };
        // A step may rebuild a table others refer to: foreign keys are checked by
        // hand at the end of each step instead (the pragma only works outside a
        // transaction, as here), and are on again afterwards whatever happens.
        if let Err(e) = self.conn.execute_batch("PRAGMA foreign_keys=OFF") {
            log::error!("catalog: cannot prepare the migrations: {e}");
            return;
        }
        struct ForeignKeysOn<'a>(&'a Connection);
        impl Drop for ForeignKeysOn<'_> {
            fn drop(&mut self) {
                if let Err(e) = self.0.execute_batch("PRAGMA foreign_keys=ON") {
                    log::error!("catalog: foreign keys not switched on again: {e}");
                }
            }
        }
        let _on = ForeignKeysOn(&self.conn);
        // The file, for remembering a failed step (none for an in-memory catalog).
        let file = self
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        for (to, what, step) in MIGRATIONS.iter().filter(|(to, ..)| *to > version) {
            if let Some(f) = &file {
                if failed_migrations().iter().any(|(p, v)| p == f && v == to) {
                    log::info!(
                        "catalog: migration to v{to} failed earlier in this run; next start"
                    );
                    return;
                }
            }
            let started = std::time::Instant::now();
            let done = self.savepoint(|| {
                self.remove_orphans()?;
                step(self)?;
                self.check_foreign_keys()?;
                self.conn
                    .execute_batch(&format!("PRAGMA user_version = {to}"))?;
                Ok(())
            });
            match done {
                Ok(()) => log::info!(
                    "catalog: migrated to v{to} ({what}) in {:.2} s",
                    started.elapsed().as_secs_f32()
                ),
                Err(e) => {
                    log::error!(
                        "catalog: migration to v{to} ({what}) failed, retried next start: {e}"
                    );
                    if let Some(f) = file {
                        failed_migrations().push((f, *to));
                    }
                    return;
                }
            }
        }
        debug_assert!(MIGRATIONS.last().is_some_and(|m| m.0 == CATALOG_VERSION));
    }

    /// Deletes rows that refer to rows that do not exist (genre links an old
    /// build left behind), so the check after a migration step sees only what
    /// that step broke.
    fn remove_orphans(&self) -> Result<()> {
        let broken: Vec<(String, i64)> = self
            .conn
            .prepare(
                "SELECT \"table\", rowid FROM pragma_foreign_key_check WHERE rowid IS NOT NULL",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (table, rowid) in &broken {
            self.conn.execute(
                &format!(
                    "DELETE FROM \"{}\" WHERE rowid=?1",
                    table.replace('"', "\"\"")
                ),
                [rowid],
            )?;
        }
        if !broken.is_empty() {
            log::warn!(
                "catalog: removed {} rows that referred to missing rows",
                broken.len()
            );
        }
        Ok(())
    }

    /// Fails if a row refers to one that does not exist.
    fn check_foreign_keys(&self) -> Result<()> {
        let broken: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
        if broken > 0 {
            return Err(crate::Error::Other(format!(
                "{broken} rows refer to rows that do not exist"
            )));
        }
        Ok(())
    }

    /// Rebuilds `tracks` without the old `UNIQUE` on `rel_path` alone (a path is
    /// unique within its library, see [`TRACKS_INDEXES`]), keeping every row and
    /// id, so genre links, selections and the saved playlist stay valid. Nothing
    /// to do for a table made by this version. Runs in the migration's
    /// transaction, with foreign keys off (dropping the old table must not delete
    /// the genre links).
    fn rebuild_tracks_table(&self) -> Result<()> {
        let old_unique: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_index_list('tracks') WHERE origin='u'",
            [],
            |r| r.get(0),
        )?;
        if old_unique == 0 {
            return Ok(());
        }
        let columns: Vec<String> = self
            .conn
            .prepare("SELECT name FROM pragma_table_info('tracks')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let columns = columns.join(", ");
        self.conn.execute_batch(&format!(
            "CREATE TABLE tracks_v5({TRACKS_COLUMNS});
             INSERT INTO tracks_v5({columns}) SELECT {columns} FROM tracks;
             DROP TABLE tracks;
             ALTER TABLE tracks_v5 RENAME TO tracks;
             {TRACKS_INDEXES}"
        ))?;
        Ok(())
    }

    /// Runs `f` in a savepoint: a transaction of its own, or nested in the caller's.
    /// Rolled back if `f` (or the release) fails.
    fn savepoint<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("SAVEPOINT catalog_sp")?;
        let result = f().and_then(|v| {
            self.conn.execute_batch("RELEASE catalog_sp")?;
            Ok(v)
        });
        if result.is_err() {
            let _ = self
                .conn
                .execute_batch("ROLLBACK TO catalog_sp; RELEASE catalog_sp");
        }
        result
    }

    /// Renames genre keys that older versions stored, in `selection_state` and
    /// `genre_rules.from_key`. A row whose new key is already taken is dropped (the
    /// existing row wins). Runs in the migration's transaction.
    fn migrate_genre_keys(&self) -> Result<()> {
        let tx = &self.conn;
        let selections: Vec<String> = tx
            .prepare("SELECT selection_key FROM selection_state")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for old in selections {
            let keys: Vec<String> = match old.as_str() {
                "*" => continue,
                s => s.split('|').map(str::to_string).collect(),
            };
            let new = selection_key(&migrate_selection(&keys));
            if new != old {
                tx.execute(
                    "INSERT OR IGNORE INTO selection_state(selection_key, last_first)
                     SELECT ?2, last_first FROM selection_state WHERE selection_key=?1",
                    params![old, new],
                )?;
                tx.execute("DELETE FROM selection_state WHERE selection_key=?1", [&old])?;
            }
        }
        let rules: Vec<String> = tx
            .prepare("SELECT from_key FROM genre_rules")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for old in rules {
            let new = current_key(&old);
            if new != old {
                tx.execute(
                    "INSERT OR IGNORE INTO genre_rules(from_key, to_display)
                     SELECT ?2, to_display FROM genre_rules WHERE from_key=?1",
                    params![old, new],
                )?;
                tx.execute("DELETE FROM genre_rules WHERE from_key=?1", [&old])?;
            }
        }
        Ok(())
    }

    /// Run `f` in a single transaction (bulk upserts); rolled back on error.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        // IMMEDIATE: a transaction that reads first would otherwise fail at its
        // first write (SQLITE_BUSY_SNAPSHOT, no busy wait) when another
        // connection wrote in between; this way it waits its turn up front.
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Rolls back a transaction left open (by a write that panicked), so the
    /// connection can start new ones.
    pub fn roll_back_open_transaction(&self) {
        if !self.conn.is_autocommit() {
            if let Err(e) = self.conn.execute_batch("ROLLBACK") {
                log::warn!("catalog: cannot roll back an open transaction: {e}");
            }
        }
    }

    /// No track of a library that is on.
    pub fn is_empty(&self) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks WHERE {ENABLED}"),
            [],
            |r| r.get(0),
        )?;
        Ok(n == 0)
    }

    /// The file is known in `library` with this size and time, and its tags need
    /// no new read.
    pub fn file_unchanged(
        &self,
        library: LibraryId,
        rel_path: &str,
        size: u64,
        mtime: i64,
    ) -> Result<bool> {
        let hit = self
            .conn
            .prepare_cached(
                "SELECT 1 FROM tracks
                 WHERE library_id=?4 AND rel_path=?1 AND size=?2 AND mtime=?3
                   AND needs_retag=0",
            )?
            .exists(params![rel_path, size as i64, mtime, library])?;
        Ok(hit)
    }

    /// A known track with this size that needs no new tag read, whatever its
    /// mtime: how a backup server's copy is judged (copies often lose the time).
    pub fn file_same_size(&self, library: LibraryId, rel_path: &str, size: u64) -> Result<bool> {
        let hit = self
            .conn
            .prepare_cached(
                "SELECT 1 FROM tracks
                 WHERE library_id=?3 AND rel_path=?1 AND size=?2 AND needs_retag=0",
            )?
            .exists(params![rel_path, size as i64, library])?;
        Ok(hit)
    }

    /// Insert or update a track by library and path after reading its tags; marks
    /// it present. A changed file loses its measured loudness and stored failure, a
    /// tag failure is cleared; the failure count is left to
    /// `begin_scan`/`record_success`.
    pub fn upsert_track(
        &self,
        library: LibraryId,
        rel_path: &str,
        size: u64,
        mtime: i64,
        meta: &TrackMeta,
    ) -> Result<TrackId> {
        let sql = format!(
            "INSERT INTO tracks(rel_path, size, mtime, artist, album, title, track_no,
                                duration_ms, raw_genre, rg_track_gain, year, library_id,
                                present)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1)
             ON CONFLICT(library_id, rel_path) DO UPDATE SET
                measured_lufs = CASE WHEN size=excluded.size AND mtime=excluded.mtime
                                     THEN measured_lufs ELSE NULL END,
                last_error = CASE WHEN {KEEP_ERROR} THEN last_error END,
                last_error_msg = CASE WHEN {KEEP_ERROR} THEN last_error_msg END,
                last_error_at = CASE WHEN {KEEP_ERROR} THEN last_error_at END,
                last_error_server = CASE WHEN {KEEP_ERROR} THEN last_error_server END,
                size=excluded.size, mtime=excluded.mtime,
                artist=excluded.artist, album=excluded.album, title=excluded.title,
                track_no=excluded.track_no, duration_ms=excluded.duration_ms,
                raw_genre=excluded.raw_genre, rg_track_gain=excluded.rg_track_gain,
                year=excluded.year, needs_retag=0, present=1
             RETURNING id"
        );
        let id = self.conn.prepare_cached(&sql)?.query_row(
            params![
                rel_path,
                size as i64,
                mtime,
                meta.artist,
                meta.album,
                meta.title,
                meta.track_no.map(i64::from),
                meta.duration_ms.map(|v| v as i64),
                meta.raw_genre,
                meta.rg_track_gain_db.map(f64::from),
                meta.year.map(i64::from),
                library,
            ],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// A file whose tags could not be read during a scan. A known track keeps its
    /// tags; a new file is stored with empty tags. After a failure worth retrying
    /// ([`crate::failure::FailCode::is_retried`]) a known track keeps everything
    /// (size, time, loudness) and the next scan reads it again; after a tag, decode
    /// or format failure the new size and time are stored (a changed file loses its
    /// loudness) and it waits until the file changes. The failure is stored unless
    /// it was network trouble ([`crate::failure::FailCode::is_transient`]).
    pub fn record_scan_failure(
        &self,
        library: LibraryId,
        rel_path: &str,
        size: u64,
        mtime: i64,
        failure: &Failure,
    ) -> Result<TrackId> {
        let retry = failure.code.is_retried();
        let known: Option<TrackId> = if retry {
            self.conn
                .prepare_cached(
                    "UPDATE tracks SET present=1, needs_retag=1
                     WHERE library_id=?2 AND rel_path=?1 RETURNING id",
                )?
                .query_row(params![rel_path, library], |r| r.get(0))
                .optional()?
        } else {
            self.conn
                .prepare_cached(
                    "UPDATE tracks SET
                        measured_lufs = CASE WHEN size=?2 AND mtime=?3 THEN measured_lufs END,
                        size=?2, mtime=?3, present=1, needs_retag=0
                     WHERE library_id=?4 AND rel_path=?1 RETURNING id",
                )?
                .query_row(params![rel_path, size as i64, mtime, library], |r| r.get(0))
                .optional()?
        };
        let id = match known {
            Some(id) => id,
            None => {
                let id =
                    self.upsert_track(library, rel_path, size, mtime, &TrackMeta::default())?;
                self.conn
                    .prepare_cached("UPDATE tracks SET needs_retag=?2 WHERE id=?1")?
                    .execute(params![id, retry])?;
                id
            }
        };
        if !failure.code.is_transient() {
            self.set_error(id, failure, None)?;
        }
        Ok(id)
    }

    fn set_error(&self, id: TrackId, failure: &Failure, server: Option<Which>) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE tracks SET last_error=?2, last_error_msg=?3, last_error_at=?4,
                                   last_error_server=?5
                 WHERE id=?1",
            )?
            .execute(params![
                id,
                failure.code.as_str(),
                failure.message,
                now_secs(),
                server.map(which_str),
            ])?;
        Ok(())
    }

    /// A scan of `library` starts: lets its tracks play again; stored failures
    /// stay listed (see [`Self::unreadable`]). Other libraries are not touched.
    pub fn begin_scan(&self, library: LibraryId) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET present=0, fail_count=0 WHERE library_id=?1",
            [library],
        )?;
        Ok(())
    }

    pub fn mark_seen(&self, library: LibraryId, rel_path: &str) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE tracks SET present=1 WHERE library_id=?2 AND rel_path=?1")?
            .execute(params![rel_path, library])?;
        Ok(())
    }

    /// Delete `library`'s tracks not seen since `begin_scan`, except those under
    /// `keep_under` (folders the walk could not list: their files were not seen,
    /// not deleted; `""` is the whole library), which count as present again.
    /// Returns how many were removed.
    pub fn finish_scan(&self, library: LibraryId, keep_under: &[String]) -> Result<usize> {
        self.savepoint(|| {
            let mut keep = self.conn.prepare_cached(
                "UPDATE tracks SET present=1
                 WHERE present=0 AND library_id=?2
                   AND substr(rel_path, 1, length(?1) + 1) = ?1 || '/'",
            )?;
            for dir in keep_under.iter().map(|d| d.trim_matches('/')) {
                if dir.is_empty() {
                    // The whole library unseen: sweep nothing.
                    self.abort_scan(library)?;
                } else {
                    keep.execute(params![dir, library])?;
                }
            }
            const GONE: &str = "SELECT id FROM tracks WHERE present=0 AND library_id=?1";
            self.conn.execute(
                &format!("DELETE FROM track_genres WHERE track_id IN ({GONE})"),
                [library],
            )?;
            self.conn.execute(
                &format!("DELETE FROM selection_state WHERE last_first IN ({GONE})"),
                [library],
            )?;
            Ok(self.conn.execute(
                "DELETE FROM tracks WHERE present=0 AND library_id=?1",
                [library],
            )?)
        })
    }

    /// A scan of `library` that stopped early (cancelled or failed): its tracks
    /// count as present again, as before `begin_scan`.
    pub fn abort_scan(&self, library: LibraryId) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET present=1 WHERE present=0 AND library_id=?1",
            [library],
        )?;
        Ok(())
    }

    /// Whether tracks of any library can be stored: false while migration 5 is
    /// still to be done (a path is then unique across all libraries, so only the
    /// first library may be scanned).
    pub fn libraries_ready(&self) -> bool {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_index_list('tracks') WHERE origin='u'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .is_ok_and(|n| n == 0)
    }

    /// Switches libraries on and off: exactly `ids` are off (their tracks are kept
    /// but neither listed nor played).
    pub fn set_disabled_libraries(&self, ids: &[LibraryId]) -> Result<()> {
        self.savepoint(|| {
            self.conn.execute("DELETE FROM disabled_libraries", [])?;
            let mut ins = self
                .conn
                .prepare_cached("INSERT OR IGNORE INTO disabled_libraries(id) VALUES(?1)")?;
            for id in ids {
                ins.execute([id])?;
            }
            Ok(())
        })
    }

    pub fn disabled_libraries(&self) -> Result<HashSet<LibraryId>> {
        let v = self
            .conn
            .prepare_cached("SELECT id FROM disabled_libraries")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// How many tracks each library has (whether on or off).
    pub fn library_track_counts(&self) -> Result<HashMap<LibraryId, usize>> {
        let v = self
            .conn
            .prepare_cached("SELECT library_id, COUNT(*) FROM tracks GROUP BY library_id")?
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// A library was removed: deletes its tracks (with their genre links and the
    /// selections that started with one), in one transaction, and rebuilds the
    /// genres. Returns how many tracks went.
    pub fn remove_library(&self, library: LibraryId) -> Result<usize> {
        let removed = self.savepoint(|| {
            const ITS: &str = "SELECT id FROM tracks WHERE library_id=?1";
            self.conn.execute(
                &format!("DELETE FROM track_genres WHERE track_id IN ({ITS})"),
                [library],
            )?;
            self.conn.execute(
                &format!("DELETE FROM selection_state WHERE last_first IN ({ITS})"),
                [library],
            )?;
            self.conn
                .execute("DELETE FROM disabled_libraries WHERE id=?1", [library])?;
            Ok(self
                .conn
                .execute("DELETE FROM tracks WHERE library_id=?1", [library])?)
        })?;
        self.rebuild_genres()?;
        Ok(removed)
    }

    /// Tracks marked not present (between `begin_scan` and `finish_scan`).
    #[cfg(test)]
    pub(crate) fn absent_count(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM tracks WHERE present=0", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap() as usize
    }

    /// Recompute `genres` and `track_genres` from `raw_genre`, folder names and rules.
    ///
    /// Only known genres (see [`known_genre`]) are kept. A track with no known genre
    /// in its tag takes the genre its top folder is named after, if that is a known
    /// genre, and otherwise goes to [`UNKNOWN_KEY`]. A rule whose target is not a
    /// known genre is ignored.
    pub fn rebuild_genres(&self) -> Result<()> {
        let rules: HashMap<String, String> = self
            .conn
            .prepare("SELECT from_key, to_display FROM genre_rules")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // A raw name -> the known genre it ends up in. Rules apply to the known genre
        // (after aliases and parents, so merging House also moves "Deep House"),
        // or else to the raw name's key.
        let resolve = |name: &str| -> Option<KnownGenre> {
            let base = known_genre(name);
            base.as_ref()
                .and_then(|g| rules.get(&g.key))
                .or_else(|| rules.get(&genre_key(name)))
                .and_then(|to| known_genre(to))
                .or(base)
        };

        let tracks: Vec<(TrackId, String, Option<String>)> = self
            .conn
            .prepare("SELECT id, rel_path, raw_genre FROM tracks")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut displays: HashMap<String, String> = HashMap::new();
        let mut assigned: Vec<(TrackId, Vec<String>)> = Vec::with_capacity(tracks.len());
        for (id, rel_path, raw) in &tracks {
            let mut found: Vec<KnownGenre> = raw
                .as_deref()
                .map(split_genres)
                .unwrap_or_default()
                .iter()
                .filter_map(|name| resolve(name))
                .collect();
            if found.is_empty() {
                // Folder fallback: the first path component, if it names a known genre.
                found.extend(
                    rel_path
                        .split_once('/')
                        .and_then(|(folder, _)| resolve(folder)),
                );
            }
            let mut keys: Vec<String> = Vec::with_capacity(found.len().max(1));
            for g in found {
                if !keys.contains(&g.key) {
                    keys.push(g.key.clone());
                    displays.entry(g.key).or_insert(g.display);
                }
            }
            if keys.is_empty() {
                keys.push(UNKNOWN_KEY.to_string());
                displays
                    .entry(UNKNOWN_KEY.to_string())
                    .or_insert_with(|| UNKNOWN_DISPLAY.to_string());
            }
            assigned.push((*id, keys));
        }

        self.savepoint(|| {
            let tx = &self.conn;
            tx.execute("DELETE FROM track_genres", [])?;
            tx.execute("DELETE FROM genres", [])?;
            let mut genre_ids: HashMap<&str, i64> = HashMap::new();
            let mut ins_genre = tx.prepare("INSERT INTO genres(key, display) VALUES(?1, ?2)")?;
            for (key, display) in &displays {
                ins_genre.execute(params![key, display])?;
                genre_ids.insert(key, tx.last_insert_rowid());
            }
            let mut ins_link = tx
                .prepare("INSERT OR IGNORE INTO track_genres(track_id, genre_id) VALUES(?1, ?2)")?;
            for (id, keys) in &assigned {
                for key in keys {
                    ins_link.execute(params![id, genre_ids[key.as_str()]])?;
                }
            }
            Ok(())
        })
    }

    /// Genres with the number of playable (not banned, not failed, library on)
    /// tracks in each.
    pub fn genres(&self) -> Result<Vec<GenreCount>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT g.key, g.display, COUNT(t.id)
             FROM genres g
             JOIN track_genres tg ON tg.genre_id = g.id
             JOIN tracks t ON t.id = tg.track_id AND t.banned = 0 AND t.fail_count < ?1
                           AND {T_ENABLED}
             GROUP BY g.id
             ORDER BY g.display COLLATE NOCASE, g.key"
        ))?;
        let rows = st
            .query_map([MAX_FAILS], |r| {
                Ok(GenreCount {
                    key: r.get(0)?,
                    display: r.get(1)?,
                    count: r.get::<_, i64>(2)? as usize,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Playable tracks in any of the genres; empty selection means everything.
    pub fn playable_tracks(&self, genre_keys: &[String]) -> Result<Vec<TrackId>> {
        self.filtered_tracks(&TrackFilter {
            genre_keys: genre_keys.to_vec(),
            ..Default::default()
        })
    }

    /// Playable tracks matching `filter`.
    pub fn filtered_tracks(&self, filter: &TrackFilter) -> Result<Vec<TrackId>> {
        let (cond, args) = filter_sql(filter);
        let ids = self
            .conn
            .prepare_cached(&format!("SELECT t.id FROM tracks t WHERE {cond}"))?
            .query_map(params_from_iter(args), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// The years of the playable tracks in any of the genres (empty = all), with
    /// their track counts, oldest first. Tracks without a year are not counted.
    pub fn years(&self, genre_keys: &[String]) -> Result<Vec<YearCount>> {
        let (cond, args) = filter_sql(&TrackFilter {
            genre_keys: genre_keys.to_vec(),
            ..Default::default()
        });
        let v = self
            .conn
            .prepare_cached(&format!(
                "SELECT t.year, COUNT(*) FROM tracks t
                 WHERE {cond} AND t.year IS NOT NULL GROUP BY t.year ORDER BY t.year"
            ))?
            .query_map(params_from_iter(args), |r| {
                Ok(YearCount {
                    year: r.get::<_, i64>(0)? as u16,
                    count: r.get::<_, i64>(1)? as usize,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// The genres and years of `ids` (each track counted once; tracks of a library
    /// that is off are not counted).
    pub fn facets(&self, ids: &[TrackId]) -> Result<Facets> {
        let wanted: HashSet<TrackId> = ids.iter().copied().collect();
        let mut genres: Vec<GenreCount> = Vec::new();
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT g.key, g.display, tg.track_id FROM genres g
                 JOIN track_genres tg ON tg.genre_id = g.id
                 JOIN tracks t ON t.id = tg.track_id AND {T_ENABLED}
                 ORDER BY g.display COLLATE NOCASE, g.key"
        ))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            if !wanted.contains(&r.get::<_, TrackId>(2)?) {
                continue;
            }
            let key: String = r.get(0)?;
            match genres.last_mut() {
                Some(g) if g.key == key => g.count += 1,
                _ => genres.push(GenreCount {
                    key,
                    display: r.get(1)?,
                    count: 1,
                }),
            }
        }
        let mut years: std::collections::BTreeMap<u16, usize> = Default::default();
        let mut unknown_year = 0;
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT id, year FROM tracks WHERE {ENABLED}"))?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            if !wanted.contains(&r.get::<_, TrackId>(0)?) {
                continue;
            }
            match r.get::<_, Option<i64>>(1)? {
                Some(y) => *years.entry(y as u16).or_default() += 1,
                None => unknown_year += 1,
            }
        }
        Ok(Facets {
            genres,
            years: years
                .into_iter()
                .map(|(year, count)| YearCount { year, count })
                .collect(),
            unknown_year,
        })
    }

    /// The tracks `ids` in that order (repeats included); ids not in the catalog,
    /// or of a library that is off, are left out.
    pub fn tracks_by_ids(&self, ids: &[TrackId]) -> Result<Vec<TrackRow>> {
        let mut found: HashMap<TrackId, TrackRow> = HashMap::with_capacity(ids.len());
        // In batches, within SQLite's parameter limit.
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql =
                format!("SELECT {TRACK_COLS} FROM tracks WHERE id IN ({marks}) AND {ENABLED}");
            let mut st = self.conn.prepare_cached(&sql)?;
            for row in st.query_map(params_from_iter(chunk), row_to_track)? {
                let row = row?;
                found.insert(row.id, row);
            }
        }
        Ok(ids.iter().filter_map(|id| found.get(id).cloned()).collect())
    }

    /// Saves the playlist's order and what it was made from (the cursor is saved
    /// separately, see [`Self::save_playlist_cursor`]).
    pub fn save_playlist_order(
        &self,
        selection: &str,
        shuffle: bool,
        order: &[Entry],
    ) -> Result<()> {
        let blob = ids_to_blob(order.iter().map(|e| if e.manual { -e.id } else { e.id }));
        self.conn.execute(
            "INSERT OR REPLACE INTO playlist(id, selection, shuffle, entries, format)
             VALUES(1, ?1, ?2, ?3, ?4)",
            params![selection, shuffle, blob, PLAYLIST_FORMAT],
        )?;
        Ok(())
    }

    /// Saves the playlist's order (when given: selection, shuffle, entries) and
    /// its cursor together: a crash or error in between never leaves a new
    /// order with the old cursor.
    pub fn save_playlist(
        &self,
        order: Option<(&str, bool, &[Entry])>,
        cursor: &PlaylistCursor,
    ) -> Result<()> {
        self.savepoint(|| {
            if let Some((selection, shuffle, entries)) = order {
                self.save_playlist_order(selection, shuffle, entries)?;
            }
            self.save_playlist_cursor(cursor)
        })
    }

    pub fn save_playlist_cursor(&self, c: &PlaylistCursor) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO playlist_cursor(id, cursor, history, history_pos,
                                                    position_ms, resume)
             VALUES(1, ?1, ?2, ?3, ?4, ?5)",
            params![
                c.cursor.map(|v| v as i64),
                ids_to_blob(c.history.iter().copied()),
                c.history_pos.map(|v| v as i64),
                c.position_ms as i64,
                c.resume
            ],
        )?;
        Ok(())
    }

    /// The saved playlist, if there is one.
    pub fn load_playlist(&self) -> Result<Option<SavedPlaylist>> {
        let list = self
            .conn
            .query_row(
                "SELECT selection, shuffle, entries FROM playlist WHERE id=1 AND format=?1",
                [PLAYLIST_FORMAT],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((selection, shuffle, entries)) = list else {
            return Ok(None);
        };
        let cursor = self
            .conn
            .query_row(
                "SELECT cursor, history, history_pos, position_ms, resume
                 FROM playlist_cursor WHERE id=1",
                [],
                |r| {
                    Ok(PlaylistCursor {
                        cursor: r.get::<_, Option<i64>>(0)?.map(|v| v as usize),
                        history: blob_to_ids(&r.get::<_, Vec<u8>>(1)?),
                        history_pos: r.get::<_, Option<i64>>(2)?.map(|v| v as usize),
                        position_ms: r.get::<_, i64>(3)?.max(0) as u64,
                        resume: r.get(4)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        // Track ids are positive; anything else is damage and left out.
        let order = blob_to_ids(&entries)
            .into_iter()
            .filter_map(|id| match id {
                1.. => Some(Entry::new(id)),
                0 => None,
                _ => id.checked_neg().map(Entry::manual),
            })
            .collect();
        Ok(Some(SavedPlaylist {
            selection,
            shuffle,
            order,
            cursor,
        }))
    }

    pub fn track(&self, id: TrackId) -> Result<TrackRow> {
        let sql = format!("SELECT {TRACK_COLS} FROM tracks WHERE id=?1");
        Ok(self.conn.query_row(&sql, [id], row_to_track)?)
    }

    /// Display names of the genres a track belongs to, sorted.
    pub fn track_genres(&self, id: TrackId) -> Result<Vec<String>> {
        let v = self
            .conn
            .prepare_cached(
                "SELECT g.display FROM track_genres tg JOIN genres g ON g.id = tg.genre_id
                 WHERE tg.track_id=?1 ORDER BY g.display COLLATE NOCASE, g.display",
            )?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// A playback failure: counts it and stores why (and from which server, if
    /// known). Network trouble ([`crate::failure::FailCode::is_transient`]) is not
    /// the file's fault: neither counted nor stored. Returns the failures in a row
    /// (0 for a track no longer in the catalog).
    pub fn record_failure(
        &self,
        id: TrackId,
        failure: &Failure,
        server: Option<Which>,
    ) -> Result<u32> {
        if failure.code.is_transient() {
            let n: Option<i64> = self
                .conn
                .query_row("SELECT fail_count FROM tracks WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()?;
            return Ok(n.unwrap_or(0) as u32);
        }
        self.savepoint(|| {
            let n: Option<i64> = self
                .conn
                .query_row(
                    "UPDATE tracks SET fail_count = fail_count + 1 WHERE id=?1 RETURNING fail_count",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if n.is_some() {
                self.set_error(id, failure, server)?;
            }
            Ok(n.unwrap_or(0) as u32)
        })
    }

    /// The track played: resets its failures and forgets the last one.
    pub fn record_success(&self, id: TrackId) -> Result<()> {
        self.retry_failed(id)
    }

    /// Tracks (not banned) that failed at least once since their last success or
    /// retry, or are left out after too many failures; newest failure first.
    pub fn unreadable(&self) -> Result<Vec<UnreadableTrack>> {
        let v = self
            .conn
            .prepare_cached(&format!(
                "SELECT id, rel_path, last_error, last_error_msg, fail_count, last_error_at,
                        last_error_server, library_id
                 FROM tracks
                 WHERE {UNREADABLE}
                 ORDER BY last_error_at IS NULL, last_error_at DESC, rel_path"
            ))?
            .query_map([], |r| {
                let fail_count: i64 = r.get(4)?;
                Ok(UnreadableTrack {
                    id: r.get(0)?,
                    library_id: r.get(7)?,
                    rel_path: r.get(1)?,
                    code: r.get(2)?,
                    message: r.get(3)?,
                    fail_count: fail_count as u32,
                    at: r.get(5)?,
                    server: match r.get::<_, Option<String>>(6)?.as_deref() {
                        Some("primary") => Some(Which::Primary),
                        Some("secondary") => Some(Which::Secondary),
                        _ => None,
                    },
                    excluded: fail_count >= MAX_FAILS,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// How many tracks [`Self::unreadable`] lists.
    pub fn unreadable_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM tracks WHERE {UNREADABLE}"),
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Resets a track's failures and forgets the last one, so it plays again.
    pub fn retry_failed(&self, id: TrackId) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET fail_count=0, last_error=NULL, last_error_msg=NULL,
                               last_error_at=NULL, last_error_server=NULL
             WHERE id=?1",
            [id],
        )?;
        Ok(())
    }

    /// [`Self::retry_failed`] for every listed track; returns how many.
    pub fn retry_all_failed(&self) -> Result<usize> {
        Ok(self.conn.execute(
            &format!(
                "UPDATE tracks SET fail_count=0, last_error=NULL, last_error_msg=NULL,
                                   last_error_at=NULL, last_error_server=NULL
                 WHERE {UNREADABLE}"
            ),
            [],
        )?)
    }

    /// Empties the catalog: tracks, genres, rules, selections and bans, in one
    /// transaction (other connections see it all at once), then compacts the file.
    pub fn clear(&self) -> Result<()> {
        self.savepoint(|| {
            self.conn.execute_batch(
                "DELETE FROM track_genres; DELETE FROM genres; DELETE FROM genre_rules;
                 DELETE FROM selection_state; DELETE FROM playlist; DELETE FROM playlist_cursor;
                 DELETE FROM tracks;",
            )?;
            Ok(())
        })?;
        // Gives the space back; not needed for correctness (another connection
        // reading at this moment makes it fail).
        if let Err(e) = self.conn.execute_batch("VACUUM") {
            log::info!("catalog: not compacted after clearing: {e}");
        }
        Ok(())
    }

    pub fn set_banned(&self, id: TrackId, banned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET banned=?2 WHERE id=?1",
            params![id, banned],
        )?;
        Ok(())
    }

    /// Banned tracks of the libraries that are on.
    pub fn banned(&self) -> Result<Vec<TrackRow>> {
        let sql = format!(
            "SELECT {TRACK_COLS} FROM tracks WHERE banned=1 AND {ENABLED}
             ORDER BY rel_path, library_id"
        );
        let rows = self
            .conn
            .prepare(&sql)?
            .query_map([], row_to_track)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn set_measured_lufs(&self, id: TrackId, lufs: f32) -> Result<()> {
        self.conn.execute(
            "UPDATE tracks SET measured_lufs=?2 WHERE id=?1",
            params![id, f64::from(lufs)],
        )?;
        Ok(())
    }

    /// Merge/rename: tracks of genre `from_key` move to `to_display` (key derived from it).
    pub fn add_genre_rule(&self, from_key: &str, to_display: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO genre_rules(from_key, to_display) VALUES(?1, ?2)",
            params![from_key, to_display],
        )?;
        self.rebuild_genres()
    }

    pub fn last_first(&self, selection_key: &str) -> Result<Option<TrackId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_first FROM selection_state WHERE selection_key=?1",
                [selection_key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_last_first(&self, selection_key: &str, id: TrackId) -> Result<()> {
        // Not for a track removed meanwhile (the library was cleared, say).
        self.conn.execute(
            "INSERT OR REPLACE INTO selection_state(selection_key, last_first)
             SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM tracks WHERE id=?2)",
            params![selection_key, id],
        )?;
        Ok(())
    }

    pub fn artists(&self) -> Result<Vec<String>> {
        let v = self
            .conn
            .prepare(&format!(
                "SELECT DISTINCT artist FROM tracks
                 WHERE artist IS NOT NULL AND artist<>'' AND {ENABLED}
                 ORDER BY artist COLLATE NOCASE, artist"
            ))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// The artist's albums by name, each with its earliest year.
    pub fn albums(&self, artist: &str) -> Result<Vec<AlbumInfo>> {
        let v = self
            .conn
            .prepare(&format!(
                "SELECT album, MIN(year) FROM tracks
                 WHERE artist=?1 AND album IS NOT NULL AND album<>'' AND {ENABLED}
                 GROUP BY album ORDER BY album COLLATE NOCASE, album"
            ))?
            .query_map([artist], |r| {
                Ok(AlbumInfo {
                    name: r.get(0)?,
                    year: r.get::<_, Option<i64>>(1)?.map(|y| y as u16),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    pub fn album_tracks(&self, artist: &str, album: &str) -> Result<Vec<TrackId>> {
        let v = self
            .conn
            .prepare(&format!(
                "SELECT id FROM tracks WHERE artist=?1 AND album=?2 AND {ENABLED}
                 ORDER BY track_no IS NULL, track_no, rel_path"
            ))?
            .query_map([artist, album], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::{FailCode, Failure};

    #[test]
    fn a_transaction_that_reads_first_does_not_fail_on_another_connections_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        let mut scan = Catalog::open(&path).unwrap();
        let id = scan
            .upsert_track(DEFAULT_LIBRARY, "a.mp3", 1, 1, &TrackMeta::default())
            .unwrap();
        let other = std::thread::scope(|s| {
            let r = scan.transaction(|c| {
                assert!(c.file_unchanged(DEFAULT_LIBRARY, "a.mp3", 1, 1)?);
                // The player's writer saves loudness meanwhile.
                let writer = s.spawn(|| {
                    Catalog::open(&path)
                        .and_then(|w| w.set_measured_lufs(id, -10.0))
                        .map_err(|e| e.to_string())
                });
                std::thread::sleep(std::time::Duration::from_millis(300));
                c.mark_seen(DEFAULT_LIBRARY, "a.mp3")?;
                Ok(writer)
            });
            r.map(|w| w.join().unwrap())
        });
        assert_eq!(
            other.unwrap(),
            Ok(()),
            "both writes go through, one after the other"
        );
        assert_eq!(scan.track(id).unwrap().measured_lufs, Some(-10.0));
    }
    use crate::queue::Entry;
    use crate::source::Which;
    use crate::DEFAULT_LIBRARY;

    fn meta(genre: Option<&str>) -> TrackMeta {
        TrackMeta {
            raw_genre: genre.map(str::to_string),
            ..Default::default()
        }
    }

    fn add(c: &Catalog, path: &str, genre: Option<&str>) -> TrackId {
        c.upsert_track(DEFAULT_LIBRARY, path, 10, 100, &meta(genre))
            .unwrap()
    }

    fn count_of(c: &Catalog, key: &str) -> Option<usize> {
        c.genres()
            .unwrap()
            .into_iter()
            .find(|g| g.key == key)
            .map(|g| g.count)
    }

    #[test]
    fn upsert_and_unchanged_detection() {
        let c = Catalog::open_in_memory().unwrap();
        assert!(c.is_empty().unwrap());
        let id = c
            .upsert_track(DEFAULT_LIBRARY, "a/x.mp3", 10, 100, &meta(None))
            .unwrap();
        assert!(!c.is_empty().unwrap());
        assert!(c
            .file_unchanged(DEFAULT_LIBRARY, "a/x.mp3", 10, 100)
            .unwrap());
        assert!(!c
            .file_unchanged(DEFAULT_LIBRARY, "a/x.mp3", 10, 101)
            .unwrap());
        assert!(!c
            .file_unchanged(DEFAULT_LIBRARY, "a/x.mp3", 11, 100)
            .unwrap());
        assert!(!c
            .file_unchanged(DEFAULT_LIBRARY, "nope.mp3", 10, 100)
            .unwrap());
        let id2 = c
            .upsert_track(DEFAULT_LIBRARY, "a/x.mp3", 10, 101, &meta(Some("Rock")))
            .unwrap();
        assert_eq!(id, id2);
        assert_eq!(c.track(id).unwrap().meta.raw_genre.as_deref(), Some("Rock"));
    }

    #[test]
    fn multi_genre_track_counts_in_each() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", Some("Trance; Chillout"));
        c.rebuild_genres().unwrap();
        assert_eq!(count_of(&c, "trance"), Some(1));
        assert_eq!(count_of(&c, "chillout"), Some(1));
    }

    #[test]
    fn multi_genre_selection_dedups() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", Some("Trance; Chillout"));
        add(&c, "b.mp3", Some("Trance;Trance"));
        add(&c, "c.mp3", Some("DnB;Drum & Bass"));
        c.rebuild_genres().unwrap();
        let sel = vec!["trance".to_string(), "chillout".to_string()];
        assert_eq!(c.playable_tracks(&sel).unwrap().len(), 2);
        assert_eq!(count_of(&c, "trance"), Some(2));
        assert_eq!(count_of(&c, "drumandbass"), Some(1));
    }

    #[test]
    fn empty_selection_means_all() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", Some("Rock"));
        add(&c, "b.mp3", None);
        add(&c, "c.mp3", Some("Pop"));
        c.rebuild_genres().unwrap();
        assert_eq!(c.playable_tracks(&[]).unwrap().len(), 3);
    }

    #[test]
    fn folder_fallback_and_none_bucket() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "x/tagged.mp3", Some("Jazz"));
        let j = add(&c, "Jazz/x.mp3", None);
        let m = add(&c, "Misc/y.mp3", None);
        c.rebuild_genres().unwrap();
        let jazz = c.playable_tracks(&["jazz".to_string()]).unwrap();
        assert!(jazz.contains(&j) && jazz.len() == 2);
        let none = c.playable_tracks(&["_none".to_string()]).unwrap();
        assert_eq!(none, vec![m]);
        let g = c
            .genres()
            .unwrap()
            .into_iter()
            .find(|g| g.key == "_none")
            .unwrap();
        assert_eq!(g.display, "Unknown");
    }

    #[test]
    fn three_failures_exclude_until_rescan() {
        let c = Catalog::open_in_memory().unwrap();
        let id = add(&c, "a.mp3", Some("Rock"));
        c.rebuild_genres().unwrap();
        assert_eq!(
            c.record_failure(id, &failure(FailCode::Decode, "x"), None)
                .unwrap(),
            1
        );
        assert_eq!(
            c.record_failure(id, &failure(FailCode::Decode, "x"), None)
                .unwrap(),
            2
        );
        assert!(c.playable_tracks(&[]).unwrap().contains(&id));
        assert_eq!(
            c.record_failure(id, &failure(FailCode::Decode, "x"), None)
                .unwrap(),
            3
        );
        assert!(!c.playable_tracks(&[]).unwrap().contains(&id));
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        c.mark_seen(DEFAULT_LIBRARY, "a.mp3").unwrap();
        assert_eq!(c.finish_scan(DEFAULT_LIBRARY, &[]).unwrap(), 0);
        assert!(c.playable_tracks(&[]).unwrap().contains(&id));
    }

    #[test]
    fn success_resets_failures() {
        let c = Catalog::open_in_memory().unwrap();
        let id = add(&c, "a.mp3", None);
        c.record_failure(id, &failure(FailCode::Decode, "x"), None)
            .unwrap();
        c.record_failure(id, &failure(FailCode::Decode, "x"), None)
            .unwrap();
        c.record_success(id).unwrap();
        assert_eq!(
            c.record_failure(id, &failure(FailCode::Decode, "x"), None)
                .unwrap(),
            1
        );
    }

    #[test]
    fn banned_excluded_and_listed() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", Some("Rock"));
        let b = add(&c, "b.mp3", Some("Rock"));
        c.rebuild_genres().unwrap();
        c.set_banned(a, true).unwrap();
        assert_eq!(c.playable_tracks(&[]).unwrap(), vec![b]);
        assert_eq!(c.playable_tracks(&["rock".to_string()]).unwrap(), vec![b]);
        let banned = c.banned().unwrap();
        assert_eq!(banned.len(), 1);
        assert_eq!(banned[0].id, a);
        c.set_banned(a, false).unwrap();
        assert!(c.banned().unwrap().is_empty());
    }

    #[test]
    fn genre_rule_merges_into_a_known_genre() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", Some("Trance; Chillout"));
        add(&c, "b.mp3", Some("Chill Out"));
        add(&c, "c.mp3", Some("Downtempo"));
        c.rebuild_genres().unwrap();
        c.add_genre_rule("chillout", "downtempo").unwrap();
        let g = c.genres().unwrap();
        let down: Vec<_> = g.iter().filter(|g| g.key == "downtempo").collect();
        assert_eq!(down.len(), 1);
        assert_eq!(
            down[0].display, "Downtempo",
            "canonical name, not the typed one"
        );
        assert_eq!(down[0].count, 3);
        assert!(g.iter().all(|g| g.key != "chillout"));
        // survives a later rebuild
        c.rebuild_genres().unwrap();
        assert_eq!(count_of(&c, "downtempo"), Some(3));
    }

    #[test]
    fn merging_a_parent_genre_also_moves_its_subgenres() {
        let c = Catalog::open_in_memory().unwrap();
        let deep = add(&c, "a.mp3", Some("Deep House"));
        let house = add(&c, "b.mp3", Some("House"));
        let folder = add(&c, "Tech House/c.mp3", None);
        c.add_genre_rule("house", "Electronic").unwrap();
        for id in [deep, house, folder] {
            assert_eq!(c.track_genres(id).unwrap(), vec!["Electronic"]);
        }
        assert_eq!(count_of(&c, "house"), None);
        assert_eq!(count_of(&c, "electronic"), Some(3));
    }

    #[test]
    fn old_genre_keys_are_remapped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        {
            let c = Catalog::open(&db).unwrap();
            add(&c, "a.mp3", Some("R&B"));
            c.conn
                .execute_batch(
                    "INSERT INTO selection_state VALUES ('randb', 1), ('goa|trance', 1),
                        ('*', 1), ('rhythmandblues|worldmusic', 7), ('world|randb', 1);
                     INSERT INTO genre_rules VALUES ('randb', 'Soul'), ('jungle', 'Dubstep');
                     PRAGMA user_version = 2;",
                )
                .unwrap();
        }
        let c = Catalog::open(&db).unwrap();
        let rows = |sql: &str| -> Vec<(String, String)> {
            c.conn
                .prepare(sql)
                .unwrap()
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get::<_, rusqlite::types::Value>(1)?))
                })
                .unwrap()
                .map(|r| {
                    let (k, v) = r.unwrap();
                    (k, format!("{v:?}"))
                })
                .collect()
        };
        assert_eq!(
            rows("SELECT selection_key, last_first FROM selection_state ORDER BY 1"),
            vec![
                ("*".to_string(), "Integer(1)".to_string()),
                ("goamusic|trance".into(), "Integer(1)".into()),
                ("rhythmandblues".into(), "Integer(1)".into()),
                // Already there under the new key: that row wins.
                ("rhythmandblues|worldmusic".into(), "Integer(7)".into()),
            ]
        );
        assert_eq!(
            rows("SELECT from_key, to_display FROM genre_rules ORDER BY 1"),
            vec![
                ("junglemusic".to_string(), "Text(\"Dubstep\")".to_string()),
                ("rhythmandblues".into(), "Text(\"Soul\")".into()),
            ]
        );
        assert_eq!(
            c.track_genres(1).unwrap(),
            vec!["Soul"],
            "remapped rule applies"
        );
    }

    /// The `tracks`..`selection_state` schema as catalog version 3 (Task 16) left it,
    /// frozen here so later schema changes do not change the fixture.
    const SCHEMA_V3: &str = "
        CREATE TABLE tracks(
            id INTEGER PRIMARY KEY,
            rel_path TEXT NOT NULL UNIQUE,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL,
            artist TEXT, album TEXT, title TEXT,
            track_no INTEGER, duration_ms INTEGER,
            raw_genre TEXT,
            rg_track_gain REAL,
            measured_lufs REAL,
            fail_count INTEGER NOT NULL DEFAULT 0,
            banned INTEGER NOT NULL DEFAULT 0,
            present INTEGER NOT NULL DEFAULT 1
        );
        CREATE INDEX idx_tracks_artist_album ON tracks(artist, album);
        CREATE TABLE genres(id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, display TEXT NOT NULL);
        CREATE TABLE track_genres(
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            genre_id INTEGER NOT NULL REFERENCES genres(id) ON DELETE CASCADE,
            PRIMARY KEY(track_id, genre_id)
        );
        CREATE INDEX idx_track_genres_genre ON track_genres(genre_id);
        CREATE TABLE genre_rules(from_key TEXT PRIMARY KEY, to_display TEXT NOT NULL);
        CREATE TABLE selection_state(selection_key TEXT PRIMARY KEY, last_first INTEGER NOT NULL);
    ";

    /// A version-3 catalog file: three tracks (one banned, one with two failures and
    /// measured loudness), genres, a rule and a selection.
    fn v3_catalog(db: &Path) {
        let c = Connection::open(db).unwrap();
        c.execute_batch(SCHEMA_V3).unwrap();
        c.execute_batch(
            "INSERT INTO tracks(id, rel_path, size, mtime, artist, title, raw_genre,
                                measured_lufs, fail_count, banned)
             VALUES (1, 'Rock/a.mp3', 10, 100, 'A', 'Song A', 'Rock', -9.5, 2, 0),
                    (2, 'Rock/b.mp3', 20, 200, 'B', 'Song B', 'Rock', NULL, 0, 1),
                    (3, 'Jazz/c.mp3', 30, 300, 'C', 'Song C', 'Jazz', NULL, 3, 0);
             INSERT INTO genres VALUES (1, 'rock', 'Rock'), (2, 'jazz', 'Jazz');
             INSERT INTO track_genres VALUES (1, 1), (2, 1), (3, 2);
             INSERT INTO genre_rules VALUES ('jazz', 'Blues');
             INSERT INTO selection_state VALUES ('rock', 1), ('*', 3);
             PRAGMA user_version = 3;",
        )
        .unwrap();
    }

    fn user_version(c: &Catalog) -> i64 {
        c.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn v3_catalog_migrates_keeping_bans_failures_rules_and_selections() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v3_catalog(&db);
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), CATALOG_VERSION);
        assert_eq!(c.banned().unwrap().len(), 1);
        assert_eq!(c.banned().unwrap()[0].rel_path, "Rock/b.mp3");
        let a = c.track(1).unwrap();
        assert_eq!(a.meta.title.as_deref(), Some("Song A"));
        assert_eq!(a.meta.year, None);
        assert_eq!(a.measured_lufs, Some(-9.5));
        assert_eq!(
            c.playable_tracks(&[]).unwrap(),
            vec![1],
            "3 failures still excluded"
        );
        assert_eq!(c.last_first("rock").unwrap(), Some(1));
        assert_eq!(c.last_first("*").unwrap(), Some(3));
        let rules: Vec<(String, String)> = c
            .conn
            .prepare("SELECT from_key, to_display FROM genre_rules")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rules, vec![("jazz".to_string(), "Blues".to_string())]);
        // Genres were not touched by this migration (v3 genres stay as they were).
        assert_eq!(c.track_genres(3).unwrap(), vec!["Jazz"]);
        // Every track is re-read on the next scan, so it gets its year.
        for (path, size, mtime) in [("Rock/a.mp3", 10, 100), ("Rock/b.mp3", 20, 200)] {
            assert!(
                !c.file_unchanged(DEFAULT_LIBRARY, path, size, mtime)
                    .unwrap(),
                "{path}"
            );
        }
        // The failures listed so far have no stored reason.
        let u = c.unreadable().unwrap();
        assert_eq!(u.len(), 2);
        assert!(u.iter().all(|t| t.code.is_none()));
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v3_catalog(&db);
        {
            let c = Catalog::open(&db).unwrap();
            // A scan re-reads a.mp3.
            c.upsert_track(DEFAULT_LIBRARY, "Rock/a.mp3", 10, 100, &meta(Some("Rock")))
                .unwrap();
            assert!(c
                .file_unchanged(DEFAULT_LIBRARY, "Rock/a.mp3", 10, 100)
                .unwrap());
        }
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), CATALOG_VERSION);
        assert!(
            c.file_unchanged(DEFAULT_LIBRARY, "Rock/a.mp3", 10, 100)
                .unwrap(),
            "not marked again"
        );
        assert!(!c
            .file_unchanged(DEFAULT_LIBRARY, "Rock/b.mp3", 20, 200)
            .unwrap());
        assert_eq!(c.track(1).unwrap().measured_lufs, Some(-9.5));
        // Columns are only added once: a third open is fine as well.
        drop(c);
        Catalog::open(&db).unwrap();
    }

    #[test]
    fn a_failing_migration_step_does_not_stop_the_catalog_from_opening() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v3_catalog(&db);
        {
            let c = Connection::open(&db).unwrap();
            // The genre-key step deletes the old 'randb' row: make that fail.
            c.execute_batch(
                "INSERT INTO selection_state VALUES ('randb', 1);
                 CREATE TRIGGER no_delete BEFORE DELETE ON selection_state
                 BEGIN SELECT RAISE(ABORT, 'simulated failure'); END;
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), 2, "the failed step is retried next time");
        assert_eq!(c.last_first("randb").unwrap(), Some(1), "rolled back");
        // The new columns are there, so the app works.
        let id = c
            .upsert_track(
                DEFAULT_LIBRARY,
                "new.mp3",
                1,
                1,
                &TrackMeta {
                    year: Some(1991),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(c.track(id).unwrap().meta.year, Some(1991));
        assert_eq!(c.banned().unwrap().len(), 1);
    }

    #[test]
    fn migrating_a_large_catalog_is_fast() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(SCHEMA_V3).unwrap();
            c.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60000)
                 INSERT INTO tracks(rel_path, size, mtime, artist, title, raw_genre)
                 SELECT 'd' || (i % 50) || '/t' || i || '.mp3', i, i, 'A' || (i % 900),
                        'T' || i, 'Rock' FROM n;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let t = Instant::now();
        let c = Catalog::open(&db).unwrap();
        let took = t.elapsed();
        assert_eq!(user_version(&c), CATALOG_VERSION);
        assert!(!c
            .file_unchanged(DEFAULT_LIBRARY, "d1/t1.mp3", 1, 1)
            .unwrap());
        // Generous bound: debug builds on slow CI runners (Windows) take several seconds.
        assert!(took < Duration::from_secs(20), "migration took {took:?}");
    }

    fn failure(code: FailCode, msg: &str) -> Failure {
        Failure::new(code, msg)
    }

    #[test]
    fn failures_store_their_reason_and_are_listed_until_retried() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", Some("Rock"));
        let b = add(&c, "b.mp3", Some("Rock"));
        let banned = add(&c, "c.mp3", Some("Rock"));
        add(&c, "ok.mp3", Some("Rock"));
        assert!(c.unreadable().unwrap().is_empty());
        assert_eq!(
            c.record_failure(
                a,
                &failure(FailCode::NotFound, "gone"),
                Some(Which::Secondary)
            )
            .unwrap(),
            1
        );
        for _ in 0..3 {
            c.record_failure(b, &failure(FailCode::Decode, "bad frame"), None)
                .unwrap();
        }
        c.record_failure(banned, &failure(FailCode::Io, "x"), None)
            .unwrap();
        c.set_banned(banned, true).unwrap();
        let u = c.unreadable().unwrap();
        assert_eq!(u.len(), 2, "banned tracks are not listed: {u:?}");
        assert_eq!(c.unreadable_count().unwrap(), 2);
        let ua = u.iter().find(|t| t.id == a).unwrap();
        assert_eq!(ua.rel_path, "a.mp3");
        assert_eq!(ua.code.as_deref(), Some("not_found"));
        assert_eq!(ua.message.as_deref(), Some("gone"));
        assert_eq!(ua.fail_count, 1);
        assert_eq!(ua.server, Some(Which::Secondary));
        assert!(!ua.excluded);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((now - ua.at.unwrap()).abs() < 5);
        let ub = u.iter().find(|t| t.id == b).unwrap();
        assert_eq!((ub.fail_count, ub.excluded, ub.server), (3, true, None));

        // A rescan lets the track play again but keeps the reason on the list.
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        for p in ["a.mp3", "b.mp3", "c.mp3", "ok.mp3"] {
            c.mark_seen(DEFAULT_LIBRARY, p).unwrap();
        }
        c.finish_scan(DEFAULT_LIBRARY, &[]).unwrap();
        assert_eq!(c.unreadable().unwrap().len(), 2);
        assert!(c.playable_tracks(&[]).unwrap().contains(&b));

        c.retry_failed(a).unwrap();
        assert_eq!(c.unreadable().unwrap().len(), 1);
        c.record_failure(a, &failure(FailCode::NotFound, "gone"), None)
            .unwrap();
        assert_eq!(c.retry_all_failed().unwrap(), 2);
        assert!(c.unreadable().unwrap().is_empty());
        assert_eq!(c.banned().unwrap().len(), 1, "bans untouched");
    }

    #[test]
    fn playing_a_track_clears_its_failure() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", None);
        c.record_failure(a, &failure(FailCode::Decode, "d"), None)
            .unwrap();
        c.record_success(a).unwrap();
        assert!(c.unreadable().unwrap().is_empty());
    }

    /// A tagged track with measured loudness, marked for a new tag read (as after
    /// the migration to version 4).
    fn retag_fixture(c: &Catalog) -> TrackId {
        let m = TrackMeta {
            artist: Some("Artist".into()),
            title: Some("Song".into()),
            raw_genre: Some("Rock".into()),
            year: Some(1999),
            ..Default::default()
        };
        let id = c
            .upsert_track(DEFAULT_LIBRARY, "known.mp3", 10, 100, &m)
            .unwrap();
        c.set_measured_lufs(id, -11.0).unwrap();
        c.conn
            .execute("UPDATE tracks SET needs_retag=1", [])
            .unwrap();
        id
    }

    fn size_mtime(c: &Catalog, id: TrackId) -> (i64, i64) {
        c.conn
            .query_row("SELECT size, mtime FROM tracks WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
    }

    #[test]
    fn a_failed_re_read_keeps_tags_loudness_and_is_retried() {
        for code in [
            FailCode::Timeout,
            FailCode::Unreachable,
            FailCode::Io,
            FailCode::NotFound,
            FailCode::Tag,
        ] {
            let c = Catalog::open_in_memory().unwrap();
            let id = retag_fixture(&c);
            let got = c
                .record_scan_failure(DEFAULT_LIBRARY, "known.mp3", 10, 100, &failure(code, "x"))
                .unwrap();
            assert_eq!(got, id);
            let row = c.track(id).unwrap();
            assert_eq!(row.meta.artist.as_deref(), Some("Artist"), "{code:?}");
            assert_eq!(row.meta.year, Some(1999), "{code:?}");
            assert_eq!(row.measured_lufs, Some(-11.0), "{code:?}");
            assert_eq!(size_mtime(&c, id), (10, 100), "never zeroed: {code:?}");
            assert_eq!(
                c.file_unchanged(DEFAULT_LIBRARY, "known.mp3", 10, 100)
                    .unwrap(),
                !code.is_retried(),
                "retried unless the file is broken: {code:?}"
            );
            // A later successful read keeps the loudness of the unchanged file.
            c.upsert_track(DEFAULT_LIBRARY, "known.mp3", 10, 100, &row.meta)
                .unwrap();
            assert_eq!(c.track(id).unwrap().measured_lufs, Some(-11.0), "{code:?}");
            assert!(c
                .file_unchanged(DEFAULT_LIBRARY, "known.mp3", 10, 100)
                .unwrap());
        }
    }

    #[test]
    fn a_failed_read_of_a_changed_file_keeps_the_old_data_until_it_reads() {
        let c = Catalog::open_in_memory().unwrap();
        let id = retag_fixture(&c);
        c.record_scan_failure(
            DEFAULT_LIBRARY,
            "known.mp3",
            20,
            200,
            &failure(FailCode::Timeout, "t"),
        )
        .unwrap();
        assert_eq!(size_mtime(&c, id), (10, 100));
        assert_eq!(c.track(id).unwrap().meta.artist.as_deref(), Some("Artist"));
        assert!(!c
            .file_unchanged(DEFAULT_LIBRARY, "known.mp3", 20, 200)
            .unwrap());
        // Read at last: the file changed, so its old loudness goes.
        c.upsert_track(DEFAULT_LIBRARY, "known.mp3", 20, 200, &meta(None))
            .unwrap();
        assert_eq!(c.track(id).unwrap().measured_lufs, None);

        // Changed into a broken file: new size and time, old tags, no loudness.
        let id = retag_fixture(&c);
        c.record_scan_failure(
            DEFAULT_LIBRARY,
            "known.mp3",
            30,
            300,
            &failure(FailCode::Tag, "bad"),
        )
        .unwrap();
        assert_eq!(size_mtime(&c, id), (30, 300));
        let row = c.track(id).unwrap();
        assert_eq!(row.meta.artist.as_deref(), Some("Artist"));
        assert_eq!(row.measured_lufs, None);
        assert!(
            c.file_unchanged(DEFAULT_LIBRARY, "known.mp3", 30, 300)
                .unwrap(),
            "waits for a change"
        );
    }

    #[test]
    fn scan_failures_of_new_files_are_stored_and_retried() {
        let c = Catalog::open_in_memory().unwrap();
        let broken = c
            .record_scan_failure(
                DEFAULT_LIBRARY,
                "broken.mp3",
                5,
                50,
                &failure(FailCode::Tag, "bad"),
            )
            .unwrap();
        let slow = c
            .record_scan_failure(
                DEFAULT_LIBRARY,
                "slow.mp3",
                6,
                60,
                &failure(FailCode::Timeout, "t"),
            )
            .unwrap();
        assert_eq!(size_mtime(&c, broken), (5, 50));
        assert_eq!(size_mtime(&c, slow), (6, 60));
        assert!(
            c.file_unchanged(DEFAULT_LIBRARY, "broken.mp3", 5, 50)
                .unwrap(),
            "until it changes"
        );
        assert!(
            !c.file_unchanged(DEFAULT_LIBRARY, "slow.mp3", 6, 60)
                .unwrap(),
            "retried"
        );
        let listed: Vec<_> = c
            .unreadable()
            .unwrap()
            .into_iter()
            .map(|t| (t.id, t.code.unwrap(), t.fail_count))
            .collect();
        assert_eq!(
            listed,
            vec![(broken, "tag".to_string(), 0)],
            "network is not listed"
        );
        assert!(
            c.playable_tracks(&[]).unwrap().contains(&broken),
            "still plays"
        );
        c.upsert_track(DEFAULT_LIBRARY, "broken.mp3", 5, 50, &meta(None))
            .unwrap();
        assert!(
            c.unreadable().unwrap().is_empty(),
            "read: tag failure cleared"
        );
    }

    #[test]
    fn network_failures_of_playback_are_neither_counted_nor_listed() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", Some("Rock"));
        for code in [FailCode::Timeout, FailCode::Unreachable] {
            for _ in 0..3 {
                assert_eq!(c.record_failure(a, &failure(code, "x"), None).unwrap(), 0);
            }
        }
        assert!(c.unreadable().unwrap().is_empty());
        assert_eq!(c.unreadable_count().unwrap(), 0);
        assert!(c.playable_tracks(&[]).unwrap().contains(&a), "not excluded");
        // A file problem still counts.
        assert_eq!(
            c.record_failure(a, &failure(FailCode::Decode, "x"), None)
                .unwrap(),
            1
        );
        assert_eq!(c.unreadable_count().unwrap(), 1);
    }

    #[test]
    fn writes_for_removed_tracks_are_ignored() {
        let c = Catalog::open_in_memory().unwrap();
        assert_eq!(
            c.record_failure(42, &failure(FailCode::Decode, "x"), None)
                .unwrap(),
            0
        );
        c.set_last_first("*", 42).unwrap();
        assert_eq!(
            c.last_first("*").unwrap(),
            None,
            "no dangling selection row"
        );
    }

    #[test]
    fn reading_the_tags_clears_failures_it_disproves() {
        let c = Catalog::open_in_memory().unwrap();
        for code in [FailCode::NotFound, FailCode::Io, FailCode::Tag] {
            let a = add(&c, "a.mp3", None);
            c.record_scan_failure(DEFAULT_LIBRARY, "a.mp3", 10, 100, &failure(code, "x"))
                .unwrap();
            c.begin_scan(DEFAULT_LIBRARY).unwrap();
            assert_eq!(c.unreadable().unwrap().len(), 1, "{code:?}");
            c.upsert_track(DEFAULT_LIBRARY, "a.mp3", 10, 100, &meta(None))
                .unwrap();
            assert!(c.unreadable().unwrap().is_empty(), "{code:?}");
            assert_eq!(c.track(a).unwrap().rel_path, "a.mp3");
        }
    }

    #[test]
    fn re_reading_unchanged_tags_keeps_a_playback_failure() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", None);
        c.record_failure(a, &failure(FailCode::Decode, "bad"), None)
            .unwrap();
        // Scans (which reset the failure count) re-read the tags.
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        c.upsert_track(DEFAULT_LIBRARY, "a.mp3", 10, 100, &meta(None))
            .unwrap();
        assert_eq!(c.unreadable().unwrap().len(), 1);
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        c.upsert_track(DEFAULT_LIBRARY, "a.mp3", 11, 100, &meta(None))
            .unwrap();
        assert!(c.unreadable().unwrap().is_empty(), "the file changed");
    }

    #[test]
    fn years_are_stored_counted_and_filtered() {
        let c = Catalog::open_in_memory().unwrap();
        let mk = |path: &str, genre: &str, year: Option<u16>| {
            let m = TrackMeta {
                raw_genre: Some(genre.into()),
                year,
                ..Default::default()
            };
            c.upsert_track(DEFAULT_LIBRARY, path, 1, 1, &m).unwrap()
        };
        let r91 = mk("a.mp3", "Rock", Some(1991));
        let r95 = mk("b.mp3", "Rock", Some(1995));
        let j91 = mk("c.mp3", "Jazz", Some(1991));
        let rnone = mk("d.mp3", "Rock", None);
        let banned = mk("e.mp3", "Rock", Some(1991));
        c.set_banned(banned, true).unwrap();
        c.rebuild_genres().unwrap();
        assert_eq!(c.track(r91).unwrap().meta.year, Some(1991));
        assert_eq!(c.track(rnone).unwrap().meta.year, None);

        let yc = |keys: &[&str]| -> Vec<(u16, usize)> {
            let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            c.years(&keys)
                .unwrap()
                .into_iter()
                .map(|y| (y.year, y.count))
                .collect()
        };
        assert_eq!(yc(&[]), vec![(1991, 2), (1995, 1)]);
        assert_eq!(yc(&["rock"]), vec![(1991, 1), (1995, 1)]);
        assert_eq!(yc(&["jazz"]), vec![(1991, 1)]);

        let ids = |genre_keys: &[&str], years: Option<(u16, u16)>| -> Vec<TrackId> {
            let mut v = c
                .filtered_tracks(&TrackFilter {
                    genre_keys: genre_keys.iter().map(|k| k.to_string()).collect(),
                    years,
                    ..Default::default()
                })
                .unwrap();
            v.sort_unstable();
            v
        };
        assert_eq!(ids(&[], None), vec![r91, r95, j91, rnone]);
        assert_eq!(ids(&[], Some((1990, 1992))), vec![r91, j91]);
        assert_eq!(ids(&["rock"], Some((1991, 1995))), vec![r91, r95]);
        assert_eq!(ids(&["rock", "jazz"], Some((1991, 1991))), vec![r91, j91]);
        assert_eq!(ids(&["jazz"], Some((1992, 2000))), Vec::<TrackId>::new());
    }

    #[test]
    fn filters_by_text_and_unknown_year() {
        let c = Catalog::open_in_memory().unwrap();
        let mk = |path: &str, artist: Option<&str>, title: Option<&str>, year: Option<u16>| {
            let m = TrackMeta {
                artist: artist.map(str::to_string),
                title: title.map(str::to_string),
                raw_genre: Some("Rock".into()),
                year,
                ..Default::default()
            };
            c.upsert_track(DEFAULT_LIBRARY, path, 1, 1, &m).unwrap()
        };
        let a = mk("x/a.mp3", Some("Metallica"), Some("One"), Some(1988));
        let b = mk("x/b.mp3", Some("Ritam 100%"), Some("Two_Step"), None);
        let d = mk("x/Untagged Song.mp3", None, None, Some(2001));
        c.rebuild_genres().unwrap();
        let ids = |f: TrackFilter| -> Vec<TrackId> {
            let mut v = c.filtered_tracks(&f).unwrap();
            v.sort_unstable();
            v
        };
        let text = |t: &str| TrackFilter {
            text: Some(t.into()),
            ..Default::default()
        };
        assert_eq!(ids(text("metal")), [a], "artist, any case");
        assert_eq!(ids(text("one")), [a], "title");
        assert_eq!(ids(text("100%")), [b], "% is literal");
        assert_eq!(ids(text("twoxstep")), Vec::<TrackId>::new(), "_ is literal");
        assert_eq!(ids(text("two_")), [b]);
        assert_eq!(
            ids(text("untagged")),
            [d],
            "the file name stands in for a title"
        );
        assert_eq!(ids(text("  ")), [a, b, d], "blank: no text filter");
        let years = |years, unknown_year| TrackFilter {
            years,
            unknown_year,
            ..Default::default()
        };
        assert_eq!(ids(years(Some((1980, 1990)), false)), [a]);
        assert_eq!(ids(years(Some((1980, 1990)), true)), [a, b]);
        assert_eq!(ids(years(None, true)), [b], "only tracks without a year");
        assert_eq!(ids(years(None, false)), [a, b, d]);
    }

    #[test]
    fn facets_count_the_genres_and_years_of_given_tracks() {
        let c = Catalog::open_in_memory().unwrap();
        let mk = |path: &str, genre: &str, year: Option<u16>| {
            let m = TrackMeta {
                raw_genre: Some(genre.into()),
                year,
                ..Default::default()
            };
            c.upsert_track(DEFAULT_LIBRARY, path, 1, 1, &m).unwrap()
        };
        let a = mk("a.mp3", "Rock; Jazz", Some(1991));
        let b = mk("b.mp3", "Rock", None);
        let _other = mk("c.mp3", "Trance", Some(2000));
        c.rebuild_genres().unwrap();
        let f = c.facets(&[a, b, b]).unwrap();
        let genres: Vec<(String, usize)> =
            f.genres.iter().map(|g| (g.key.clone(), g.count)).collect();
        assert_eq!(genres, [("jazz".to_string(), 1), ("rock".to_string(), 2)]);
        assert_eq!(
            f.years,
            [YearCount {
                year: 1991,
                count: 1
            }]
        );
        assert_eq!(f.unknown_year, 1);
    }

    #[test]
    fn tracks_by_ids_keep_the_asked_order_and_skip_missing() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", None);
        let b = add(&c, "b.mp3", None);
        let rows = c.tracks_by_ids(&[b, 999, a, b]).unwrap();
        let got: Vec<TrackId> = rows.iter().map(|r| r.id).collect();
        assert_eq!(got, [b, a, b]);
        assert_eq!(rows[1].rel_path, "a.mp3");
    }

    #[test]
    fn the_playlist_is_saved_and_loaded() {
        use crate::queue::Entry;
        let c = Catalog::open_in_memory().unwrap();
        assert_eq!(c.load_playlist().unwrap(), None);
        let order: Vec<Entry> = (1..=60_000)
            .map(|i| {
                if i % 7 == 0 {
                    Entry::manual(i)
                } else {
                    Entry::new(i)
                }
            })
            .collect();
        let started = std::time::Instant::now();
        c.save_playlist_order(r#"{"genres":[]}"#, true, &order)
            .unwrap();
        let cursor = PlaylistCursor {
            cursor: Some(41),
            history: vec![5, 9, 42],
            history_pos: Some(2),
            position_ms: 61_500,
            resume: true,
        };
        c.save_playlist_cursor(&cursor).unwrap();
        let saved = c.load_playlist().unwrap().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(saved.selection, r#"{"genres":[]}"#);
        assert!(saved.shuffle);
        assert_eq!(saved.order, order);
        assert_eq!(saved.cursor, cursor);

        // The cursor alone changes often; the order stays.
        let later = PlaylistCursor {
            position_ms: 70_000,
            resume: false,
            ..cursor
        };
        c.save_playlist_cursor(&later).unwrap();
        let saved = c.load_playlist().unwrap().unwrap();
        assert_eq!(saved.cursor.position_ms, 70_000);
        assert!(!saved.cursor.resume);
        assert_eq!(saved.order.len(), 60_000);

        // A list without a cursor yet loads with an empty one.
        c.save_playlist_order("{}", false, &[Entry::new(1)])
            .unwrap();
        c.clear().unwrap();
        assert_eq!(c.load_playlist().unwrap(), None, "cleared with the library");
        c.save_playlist_order("{}", false, &[]).unwrap();
        let saved = c.load_playlist().unwrap().unwrap();
        assert_eq!(saved.cursor, PlaylistCursor::default());
    }

    #[test]
    fn a_new_order_and_its_cursor_are_saved_together_or_not_at_all() {
        let c = Catalog::open_in_memory().unwrap();
        let cursor = |at| PlaylistCursor {
            cursor: Some(at),
            ..Default::default()
        };
        c.save_playlist(
            Some(("{}", false, &[Entry::new(1), Entry::new(2)])),
            &cursor(1),
        )
        .unwrap();
        c.conn
            .execute_batch(
                "CREATE TRIGGER no_cursor BEFORE UPDATE ON playlist_cursor
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;
                 CREATE TRIGGER no_cursor_insert BEFORE INSERT ON playlist_cursor
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        let new_order = [Entry::new(7), Entry::new(8), Entry::new(9)];
        assert!(c
            .save_playlist(Some(("{}", true, &new_order)), &cursor(2))
            .is_err());
        let saved = c.load_playlist().unwrap().unwrap();
        assert_eq!(
            saved.order,
            [Entry::new(1), Entry::new(2)],
            "the old order stays"
        );
        assert_eq!(saved.cursor, cursor(1));
    }

    #[test]
    fn a_playlist_cursor_table_from_before_resume_gets_the_column() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        {
            let c = Catalog::open(&db).unwrap();
            c.conn
                .execute_batch(
                    "DROP TABLE playlist_cursor;
                     CREATE TABLE playlist_cursor(
                         id INTEGER PRIMARY KEY CHECK (id = 1), cursor INTEGER,
                         history BLOB NOT NULL, history_pos INTEGER,
                         position_ms INTEGER NOT NULL);",
                )
                .unwrap();
        }
        let c = Catalog::open(&db).unwrap();
        c.save_playlist_order("{}", false, &[Entry::new(1)])
            .unwrap();
        let cursor = PlaylistCursor {
            resume: true,
            position_ms: 5,
            ..Default::default()
        };
        c.save_playlist_cursor(&cursor).unwrap();
        assert_eq!(c.load_playlist().unwrap().unwrap().cursor, cursor);
    }

    #[test]
    fn a_playlist_of_another_format_or_with_bad_ids_is_not_misread() {
        let c = Catalog::open_in_memory().unwrap();
        c.save_playlist_order("{}", false, &[Entry::new(1), Entry::manual(2)])
            .unwrap();
        let mut blob = Vec::new();
        for id in [3i64, 0, i64::MIN, -4] {
            blob.extend(id.to_le_bytes());
        }
        c.conn
            .execute("UPDATE playlist SET entries=?1", [blob])
            .unwrap();
        let order = c.load_playlist().unwrap().unwrap().order;
        assert_eq!(order, [Entry::new(3), Entry::manual(4)], "bad ids dropped");
        c.conn.execute("UPDATE playlist SET format=99", []).unwrap();
        assert_eq!(
            c.load_playlist().unwrap(),
            None,
            "a newer format is not read"
        );
    }

    #[test]
    fn albums_come_with_their_year() {
        let c = Catalog::open_in_memory().unwrap();
        let m = |album: &str, year: Option<u16>| TrackMeta {
            artist: Some("A".into()),
            album: Some(album.into()),
            year,
            ..Default::default()
        };
        c.upsert_track(DEFAULT_LIBRARY, "1.mp3", 1, 1, &m("X", Some(1994)))
            .unwrap();
        c.upsert_track(DEFAULT_LIBRARY, "2.mp3", 1, 1, &m("X", Some(1993)))
            .unwrap();
        c.upsert_track(DEFAULT_LIBRARY, "3.mp3", 1, 1, &m("Y", None))
            .unwrap();
        assert_eq!(
            c.albums("A").unwrap(),
            vec![
                AlbumInfo {
                    name: "X".into(),
                    year: Some(1993)
                },
                AlbumInfo {
                    name: "Y".into(),
                    year: None
                },
            ]
        );
    }

    #[test]
    fn clear_empties_the_catalog_and_it_stays_usable() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v3_catalog(&db);
        let c = Catalog::open(&db).unwrap();
        let other = Catalog::open(&db).unwrap();
        c.record_failure(1, &failure(FailCode::Io, "x"), None)
            .unwrap();
        c.clear().unwrap();
        for table in [
            "tracks",
            "genres",
            "track_genres",
            "genre_rules",
            "selection_state",
        ] {
            let n: i64 = c
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        assert!(
            other.is_empty().unwrap(),
            "other connections see it at once"
        );
        assert!(other.genres().unwrap().is_empty());
        assert!(c.unreadable().unwrap().is_empty());
        let id = add(&c, "new.mp3", Some("Rock"));
        c.rebuild_genres().unwrap();
        assert_eq!(other.playable_tracks(&[]).unwrap(), vec![id]);
        drop((c, other));
        assert_eq!(user_version(&Catalog::open(&db).unwrap()), CATALOG_VERSION);
    }

    #[test]
    fn rule_to_an_unknown_name_is_ignored() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", Some("Chillout"));
        c.add_genre_rule("chillout", "ZZ Test Preimenovano")
            .unwrap();
        assert_eq!(count_of(&c, "chillout"), Some(1));
        assert!(c
            .genres()
            .unwrap()
            .iter()
            .all(|g| g.key != "zztestpreimenovano"));
    }

    #[test]
    fn unknown_tag_genres_go_to_unknown() {
        let c = Catalog::open_in_memory().unwrap();
        let zz = add(&c, "a.mp3", Some("ZZ Test Preimenovano"));
        let other = add(&c, "b.mp3", Some("Other"));
        let num = add(&c, "c.mp3", Some("(250)"));
        c.rebuild_genres().unwrap();
        let g = c.genres().unwrap();
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(
            (g[0].key.as_str(), g[0].display.as_str()),
            ("_none", "Unknown")
        );
        assert_eq!(g[0].count, 3);
        for id in [zz, other, num] {
            assert_eq!(c.track_genres(id).unwrap(), vec!["Unknown"]);
        }
    }

    #[test]
    fn known_tokens_win_over_unknown_ones() {
        let c = Catalog::open_in_memory().unwrap();
        let t = add(&c, "a.mp3", Some("Trance; Foo"));
        let d = add(&c, "b.mp3", Some("dnb"));
        c.rebuild_genres().unwrap();
        assert_eq!(c.track_genres(t).unwrap(), vec!["Trance"]);
        assert_eq!(c.track_genres(d).unwrap(), vec!["Drum & bass"]);
        assert_eq!(count_of(&c, "_none"), None);
        assert_eq!(count_of(&c, "drumandbass"), Some(1));
    }

    #[test]
    fn folder_fallback_only_for_known_genre_folders() {
        let c = Catalog::open_in_memory().unwrap();
        let chill = add(&c, "Chillout/x.mp3", None);
        let dnb = add(&c, "DnB/y.mp3", Some("Other"));
        let artists = add(&c, "Artists/z.mp3", None);
        c.rebuild_genres().unwrap();
        assert_eq!(c.track_genres(chill).unwrap(), vec!["Chillout"]);
        assert_eq!(c.track_genres(dnb).unwrap(), vec!["Drum & bass"]);
        assert_eq!(c.track_genres(artists).unwrap(), vec!["Unknown"]);
    }

    #[test]
    fn existing_catalog_is_recategorized_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        let (zz, rock) = {
            let c = Catalog::open(&db).unwrap();
            let zz = add(&c, "a.mp3", Some("ZZ Test Preimenovano"));
            let rock = add(&c, "b.mp3", Some("Rock"));
            add(&c, "c.mp3", None);
            // What an older version left behind: every tag value a genre, Croatian
            // name for the empty one, and no genre version.
            c.conn
                .execute_batch(
                    "DELETE FROM track_genres; DELETE FROM genres;
                     INSERT INTO genres(id, key, display) VALUES
                        (1, 'zztestpreimenovano', 'ZZ Test Preimenovano'),
                        (2, 'rock', 'Rock'), (3, '_none', '(bez žanra)');
                     INSERT INTO track_genres VALUES (1, 1), (2, 2), (3, 3);
                     INSERT INTO genre_rules VALUES ('rock', 'ZZ Test Preimenovano');
                     PRAGMA user_version = 0;",
                )
                .unwrap();
            (zz, rock)
        };
        let c = Catalog::open(&db).unwrap();
        let g = c.genres().unwrap();
        let shown: Vec<(&str, &str, usize)> = g
            .iter()
            .map(|g| (g.key.as_str(), g.display.as_str(), g.count))
            .collect();
        assert_eq!(shown, vec![("rock", "Rock", 1), ("_none", "Unknown", 2)]);
        assert_eq!(c.track_genres(zz).unwrap(), vec!["Unknown"]);
        assert_eq!(c.track_genres(rock).unwrap(), vec!["Rock"]);
        // Ignored rules are kept, not deleted.
        let rules: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM genre_rules", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rules, 1);
    }

    #[test]
    fn finish_scan_removes_missing() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", Some("Rock"));
        add(&c, "b.mp3", Some("Rock"));
        c.rebuild_genres().unwrap();
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        c.mark_seen(DEFAULT_LIBRARY, "b.mp3").unwrap();
        assert_eq!(c.finish_scan(DEFAULT_LIBRARY, &[]).unwrap(), 1);
        assert!(c.track(a).is_err());
        c.rebuild_genres().unwrap();
        assert_eq!(count_of(&c, "rock"), Some(1));
    }

    #[test]
    fn finish_scan_keeps_tracks_under_folders_the_walk_skipped() {
        let c = Catalog::open_in_memory().unwrap();
        let jazz = add(&c, "Jazz/a.mp3", Some("Jazz"));
        let deep = add(&c, "Jazz/Live/b.mp3", Some("Jazz"));
        let jazzy = add(&c, "Jazzy/c.mp3", Some("Jazz"));
        let gone = add(&c, "Rock/d.mp3", Some("Rock"));
        let seen = add(&c, "Rock/e.mp3", Some("Rock"));
        c.set_banned(jazz, true).unwrap();
        c.set_measured_lufs(deep, -9.0).unwrap();
        c.set_last_first("jazz", deep).unwrap();
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        assert_eq!(c.absent_count(), 5);
        c.mark_seen(DEFAULT_LIBRARY, "Rock/e.mp3").unwrap();
        assert_eq!(
            c.finish_scan(DEFAULT_LIBRARY, &["Jazz".to_string()])
                .unwrap(),
            2
        );
        assert!(c.track(gone).is_err(), "not seen elsewhere: swept");
        assert!(c.track(jazzy).is_err(), "Jazzy is not under Jazz");
        assert!(c.track(seen).is_ok());
        assert_eq!(c.banned().unwrap()[0].id, jazz, "ban kept");
        assert_eq!(c.track(deep).unwrap().measured_lufs, Some(-9.0));
        assert_eq!(c.last_first("jazz").unwrap(), Some(deep));
        assert_eq!(c.absent_count(), 0, "kept rows are present again");
    }

    #[test]
    fn abort_scan_makes_every_track_present_again() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", None);
        add(&c, "b.mp3", None);
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        c.mark_seen(DEFAULT_LIBRARY, "a.mp3").unwrap();
        assert_eq!(c.absent_count(), 1);
        c.abort_scan(DEFAULT_LIBRARY).unwrap();
        assert_eq!(c.absent_count(), 0);
        assert_eq!(
            c.finish_scan(DEFAULT_LIBRARY, &[]).unwrap(),
            0,
            "nothing to sweep"
        );
    }

    #[test]
    fn selection_key_is_order_independent() {
        let ab = selection_key(&["a".to_string(), "b".to_string()]);
        assert_eq!(ab, selection_key(&["b".to_string(), "a".to_string()]));
        assert_eq!(ab, "a|b");
        assert_eq!(selection_key(&[]), "*");
    }

    #[test]
    fn last_first_and_lufs_and_browse() {
        let c = Catalog::open_in_memory().unwrap();
        assert_eq!(c.last_first("*").unwrap(), None);
        let m = |artist: &str, album: &str, no: u32| TrackMeta {
            artist: Some(artist.into()),
            album: Some(album.into()),
            track_no: Some(no),
            ..Default::default()
        };
        let t2 = c
            .upsert_track(DEFAULT_LIBRARY, "b/2.mp3", 1, 1, &m("A", "X", 2))
            .unwrap();
        let t1 = c
            .upsert_track(DEFAULT_LIBRARY, "b/1.mp3", 1, 1, &m("A", "X", 1))
            .unwrap();
        c.upsert_track(DEFAULT_LIBRARY, "b/3.mp3", 1, 1, &m("B", "Y", 1))
            .unwrap();
        c.set_last_first("*", t2).unwrap();
        c.set_last_first("*", t1).unwrap();
        assert_eq!(c.last_first("*").unwrap(), Some(t1));
        c.set_measured_lufs(t1, -20.5).unwrap();
        assert_eq!(c.track(t1).unwrap().measured_lufs, Some(-20.5));
        assert_eq!(c.artists().unwrap(), vec!["A", "B"]);
        assert_eq!(c.albums("A").unwrap()[0].name, "X");
        assert_eq!(c.album_tracks("A", "X").unwrap(), vec![t1, t2]);
    }

    #[test]
    fn genres_query_fast_on_50k_tracks() {
        use std::time::{Duration, Instant};
        let mut c = Catalog::open_in_memory().unwrap();
        c.transaction(|c| {
            let names = crate::genre::known_genre_names();
            for i in 0..50_000 {
                let m = meta(Some(&names[i % 40]));
                c.upsert_track(
                    DEFAULT_LIBRARY,
                    &format!("g{}/t{}.mp3", i % 40, i),
                    1,
                    1,
                    &m,
                )?;
            }
            Ok(())
        })
        .unwrap();
        c.rebuild_genres().unwrap();
        let t = Instant::now();
        assert_eq!(c.genres().unwrap().len(), 40);
        let d1 = t.elapsed();
        let t = Instant::now();
        assert_eq!(c.playable_tracks(&[]).unwrap().len(), 50_000);
        let d2 = t.elapsed();
        assert!(d1 < Duration::from_millis(200), "genres() took {d1:?}");
        assert!(
            d2 < Duration::from_millis(200),
            "playable_tracks took {d2:?}"
        );
    }

    #[test]
    fn track_genres_lists_display_names() {
        let c = Catalog::open_in_memory().unwrap();
        let a = add(&c, "a.mp3", Some("trance; Chill Out"));
        add(&c, "b.mp3", Some("Trance"));
        add(&c, "c.mp3", Some("Trance"));
        let none = add(&c, "d.mp3", None);
        c.rebuild_genres().unwrap();
        assert_eq!(c.track_genres(a).unwrap(), vec!["Chillout", "Trance"]);
        assert_eq!(c.track_genres(none).unwrap(), vec!["Unknown"]);
        assert!(c.track_genres(999).unwrap().is_empty());
    }

    /// The whole schema as catalog version 4 left it (before libraries), frozen here
    /// so later schema changes do not change the fixture.
    const SCHEMA_V4: &str = "
        CREATE TABLE tracks(
            id INTEGER PRIMARY KEY,
            rel_path TEXT NOT NULL UNIQUE,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL,
            artist TEXT, album TEXT, title TEXT,
            track_no INTEGER, duration_ms INTEGER,
            raw_genre TEXT,
            rg_track_gain REAL,
            measured_lufs REAL,
            fail_count INTEGER NOT NULL DEFAULT 0,
            banned INTEGER NOT NULL DEFAULT 0,
            present INTEGER NOT NULL DEFAULT 1,
            year INTEGER,
            needs_retag INTEGER NOT NULL DEFAULT 0,
            last_error TEXT, last_error_msg TEXT, last_error_at INTEGER,
            last_error_server TEXT
        );
        CREATE INDEX idx_tracks_artist_album ON tracks(artist, album);
        CREATE INDEX idx_tracks_year ON tracks(year);
        CREATE TABLE genres(id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, display TEXT NOT NULL);
        CREATE TABLE track_genres(
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            genre_id INTEGER NOT NULL REFERENCES genres(id) ON DELETE CASCADE,
            PRIMARY KEY(track_id, genre_id)
        );
        CREATE INDEX idx_track_genres_genre ON track_genres(genre_id);
        CREATE TABLE genre_rules(from_key TEXT PRIMARY KEY, to_display TEXT NOT NULL);
        CREATE TABLE selection_state(selection_key TEXT PRIMARY KEY, last_first INTEGER NOT NULL);
        CREATE TABLE playlist(
            id INTEGER PRIMARY KEY CHECK (id = 1), selection TEXT NOT NULL,
            shuffle INTEGER NOT NULL, entries BLOB NOT NULL, format INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE playlist_cursor(
            id INTEGER PRIMARY KEY CHECK (id = 1), cursor INTEGER, history BLOB NOT NULL,
            history_pos INTEGER, position_ms INTEGER NOT NULL, resume INTEGER NOT NULL DEFAULT 1
        );
    ";

    /// A version-4 catalog file as the owner's is: tagged tracks with years and
    /// loudness, a ban, failures with reasons, genres, a rule, selections, and a
    /// saved playlist with a hand-added entry, cursor and history.
    fn v4_catalog(db: &Path) {
        let c = Connection::open(db).unwrap();
        c.execute_batch(SCHEMA_V4).unwrap();
        c.execute_batch(
            "INSERT INTO tracks(id, rel_path, size, mtime, artist, title, raw_genre, year,
                                measured_lufs, fail_count, banned, last_error, last_error_msg,
                                last_error_at, last_error_server)
             VALUES (1, 'Rock/a.mp3', 10, 100, 'A', 'Song A', 'Rock', 1991, -9.5, 0, 0,
                     NULL, NULL, NULL, NULL),
                    (2, 'Rock/b.mp3', 20, 200, 'B', 'Song B', 'Rock', 1995, NULL, 0, 1,
                     NULL, NULL, NULL, NULL),
                    (3, 'Jazz/c.mp3', 30, 300, 'C', 'Song C', 'Jazz', NULL, NULL, 3, 0,
                     'decode', 'bad frame', 1700000000, 'secondary'),
                    (4, 'Jazz/d.mp3', 40, 400, 'D', 'Song D', 'Jazz', 2001, -12.0, 1, 0,
                     'not_found', 'gone', 1700000001, 'primary');
             INSERT INTO genres VALUES (1, 'rock', 'Rock'), (2, 'jazz', 'Jazz');
             INSERT INTO track_genres VALUES (1, 1), (2, 1), (3, 2), (4, 2);
             INSERT INTO genre_rules VALUES ('bebop', 'Jazz');
             INSERT INTO selection_state VALUES ('rock', 1), ('*', 4);
             INSERT INTO playlist VALUES (1, '{\"genres\":[\"rock\"]}', 1,
                 X'01000000000000000400000000000000FDFFFFFFFFFFFFFF', 1);
             INSERT INTO playlist_cursor VALUES (1, 1, X'01000000000000000400000000000000',
                 1, 61500, 1);
             PRAGMA user_version = 4;",
        )
        .unwrap();
    }

    fn library_of(c: &Catalog, id: TrackId) -> LibraryId {
        c.track(id).unwrap().library_id
    }

    #[test]
    fn ids_of_deleted_tracks_are_not_given_out_again() {
        let c = Catalog::open_in_memory().unwrap();
        let m = TrackMeta::default();
        c.upsert_track(1, "a.mp3", 1, 1, &m).unwrap();
        let last = c.upsert_track(2, "b.mp3", 1, 1, &m).unwrap();
        c.remove_library(2).unwrap();
        let new = c.upsert_track(1, "c.mp3", 1, 1, &m).unwrap();
        assert!(new > last, "{new} reuses a deleted id");
        // Also in a catalog migrated from version 4.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v4_catalog(&db);
        let c = Catalog::open(&db).unwrap();
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        for p in ["Rock/a.mp3", "Rock/b.mp3", "Jazz/c.mp3"] {
            c.mark_seen(DEFAULT_LIBRARY, p).unwrap();
        }
        assert_eq!(
            c.finish_scan(DEFAULT_LIBRARY, &[]).unwrap(),
            1,
            "track 4 swept"
        );
        assert_eq!(c.upsert_track(1, "new.mp3", 1, 1, &m).unwrap(), 5);
    }

    #[test]
    fn orphan_genre_links_from_an_old_build_do_not_block_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v4_catalog(&db);
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "PRAGMA foreign_keys=OFF; INSERT INTO track_genres VALUES (99, 1), (1, 77);",
            )
            .unwrap();
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), CATALOG_VERSION);
        let links: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM track_genres", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 4, "the orphans are gone, the rest kept");
        assert_eq!(c.track_genres(1).unwrap(), vec!["Rock"]);
    }

    #[test]
    fn v4_catalog_moves_to_library_1_keeping_everything() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v4_catalog(&db);
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), CATALOG_VERSION);
        for id in 1..=4 {
            assert_eq!(library_of(&c, id), DEFAULT_LIBRARY, "track {id}");
        }
        // Bans, loudness, years, failures.
        assert_eq!(c.banned().unwrap()[0].id, 2);
        assert_eq!(c.track(1).unwrap().measured_lufs, Some(-9.5));
        assert_eq!(c.track(4).unwrap().meta.year, Some(2001));
        assert_eq!(
            c.playable_tracks(&[]).unwrap(),
            vec![1, 4],
            "3 failures excluded"
        );
        let u = c.unreadable().unwrap();
        let u3 = u.iter().find(|t| t.id == 3).unwrap();
        assert_eq!(
            (u3.code.as_deref(), u3.fail_count, u3.server),
            (Some("decode"), 3, Some(Which::Secondary))
        );
        assert_eq!(u3.library_id, DEFAULT_LIBRARY);
        let u4 = u.iter().find(|t| t.id == 4).unwrap();
        assert_eq!(
            (u4.message.as_deref(), u4.at),
            (Some("gone"), Some(1_700_000_001))
        );
        // Genres (links kept, not rebuilt), rules, selections.
        assert_eq!(c.track_genres(3).unwrap(), vec!["Jazz"]);
        assert_eq!(
            count_of(&c, "rock"),
            Some(1),
            "the banned one is not counted"
        );
        assert_eq!(c.last_first("rock").unwrap(), Some(1));
        assert_eq!(c.last_first("*").unwrap(), Some(4));
        let rules: i64 = c
            .conn
            .query_row("SELECT COUNT(*) FROM genre_rules", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rules, 1);
        // The saved playlist, cursor and history.
        let saved = c.load_playlist().unwrap().unwrap();
        assert_eq!(saved.selection, r#"{"genres":["rock"]}"#);
        assert_eq!(
            saved.order,
            [Entry::new(1), Entry::new(4), Entry::manual(3)]
        );
        assert_eq!(saved.cursor.history, [1, 4]);
        assert_eq!(
            (
                saved.cursor.cursor,
                saved.cursor.position_ms,
                saved.cursor.resume
            ),
            (Some(1), 61_500, true)
        );
        // Unchanged files stay unchanged: no re-read of the whole library.
        assert!(c
            .file_unchanged(DEFAULT_LIBRARY, "Rock/a.mp3", 10, 100)
            .unwrap());
        // The same path can now be in another library.
        let other = c
            .upsert_track(2, "Rock/a.mp3", 1, 1, &meta(Some("Pop")))
            .unwrap();
        assert_ne!(other, 1);
        assert_eq!(c.track(1).unwrap().meta.title.as_deref(), Some("Song A"));
        // Indexes are back.
        let indexes: Vec<String> = c
            .conn
            .prepare("SELECT name FROM pragma_index_list('tracks') ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for want in [
            "idx_tracks_artist_album",
            "idx_tracks_library_path",
            "idx_tracks_year",
        ] {
            assert!(indexes.iter().any(|i| i == want), "{want} in {indexes:?}");
        }
        drop(c);
        // Opening again changes nothing.
        let c = Catalog::open(&db).unwrap();
        assert_eq!(c.track(4).unwrap().meta.year, Some(2001));
        assert_eq!(library_of(&c, other), 2);
    }

    #[test]
    fn a_failed_library_migration_leaves_a_working_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        v4_catalog(&db);
        {
            // The rebuild copies the tracks: make that fail.
            let c = Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE tracks_v5(x);").unwrap();
        }
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), 4, "retried next start");
        assert!(
            !c.libraries_ready(),
            "only the first library can be scanned"
        );
        assert_eq!(library_of(&c, 1), DEFAULT_LIBRARY, "the column is there");
        assert_eq!(c.track_genres(1).unwrap(), vec!["Rock"], "rolled back");
        c.upsert_track(DEFAULT_LIBRARY, "new.mp3", 1, 1, &meta(None))
            .unwrap();
        c.begin_scan(DEFAULT_LIBRARY).unwrap();
        c.mark_seen(DEFAULT_LIBRARY, "new.mp3").unwrap();
        c.abort_scan(DEFAULT_LIBRARY).unwrap();
        assert_eq!(c.banned().unwrap().len(), 1);
        assert!(c.load_playlist().unwrap().is_some());
        // Not tried again by the other opens of this run (each would copy every
        // track), even once it would work: the next start tries again.
        c.conn.execute_batch("DROP TABLE tracks_v5").unwrap();
        drop(c);
        let again = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&again), 4);
        forget_failed_migrations();
        let next_start = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&next_start), CATALOG_VERSION);
        assert!(next_start.libraries_ready());
    }

    #[test]
    fn the_same_path_in_two_libraries_is_two_tracks() {
        let c = Catalog::open_in_memory().unwrap();
        let a = c
            .upsert_track(1, "Rock/x.mp3", 1, 1, &meta(Some("Rock")))
            .unwrap();
        let b = c
            .upsert_track(2, "Rock/x.mp3", 2, 2, &meta(Some("Jazz")))
            .unwrap();
        assert_ne!(a, b);
        assert!(c.file_unchanged(1, "Rock/x.mp3", 1, 1).unwrap());
        assert!(!c.file_unchanged(2, "Rock/x.mp3", 1, 1).unwrap());
        assert!(c.file_unchanged(2, "Rock/x.mp3", 2, 2).unwrap());
        c.rebuild_genres().unwrap();
        assert_eq!(c.track_genres(a).unwrap(), vec!["Rock"]);
        assert_eq!(c.track_genres(b).unwrap(), vec!["Jazz"]);
        assert_eq!((library_of(&c, a), library_of(&c, b)), (1, 2));
        // A scan failure lands on the right one.
        c.record_scan_failure(2, "Rock/x.mp3", 2, 2, &failure(FailCode::Tag, "bad"))
            .unwrap();
        let u = c.unreadable().unwrap();
        assert_eq!((u.len(), u[0].id, u[0].library_id), (1, b, 2));
    }

    #[test]
    fn a_scan_of_one_library_never_sweeps_another() {
        let c = Catalog::open_in_memory().unwrap();
        let a = c.upsert_track(1, "a.mp3", 1, 1, &meta(None)).unwrap();
        let b = c.upsert_track(2, "b.mp3", 1, 1, &meta(None)).unwrap();
        let gone = c.upsert_track(2, "gone.mp3", 1, 1, &meta(None)).unwrap();
        c.record_failure(a, &failure(FailCode::Decode, "x"), None)
            .unwrap();
        c.begin_scan(2).unwrap();
        c.mark_seen(2, "b.mp3").unwrap();
        // The same path in library 1 is not marked by library 2's scan.
        c.mark_seen(2, "a.mp3").unwrap();
        assert_eq!(c.finish_scan(2, &[]).unwrap(), 1);
        assert!(c.track(gone).is_err());
        assert!(c.track(a).is_ok() && c.track(b).is_ok());
        assert_eq!(
            c.unreadable().unwrap()[0].fail_count,
            1,
            "library 1's failures are not reset by library 2's scan"
        );
        // The whole of library 2 unseen (a skipped root): nothing swept.
        c.begin_scan(2).unwrap();
        assert_eq!(c.finish_scan(2, &[String::new()]).unwrap(), 0);
        assert!(c.track(b).is_ok());
        c.begin_scan(1).unwrap();
        c.abort_scan(1).unwrap();
        assert_eq!(c.absent_count(), 0);
    }

    #[test]
    fn a_disabled_library_is_hidden_but_kept() {
        let c = Catalog::open_in_memory().unwrap();
        let m = |artist: &str, genre: &str| TrackMeta {
            artist: Some(artist.into()),
            album: Some("Album".into()),
            raw_genre: Some(genre.into()),
            year: Some(1990),
            ..Default::default()
        };
        let a = c.upsert_track(1, "a.mp3", 1, 1, &m("A", "Rock")).unwrap();
        let b = c.upsert_track(2, "b.mp3", 1, 1, &m("B", "Jazz")).unwrap();
        let b2 = c.upsert_track(2, "b2.mp3", 1, 1, &m("B", "Rock")).unwrap();
        c.record_failure(b, &failure(FailCode::Decode, "x"), None)
            .unwrap();
        c.set_banned(b2, true).unwrap();
        c.rebuild_genres().unwrap();
        assert_eq!(c.library_track_counts().unwrap()[&2], 2);

        c.set_disabled_libraries(&[2]).unwrap();
        assert_eq!(c.disabled_libraries().unwrap(), [2].into());
        assert_eq!(c.playable_tracks(&[]).unwrap(), vec![a]);
        let keys: Vec<String> = c.genres().unwrap().into_iter().map(|g| g.key).collect();
        assert_eq!(keys, ["rock"]);
        assert_eq!(count_of(&c, "rock"), Some(1));
        assert_eq!(c.artists().unwrap(), ["A"]);
        assert!(c.albums("B").unwrap().is_empty());
        assert!(c.album_tracks("B", "Album").unwrap().is_empty());
        assert_eq!(c.years(&[]).unwrap()[0].count, 1);
        assert!(c.unreadable().unwrap().is_empty());
        assert_eq!(c.unreadable_count().unwrap(), 0);
        assert!(c.banned().unwrap().is_empty());
        assert!(c.track(b).is_ok(), "kept in the catalog");
        // Nor in the playlist's rows, facets or export.
        assert_eq!(
            c.tracks_by_ids(&[b, a, b2])
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [a]
        );
        let f = c.facets(&[a, b, b2]).unwrap();
        assert_eq!(f.genres.iter().map(|g| g.count).sum::<usize>(), 1);
        assert_eq!(f.years[0].count, 1);
        // A catalog with only hidden tracks counts as empty.
        let only = Catalog::open_in_memory().unwrap();
        only.upsert_track(4, "x.mp3", 1, 1, &meta(None)).unwrap();
        assert!(!only.is_empty().unwrap());
        only.set_disabled_libraries(&[4]).unwrap();
        assert!(only.is_empty().unwrap());
        assert_eq!(c.library_track_counts().unwrap()[&2], 2);

        c.set_disabled_libraries(&[]).unwrap();
        assert_eq!(c.playable_tracks(&[]).unwrap().len(), 2);
        assert_eq!(c.unreadable_count().unwrap(), 1);
        assert_eq!(c.banned().unwrap().len(), 1);
    }

    #[test]
    fn removing_a_library_deletes_only_its_tracks() {
        let c = Catalog::open_in_memory().unwrap();
        let a = c
            .upsert_track(1, "x.mp3", 1, 1, &meta(Some("Rock")))
            .unwrap();
        let b = c
            .upsert_track(2, "x.mp3", 1, 1, &meta(Some("Jazz")))
            .unwrap();
        c.upsert_track(2, "y.mp3", 1, 1, &meta(Some("Jazz")))
            .unwrap();
        c.rebuild_genres().unwrap();
        c.set_last_first("jazz", b).unwrap();
        c.set_last_first("rock", a).unwrap();
        c.set_disabled_libraries(&[2]).unwrap();
        assert_eq!(c.remove_library(2).unwrap(), 2);
        assert!(c.track(b).is_err());
        assert!(c.track(a).is_ok());
        assert_eq!(c.last_first("jazz").unwrap(), None);
        assert_eq!(c.last_first("rock").unwrap(), Some(a));
        let keys: Vec<String> = c.genres().unwrap().into_iter().map(|g| g.key).collect();
        assert_eq!(keys, ["rock"], "its genres are gone");
        assert!(c.disabled_libraries().unwrap().is_empty());
        assert_eq!(c.remove_library(2).unwrap(), 0);
    }
}
