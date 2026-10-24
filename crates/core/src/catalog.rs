//! SQLite catalog of tracks, genres and playback state.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::genre::{genre_key, pick_display, split_genres};
use crate::{Result, TrackId};

const NONE_KEY: &str = "_none";
const NONE_DISPLAY: &str = "(bez žanra)";
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRow {
    pub id: TrackId,
    pub rel_path: String,
    pub meta: TrackMeta,
    pub measured_lufs: Option<f32>,
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

const TRACK_COLS: &str = "id, rel_path, artist, album, title, track_no, duration_ms, \
                          raw_genre, rg_track_gain, measured_lufs";

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
        Ok(Self { conn })
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

    pub fn file_unchanged(&self, rel_path: &str, size: u64, mtime: i64) -> Result<bool> {
        let hit = self
            .conn
            .prepare_cached("SELECT 1 FROM tracks WHERE rel_path=?1 AND size=?2 AND mtime=?3")?
            .exists(params![rel_path, size as i64, mtime])?;
        Ok(hit)
    }

    /// Insert or update a track by path; marks it present. A changed file loses its
    /// measured loudness; failure count is left to `begin_scan`/`record_success`.
    pub fn upsert_track(
        &self,
        rel_path: &str,
        size: u64,
        mtime: i64,
        meta: &TrackMeta,
    ) -> Result<TrackId> {
        let id = self
            .conn
            .prepare_cached(
                "INSERT INTO tracks(rel_path, size, mtime, artist, album, title, track_no,
                                    duration_ms, raw_genre, rg_track_gain, present)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1)
                 ON CONFLICT(rel_path) DO UPDATE SET
                    measured_lufs = CASE WHEN size=excluded.size AND mtime=excluded.mtime
                                         THEN measured_lufs ELSE NULL END,
                    size=excluded.size, mtime=excluded.mtime,
                    artist=excluded.artist, album=excluded.album, title=excluded.title,
                    track_no=excluded.track_no, duration_ms=excluded.duration_ms,
                    raw_genre=excluded.raw_genre, rg_track_gain=excluded.rg_track_gain,
                    present=1
                 RETURNING id",
            )?
            .query_row(
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
                ],
                |r| r.get(0),
            )?;
        Ok(id)
    }

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
    pub fn rebuild_genres(&self) -> Result<()> {
        let rules: HashMap<String, String> = self
            .conn
            .prepare("SELECT from_key, to_display FROM genre_rules")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // Resolve a raw name to (key, forced display) applying merge/rename rules.
        let resolve = |name: &str| -> (String, Option<String>) {
            let key = genre_key(name);
            match rules.get(&key) {
                Some(to) => (genre_key(to), Some(to.clone())),
                None => (key, None),
            }
        };

        let tracks: Vec<(TrackId, String, Option<String>)> = self
            .conn
            .prepare("SELECT id, rel_path, raw_genre FROM tracks")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut spellings: HashMap<String, HashMap<String, usize>> = HashMap::new();
        let mut forced: HashMap<String, String> = HashMap::new();
        let mut assigned: Vec<(TrackId, Vec<String>)> = Vec::with_capacity(tracks.len());
        let mut untagged: Vec<(TrackId, &str)> = Vec::new();
        for (id, rel_path, raw) in &tracks {
            let mut keys: Vec<String> = Vec::new();
            for name in raw.as_deref().map(split_genres).unwrap_or_default() {
                let (key, disp) = resolve(&name);
                if key.is_empty() {
                    continue;
                }
                if let Some(d) = disp {
                    forced.insert(key.clone(), d);
                } else {
                    *spellings
                        .entry(key.clone())
                        .or_default()
                        .entry(name)
                        .or_default() += 1;
                }
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            if keys.is_empty() {
                untagged.push((*id, rel_path));
            } else {
                assigned.push((*id, keys));
            }
        }

        // Folder fallback: first path component matching an existing tag-derived key.
        let known: HashSet<String> = assigned
            .iter()
            .flat_map(|(_, k)| k.iter().cloned())
            .collect();
        for (id, rel_path) in untagged {
            let folder = rel_path.split_once('/').map(|(f, _)| resolve(f).0);
            let key = match folder {
                Some(k) if known.contains(&k) => k,
                _ => NONE_KEY.to_string(),
            };
            assigned.push((id, vec![key]));
        }

        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM track_genres", [])?;
        tx.execute("DELETE FROM genres", [])?;
        let mut genre_ids: HashMap<String, i64> = HashMap::new();
        {
            let mut ins_genre = tx.prepare("INSERT INTO genres(key, display) VALUES(?1, ?2)")?;
            let mut all_keys: HashSet<&String> = known.iter().collect();
            let none_key = NONE_KEY.to_string();
            if assigned.iter().any(|(_, k)| k[0] == NONE_KEY) {
                all_keys.insert(&none_key);
            }
            for key in all_keys {
                let display = if key == NONE_KEY {
                    NONE_DISPLAY.to_string()
                } else if let Some(d) = forced.get(key) {
                    d.clone()
                } else {
                    let sp: Vec<(String, usize)> = spellings[key]
                        .iter()
                        .map(|(s, n)| (s.clone(), *n))
                        .collect();
                    pick_display(&sp)
                };
                ins_genre.execute(params![key, display])?;
                genre_ids.insert(key.clone(), tx.last_insert_rowid());
            }
            let mut ins_link = tx
                .prepare("INSERT OR IGNORE INTO track_genres(track_id, genre_id) VALUES(?1, ?2)")?;
            for (id, keys) in &assigned {
                for key in keys {
                    ins_link.execute(params![id, genre_ids[key]])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
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
        if genre_keys.is_empty() {
            let ids = self
                .conn
                .prepare_cached("SELECT id FROM tracks WHERE banned=0 AND fail_count<?1")?
                .query_map([MAX_FAILS], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            return Ok(ids);
        }
        let marks = vec!["?"; genre_keys.len()].join(",");
        let sql = format!(
            "SELECT DISTINCT t.id FROM tracks t
             JOIN track_genres tg ON tg.track_id = t.id
             JOIN genres g ON g.id = tg.genre_id
             WHERE g.key IN ({marks}) AND t.banned=0 AND t.fail_count<{MAX_FAILS}"
        );
        let ids = self
            .conn
            .prepare(&sql)?
            .query_map(params_from_iter(genre_keys), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
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

    pub fn record_failure(&self, id: TrackId) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "UPDATE tracks SET fail_count = fail_count + 1 WHERE id=?1 RETURNING fail_count",
            [id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn record_success(&self, id: TrackId) -> Result<()> {
        self.conn
            .execute("UPDATE tracks SET fail_count=0 WHERE id=?1", [id])?;
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
        self.conn.execute(
            "INSERT OR REPLACE INTO selection_state(selection_key, last_first) VALUES(?1, ?2)",
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

    pub fn albums(&self, artist: &str) -> Result<Vec<String>> {
        let v = self
            .conn
            .prepare(
                "SELECT DISTINCT album FROM tracks WHERE artist=?1 AND album IS NOT NULL AND album<>''
                 ORDER BY album COLLATE NOCASE, album",
            )?
            .query_map([artist], |r| r.get(0))?
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
        assert_eq!(g.display, "(bez žanra)");
    }

    #[test]
    fn three_failures_exclude_until_rescan() {
        let c = Catalog::open_in_memory().unwrap();
        let id = add(&c, "a.mp3", Some("Rock"));
        c.rebuild_genres().unwrap();
        assert_eq!(c.record_failure(id).unwrap(), 1);
        assert_eq!(c.record_failure(id).unwrap(), 2);
        assert!(c.playable_tracks(&[]).unwrap().contains(&id));
        assert_eq!(c.record_failure(id).unwrap(), 3);
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
        c.record_failure(id).unwrap();
        c.record_failure(id).unwrap();
        c.record_success(id).unwrap();
        assert_eq!(c.record_failure(id).unwrap(), 1);
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
    fn genre_rule_merges_and_renames() {
        let c = Catalog::open_in_memory().unwrap();
        add(&c, "a.mp3", Some("Trance; Chillout"));
        add(&c, "b.mp3", Some("Chill Out"));
        add(&c, "c.mp3", Some("Chillout"));
        c.rebuild_genres().unwrap();
        c.add_genre_rule("chillout", "Chill").unwrap();
        let g = c.genres().unwrap();
        let chill: Vec<_> = g.iter().filter(|g| g.display == "Chill").collect();
        assert_eq!(chill.len(), 1);
        assert_eq!(chill[0].key, "chill");
        assert_eq!(chill[0].count, 3);
        assert!(g.iter().all(|g| g.key != "chillout"));
        // survives a later rebuild
        c.rebuild_genres().unwrap();
        assert_eq!(count_of(&c, "chill"), Some(3));
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
        assert_eq!(c.albums("A").unwrap(), vec!["X"]);
        assert_eq!(c.album_tracks("A", "X").unwrap(), vec![t1, t2]);
    }

    #[test]
    fn genres_query_fast_on_50k_tracks() {
        use std::time::{Duration, Instant};
        let mut c = Catalog::open_in_memory().unwrap();
        c.transaction(|c| {
            for i in 0..50_000 {
                let m = meta(Some(&format!("Genre{}", i % 40)));
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
        assert_eq!(c.track_genres(a).unwrap(), vec!["Chill Out", "Trance"]);
        assert_eq!(c.track_genres(none).unwrap(), vec!["(bez žanra)"]);
        assert!(c.track_genres(999).unwrap().is_empty());
    }
}
