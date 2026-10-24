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
use crate::source::Which;
use crate::{Result, TrackId};

/// Catalog version, kept in `PRAGMA user_version`; see [`MIGRATIONS`].
const CATALOG_VERSION: i64 = 4;
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
    pub rel_path: String,
    pub meta: TrackMeta,
    pub measured_lufs: Option<f32>,
}

/// A track that failed to open, tag or decode (see [`Catalog::unreadable`]).
#[derive(Debug, Clone, PartialEq)]
pub struct UnreadableTrack {
    pub id: TrackId,
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
    /// Only tracks whose year is within `from..=to` (tracks without a year drop out).
    pub years: Option<(u16, u16)>,
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

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks(
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
CREATE INDEX IF NOT EXISTS idx_tracks_artist_album ON tracks(artist, album);
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
";

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
];

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
];

/// `upsert_track` keeps a stored failure only when the file did not change and the
/// failure was in decoding the audio, which a successful tag read does not disprove.
const KEEP_ERROR: &str = "size=excluded.size AND mtime=excluded.mtime \
                          AND last_error IN ('decode', 'unsupported')";

/// Tracks the "Unreadable" list shows: not banned, and failed (network trouble
/// stored by an older build aside).
const UNREADABLE: &str = "banned=0 AND (fail_count>0 OR (last_error IS NOT NULL \
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

/// Playable (not banned, not excluded) tracks in `f`'s genres and years, as a
/// condition on `tracks t` and its parameters.
fn filter_sql(f: &TrackFilter) -> (String, Vec<Value>) {
    let mut sql = format!("t.banned=0 AND t.fail_count<{MAX_FAILS}");
    let mut args = Vec::new();
    if !f.genre_keys.is_empty() {
        let marks = vec!["?"; f.genre_keys.len()].join(",");
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM track_genres tg JOIN genres g ON g.id = tg.genre_id
                          WHERE tg.track_id = t.id AND g.key IN ({marks}))"
        ));
        args.extend(f.genre_keys.iter().cloned().map(Value::Text));
    }
    if let Some((from, to)) = f.years {
        sql.push_str(" AND t.year BETWEEN ? AND ?");
        args.extend([Value::Integer(from.into()), Value::Integer(to.into())]);
    }
    (sql, args)
}

const TRACK_COLS: &str = "id, rel_path, artist, album, title, track_no, duration_ms, \
                          raw_genre, rg_track_gain, measured_lufs, year";

fn row_to_track(r: &Row) -> rusqlite::Result<TrackRow> {
    Ok(TrackRow {
        id: r.get(0)?,
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
            self.conn
                .execute_batch("CREATE INDEX IF NOT EXISTS idx_tracks_year ON tracks(year)")?;
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
        for (to, what, step) in MIGRATIONS.iter().filter(|(to, ..)| *to > version) {
            let started = std::time::Instant::now();
            let done = self.savepoint(|| {
                step(self)?;
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
                    return;
                }
            }
        }
        debug_assert!(MIGRATIONS.last().is_some_and(|m| m.0 == CATALOG_VERSION));
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
        self.conn.execute_batch("BEGIN")?;
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

    pub fn is_empty(&self) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
        Ok(n == 0)
    }

    /// The file is known with this size and time, and its tags need no new read.
    pub fn file_unchanged(&self, rel_path: &str, size: u64, mtime: i64) -> Result<bool> {
        let hit = self
            .conn
            .prepare_cached(
                "SELECT 1 FROM tracks
                 WHERE rel_path=?1 AND size=?2 AND mtime=?3 AND needs_retag=0",
            )?
            .exists(params![rel_path, size as i64, mtime])?;
        Ok(hit)
    }

    /// Insert or update a track by path after reading its tags; marks it present. A
    /// changed file loses its measured loudness and stored failure, a tag failure
    /// is cleared; the failure count is left to `begin_scan`/`record_success`.
    pub fn upsert_track(
        &self,
        rel_path: &str,
        size: u64,
        mtime: i64,
        meta: &TrackMeta,
    ) -> Result<TrackId> {
        let sql = format!(
            "INSERT INTO tracks(rel_path, size, mtime, artist, album, title, track_no,
                                duration_ms, raw_genre, rg_track_gain, year, present)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1)
             ON CONFLICT(rel_path) DO UPDATE SET
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
        rel_path: &str,
        size: u64,
        mtime: i64,
        failure: &Failure,
    ) -> Result<TrackId> {
        let retry = failure.code.is_retried();
        let known: Option<TrackId> = if retry {
            self.conn
                .prepare_cached(
                    "UPDATE tracks SET present=1, needs_retag=1 WHERE rel_path=?1 RETURNING id",
                )?
                .query_row([rel_path], |r| r.get(0))
                .optional()?
        } else {
            self.conn
                .prepare_cached(
                    "UPDATE tracks SET
                        measured_lufs = CASE WHEN size=?2 AND mtime=?3 THEN measured_lufs END,
                        size=?2, mtime=?3, present=1, needs_retag=0
                     WHERE rel_path=?1 RETURNING id",
                )?
                .query_row(params![rel_path, size as i64, mtime], |r| r.get(0))
                .optional()?
        };
        let id = match known {
            Some(id) => id,
            None => {
                let id = self.upsert_track(rel_path, size, mtime, &TrackMeta::default())?;
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

    /// Lets every track play again; stored failures stay listed (see [`Self::unreadable`]).
    pub fn begin_scan(&self) -> Result<()> {
        self.conn
            .execute("UPDATE tracks SET present=0, fail_count=0", [])?;
        Ok(())
    }

    pub fn mark_seen(&self, rel_path: &str) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE tracks SET present=1 WHERE rel_path=?1")?
            .execute([rel_path])?;
        Ok(())
    }

    /// Delete tracks not seen since `begin_scan`; returns how many were removed.
    pub fn finish_scan(&self) -> Result<usize> {
        self.conn.execute(
            "DELETE FROM track_genres WHERE track_id IN (SELECT id FROM tracks WHERE present=0)",
            [],
        )?;
        self.conn
            .execute("DELETE FROM selection_state WHERE last_first IN (SELECT id FROM tracks WHERE present=0)", [])?;
        Ok(self
            .conn
            .execute("DELETE FROM tracks WHERE present=0", [])?)
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

    /// Genres with the number of playable (not banned, not failed) tracks in each.
    pub fn genres(&self) -> Result<Vec<GenreCount>> {
        let mut st = self.conn.prepare_cached(
            "SELECT g.key, g.display, COUNT(t.id)
             FROM genres g
             JOIN track_genres tg ON tg.genre_id = g.id
             JOIN tracks t ON t.id = tg.track_id AND t.banned = 0 AND t.fail_count < ?1
             GROUP BY g.id
             ORDER BY g.display COLLATE NOCASE, g.key",
        )?;
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
            years: None,
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
            years: None,
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
                        last_error_server
                 FROM tracks
                 WHERE {UNREADABLE}
                 ORDER BY last_error_at IS NULL, last_error_at DESC, rel_path"
            ))?
            .query_map([], |r| {
                let fail_count: i64 = r.get(4)?;
                Ok(UnreadableTrack {
                    id: r.get(0)?,
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
                 DELETE FROM selection_state; DELETE FROM tracks;",
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

    pub fn banned(&self) -> Result<Vec<TrackRow>> {
        let sql = format!("SELECT {TRACK_COLS} FROM tracks WHERE banned=1 ORDER BY rel_path");
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
            .prepare(
                "SELECT DISTINCT artist FROM tracks WHERE artist IS NOT NULL AND artist<>''
                 ORDER BY artist COLLATE NOCASE, artist",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }

    /// The artist's albums by name, each with its earliest year.
    pub fn albums(&self, artist: &str) -> Result<Vec<AlbumInfo>> {
        let v = self
            .conn
            .prepare(
                "SELECT album, MIN(year) FROM tracks
                 WHERE artist=?1 AND album IS NOT NULL AND album<>''
                 GROUP BY album ORDER BY album COLLATE NOCASE, album",
            )?
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
            .prepare(
                "SELECT id FROM tracks WHERE artist=?1 AND album=?2
                 ORDER BY track_no IS NULL, track_no, rel_path",
            )?
            .query_map([artist, album], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::{FailCode, Failure};
    use crate::source::Which;

    fn meta(genre: Option<&str>) -> TrackMeta {
        TrackMeta {
            raw_genre: genre.map(str::to_string),
            ..Default::default()
        }
    }

    fn add(c: &Catalog, path: &str, genre: Option<&str>) -> TrackId {
        c.upsert_track(path, 10, 100, &meta(genre)).unwrap()
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
        let id = c.upsert_track("a/x.mp3", 10, 100, &meta(None)).unwrap();
        assert!(!c.is_empty().unwrap());
        assert!(c.file_unchanged("a/x.mp3", 10, 100).unwrap());
        assert!(!c.file_unchanged("a/x.mp3", 10, 101).unwrap());
        assert!(!c.file_unchanged("a/x.mp3", 11, 100).unwrap());
        assert!(!c.file_unchanged("nope.mp3", 10, 100).unwrap());
        let id2 = c
            .upsert_track("a/x.mp3", 10, 101, &meta(Some("Rock")))
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
        c.begin_scan().unwrap();
        c.mark_seen("a.mp3").unwrap();
        assert_eq!(c.finish_scan().unwrap(), 0);
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
            assert!(!c.file_unchanged(path, size, mtime).unwrap(), "{path}");
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
            c.upsert_track("Rock/a.mp3", 10, 100, &meta(Some("Rock")))
                .unwrap();
            assert!(c.file_unchanged("Rock/a.mp3", 10, 100).unwrap());
        }
        let c = Catalog::open(&db).unwrap();
        assert_eq!(user_version(&c), CATALOG_VERSION);
        assert!(
            c.file_unchanged("Rock/a.mp3", 10, 100).unwrap(),
            "not marked again"
        );
        assert!(!c.file_unchanged("Rock/b.mp3", 20, 200).unwrap());
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
        assert!(!c.file_unchanged("d1/t1.mp3", 1, 1).unwrap());
        assert!(took < Duration::from_secs(3), "migration took {took:?}");
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
        c.begin_scan().unwrap();
        for p in ["a.mp3", "b.mp3", "c.mp3", "ok.mp3"] {
            c.mark_seen(p).unwrap();
        }
        c.finish_scan().unwrap();
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
        let id = c.upsert_track("known.mp3", 10, 100, &m).unwrap();
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
                .record_scan_failure("known.mp3", 10, 100, &failure(code, "x"))
                .unwrap();
            assert_eq!(got, id);
            let row = c.track(id).unwrap();
            assert_eq!(row.meta.artist.as_deref(), Some("Artist"), "{code:?}");
            assert_eq!(row.meta.year, Some(1999), "{code:?}");
            assert_eq!(row.measured_lufs, Some(-11.0), "{code:?}");
            assert_eq!(size_mtime(&c, id), (10, 100), "never zeroed: {code:?}");
            assert_eq!(
                c.file_unchanged("known.mp3", 10, 100).unwrap(),
                !code.is_retried(),
                "retried unless the file is broken: {code:?}"
            );
            // A later successful read keeps the loudness of the unchanged file.
            c.upsert_track("known.mp3", 10, 100, &row.meta).unwrap();
            assert_eq!(c.track(id).unwrap().measured_lufs, Some(-11.0), "{code:?}");
            assert!(c.file_unchanged("known.mp3", 10, 100).unwrap());
        }
    }

    #[test]
    fn a_failed_read_of_a_changed_file_keeps_the_old_data_until_it_reads() {
        let c = Catalog::open_in_memory().unwrap();
        let id = retag_fixture(&c);
        c.record_scan_failure("known.mp3", 20, 200, &failure(FailCode::Timeout, "t"))
            .unwrap();
        assert_eq!(size_mtime(&c, id), (10, 100));
        assert_eq!(c.track(id).unwrap().meta.artist.as_deref(), Some("Artist"));
        assert!(!c.file_unchanged("known.mp3", 20, 200).unwrap());
        // Read at last: the file changed, so its old loudness goes.
        c.upsert_track("known.mp3", 20, 200, &meta(None)).unwrap();
        assert_eq!(c.track(id).unwrap().measured_lufs, None);

        // Changed into a broken file: new size and time, old tags, no loudness.
        let id = retag_fixture(&c);
        c.record_scan_failure("known.mp3", 30, 300, &failure(FailCode::Tag, "bad"))
            .unwrap();
        assert_eq!(size_mtime(&c, id), (30, 300));
        let row = c.track(id).unwrap();
        assert_eq!(row.meta.artist.as_deref(), Some("Artist"));
        assert_eq!(row.measured_lufs, None);
        assert!(
            c.file_unchanged("known.mp3", 30, 300).unwrap(),
            "waits for a change"
        );
    }

    #[test]
    fn scan_failures_of_new_files_are_stored_and_retried() {
        let c = Catalog::open_in_memory().unwrap();
        let broken = c
            .record_scan_failure("broken.mp3", 5, 50, &failure(FailCode::Tag, "bad"))
            .unwrap();
        let slow = c
            .record_scan_failure("slow.mp3", 6, 60, &failure(FailCode::Timeout, "t"))
            .unwrap();
        assert_eq!(size_mtime(&c, broken), (5, 50));
        assert_eq!(size_mtime(&c, slow), (6, 60));
        assert!(
            c.file_unchanged("broken.mp3", 5, 50).unwrap(),
            "until it changes"
        );
        assert!(!c.file_unchanged("slow.mp3", 6, 60).unwrap(), "retried");
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
        c.upsert_track("broken.mp3", 5, 50, &meta(None)).unwrap();
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
            c.record_scan_failure("a.mp3", 10, 100, &failure(code, "x"))
                .unwrap();
            c.begin_scan().unwrap();
            assert_eq!(c.unreadable().unwrap().len(), 1, "{code:?}");
            c.upsert_track("a.mp3", 10, 100, &meta(None)).unwrap();
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
        c.begin_scan().unwrap();
        c.upsert_track("a.mp3", 10, 100, &meta(None)).unwrap();
        assert_eq!(c.unreadable().unwrap().len(), 1);
        c.begin_scan().unwrap();
        c.upsert_track("a.mp3", 11, 100, &meta(None)).unwrap();
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
            c.upsert_track(path, 1, 1, &m).unwrap()
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
    fn albums_come_with_their_year() {
        let c = Catalog::open_in_memory().unwrap();
        let m = |album: &str, year: Option<u16>| TrackMeta {
            artist: Some("A".into()),
            album: Some(album.into()),
            year,
            ..Default::default()
        };
        c.upsert_track("1.mp3", 1, 1, &m("X", Some(1994))).unwrap();
        c.upsert_track("2.mp3", 1, 1, &m("X", Some(1993))).unwrap();
        c.upsert_track("3.mp3", 1, 1, &m("Y", None)).unwrap();
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
        c.begin_scan().unwrap();
        c.mark_seen("b.mp3").unwrap();
        assert_eq!(c.finish_scan().unwrap(), 1);
        assert!(c.track(a).is_err());
        c.rebuild_genres().unwrap();
        assert_eq!(count_of(&c, "rock"), Some(1));
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
        let t2 = c.upsert_track("b/2.mp3", 1, 1, &m("A", "X", 2)).unwrap();
        let t1 = c.upsert_track("b/1.mp3", 1, 1, &m("A", "X", 1)).unwrap();
        c.upsert_track("b/3.mp3", 1, 1, &m("B", "Y", 1)).unwrap();
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
                c.upsert_track(&format!("g{}/t{}.mp3", i % 40, i), 1, 1, &m)?;
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
}
