//! The playlist: the play queue of the current selection (genres, an album, a
//! filtered list or a hand-made one), shared by the commands and the provider the
//! player pulls tracks from. Also its pages, filters, export and saving.
//!
//! Saved in the catalog: the order as one blob (rewritten only when it changes)
//! and the cursor, history and position (small, written every few seconds while
//! playing). See `Catalog::save_playlist_order`.

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use smp_core::catalog::{
    selection_key, Catalog, PlaylistCursor, SavedPlaylist, TrackFilter, TrackRow, YearCount,
};
use smp_core::genre::UNKNOWN_KEY;
use smp_core::m3u::{M3uTrack, M3uWriter};
use smp_core::queue::{Entry, Queue, QueueState};
use smp_core::shuffle::entropy_rng;
use smp_core::TrackId;

use crate::i18n;
use crate::state::lock;

pub type SharedPlaylist = Arc<Mutex<Playlist>>;

/// Rows per page at most.
pub const MAX_PAGE: usize = 500;
/// Past this point into a track, "Previous" starts it again instead.
pub const RESTART_AFTER_MS: u64 = 3_000;
/// Network failures of the track to resume before the playlist moves on (the share
/// answers, but that track does not).
pub const RESUME_RETRIES: u32 = 3;
/// Ids looked up per catalog query when exporting.
const EXPORT_BATCH: usize = 500;

/// What the playlist was made from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    /// A shuffle of these genres (none = everything).
    Genres {
        keys: Vec<String>,
    },
    Album {
        artist: String,
        album: String,
    },
    /// Made by hand ("New empty playlist").
    Manual,
    /// A shuffle of what a filter left of another playlist.
    Filtered {
        base: Box<Selection>,
        filter: QueueFilter,
    },
}

/// Narrows what the playlist shows (or exports); never changes the play order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueFilter {
    /// In the artist or title.
    pub text: String,
    /// In any of these genres; none = any genre.
    pub genres: Vec<String>,
    /// `[from, to]`, inclusive; none = any year.
    pub years: Option<(u16, u16)>,
    /// Also tracks without a year; with no `years`, only those.
    pub unknown_year: bool,
}

impl QueueFilter {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.genres.is_empty()
            && self.years.is_none()
            && !self.unknown_year
    }

    fn track_filter(&self) -> TrackFilter {
        TrackFilter {
            genre_keys: self.genres.clone(),
            years: self.years,
            unknown_year: self.unknown_year,
            text: Some(self.text.clone()),
        }
    }
}

pub struct Playlist {
    pub queue: Queue<StdRng>,
    pub selection: Selection,
    /// Bumped when the playlist is replaced.
    generation: u64,
    /// `(generation, order version)` last saved.
    saved_order: Option<(u64, u64)>,
    /// The track to resume and where (0 = its start); kept through a few network
    /// failures (see [`RESUME_RETRIES`]).
    pub resume_at: Option<(TrackId, u64)>,
    /// Network failures of that track so far.
    pub resume_failures: u32,
    /// The first track of a genre shuffle is recorded (to avoid it next time).
    pub first_recorded: bool,
    /// Played to its end: nothing to resume.
    pub ended: bool,
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            queue: Queue::fixed(&[], entropy_rng()),
            selection: Selection::Manual,
            generation: 0,
            saved_order: None,
            resume_at: None,
            resume_failures: 0,
            first_recorded: false,
            ended: false,
        }
    }
}

impl Playlist {
    /// Makes this the playlist of `selection`.
    pub fn replace(&mut self, selection: Selection, queue: Queue<StdRng>) {
        self.generation += 1;
        *self = Self {
            queue,
            selection,
            generation: self.generation,
            saved_order: None,
            ..Self::default()
        };
    }

    /// Changes whenever the playlist is replaced or its order changes.
    pub fn version(&self) -> (u64, u64) {
        (self.generation, self.queue.version())
    }
}

/// A fresh shuffle of the playable tracks in `keys` (empty = everything), not
/// starting with the first track of the last run of the same selection.
pub fn genres_queue(query: &Catalog, keys: &[String]) -> smp_core::Result<Queue<StdRng>> {
    let pool = query.playable_tracks(keys)?;
    let avoid = query.last_first(&selection_key(keys))?;
    Ok(Queue::shuffled(&pool, avoid, entropy_rng()))
}

/// An album in track order, without banned tracks.
pub fn album_queue(query: &Catalog, artist: &str, album: &str) -> smp_core::Result<Queue<StdRng>> {
    let banned: HashSet<TrackId> = query.banned()?.into_iter().map(|t| t.id).collect();
    let tracks: Vec<TrackId> = query
        .album_tracks(artist, album)?
        .into_iter()
        .filter(|id| !banned.contains(id))
        .collect();
    Ok(Queue::fixed(&tracks, entropy_rng()))
}

/// The playable tracks of `filter`, as a set; `None` for an empty filter (all).
fn matching(
    query: &Catalog,
    filter: Option<&QueueFilter>,
) -> smp_core::Result<Option<HashSet<TrackId>>> {
    match filter.filter(|f| !f.is_empty()) {
        None => Ok(None),
        Some(f) => Ok(Some(
            query
                .filtered_tracks(&f.track_filter())?
                .into_iter()
                .collect(),
        )),
    }
}

/// A shuffle of the playlist's tracks that `filter` leaves (each once), as the
/// new selection.
pub fn filtered_queue(
    query: &Catalog,
    list: &SharedPlaylist,
    filter: &QueueFilter,
) -> smp_core::Result<(Selection, Queue<StdRng>)> {
    let (order, base, current) = {
        let l = lock(list);
        (
            l.queue.order().to_vec(),
            l.selection.clone(),
            l.queue.current(),
        )
    };
    let keep = matching(query, Some(filter))?;
    let mut seen = HashSet::new();
    let pool: Vec<TrackId> = order
        .iter()
        .map(|e| e.id)
        .filter(|id| keep.as_ref().is_none_or(|k| k.contains(id)) && seen.insert(*id))
        .collect();
    let base = match base {
        // Filtering a filtered list narrows the same base.
        Selection::Filtered { base, .. } => base,
        other => Box::new(other),
    };
    let selection = Selection::Filtered {
        base,
        filter: filter.clone(),
    };
    Ok((selection, Queue::shuffled(&pool, current, entropy_rng())))
}

/// The selection's name in `lang`: "Trance + Chillout", "All", "Artist – Album",
/// "My playlist", "… (filtered)".
pub fn title(query: &Catalog, lang: &str, selection: &Selection) -> String {
    match selection {
        Selection::Genres { keys } if keys.is_empty() => i18n::tr(lang, "playlist.all"),
        Selection::Genres { keys } => {
            let names: HashMap<String, String> = query
                .genres()
                .unwrap_or_default()
                .into_iter()
                .map(|g| (g.key, g.display))
                .collect();
            keys.iter()
                .map(|k| {
                    if k == UNKNOWN_KEY {
                        i18n::tr(lang, "genres.unknown")
                    } else {
                        names.get(k).cloned().unwrap_or_else(|| k.clone())
                    }
                })
                .collect::<Vec<_>>()
                .join(" + ")
        }
        Selection::Album { artist, album } if artist.is_empty() => album.clone(),
        Selection::Album { artist, album } => format!("{artist} – {album}"),
        Selection::Manual => i18n::tr(lang, "playlist.manual"),
        Selection::Filtered { base, .. } => i18n::trf(
            lang,
            "playlist.filtered",
            &[("name", &title(query, lang, base))],
        ),
    }
}

/// A track's title, or its file name without the extension.
pub fn track_title(row: &TrackRow) -> String {
    row.meta
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            let name = row.rel_path.rsplit('/').next().unwrap_or(&row.rel_path);
            name.rsplit_once('.')
                .map_or(name, |(stem, _)| stem)
                .to_string()
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    /// Earlier in this pass (played or jumped over).
    Played,
    Current,
    Upcoming,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistRow {
    /// Position in the whole playlist (for jump, remove, move).
    pub index: usize,
    pub id: TrackId,
    pub artist: String,
    pub title: String,
    pub year: Option<u16>,
    pub duration_ms: Option<u64>,
    /// Added by hand.
    pub manual: bool,
    pub state: RowState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistPage {
    /// The selection's name.
    pub title: String,
    pub selection: Selection,
    pub shuffle: bool,
    /// Entries in the whole playlist.
    pub count: usize,
    /// Entries the filter leaves (= `count` without a filter).
    pub total: usize,
    pub offset: usize,
    pub rows: Vec<PlaylistRow>,
    /// Index of the current track in the playlist, if it is the one at the cursor.
    pub current_index: Option<usize>,
    /// Its position among the filtered entries (the row number to scroll to).
    pub current_row: Option<usize>,
    /// The playing (or last played) track, also when it is not in the list.
    pub current_id: Option<TrackId>,
    /// Changes when the playlist is replaced or its order changes.
    pub version: (u64, u64),
}

/// Rows `offset..offset + limit` of the playlist (of what `filter` leaves of it).
pub fn page(
    list: &SharedPlaylist,
    query: &Mutex<Catalog>,
    lang: &str,
    offset: usize,
    limit: usize,
    filter: Option<&QueueFilter>,
) -> smp_core::Result<PlaylistPage> {
    let limit = limit.min(MAX_PAGE);
    let filtered = filter.is_some_and(|f| !f.is_empty());
    let (entries, count, cursor, current_index, current_id, selection, shuffle, version) = {
        let l = lock(list);
        let order = l.queue.order();
        let entries: Vec<(usize, Entry)> = if filtered {
            order.iter().copied().enumerate().collect()
        } else {
            let end = offset.saturating_add(limit).min(order.len());
            (offset.min(end)..end).map(|i| (i, order[i])).collect()
        };
        (
            entries,
            order.len(),
            l.queue.cursor(),
            l.queue.current_index(),
            l.queue.current(),
            l.selection.clone(),
            l.queue.is_shuffle(),
            l.version(),
        )
    };
    let q = lock(query);
    let (picked, total, current_row) = match matching(&q, filter)? {
        Some(keep) => {
            let hits: Vec<(usize, Entry)> = entries
                .into_iter()
                .filter(|(_, e)| keep.contains(&e.id))
                .collect();
            let current_row = current_index.and_then(|c| hits.iter().position(|(i, _)| *i == c));
            let total = hits.len();
            let picked = hits.into_iter().skip(offset).take(limit).collect();
            (picked, total, current_row)
        }
        None => (entries, count, current_index),
    };
    let ids: Vec<TrackId> = picked.iter().map(|(_, e)| e.id).collect();
    let rows: HashMap<TrackId, TrackRow> = q
        .tracks_by_ids(&ids)?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    let rows = picked
        .into_iter()
        .filter_map(|(index, e)| {
            let row = rows.get(&e.id)?;
            let state = match cursor {
                Some(c) if index == c && current_index == Some(c) => RowState::Current,
                Some(c) if index <= c => RowState::Played,
                _ => RowState::Upcoming,
            };
            Some(PlaylistRow {
                index,
                id: e.id,
                artist: row.meta.artist.clone().unwrap_or_default(),
                title: track_title(row),
                year: row.meta.year,
                duration_ms: row.meta.duration_ms,
                manual: e.manual,
                state,
            })
        })
        .collect();
    Ok(PlaylistPage {
        title: title(&q, lang, &selection),
        selection,
        shuffle,
        count,
        total,
        offset,
        rows,
        current_index,
        current_row,
        current_id,
        version,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FacetGenre {
    pub key: String,
    pub display: String,
    pub count: usize,
}

/// The choices for the playlist's genre and year filters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistFacets {
    pub genres: Vec<FacetGenre>,
    /// Oldest first.
    pub years: Vec<YearCountPayload>,
    /// Tracks without a year.
    pub unknown_year: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct YearCountPayload {
    pub year: u16,
    pub count: usize,
}

impl From<YearCount> for YearCountPayload {
    fn from(y: YearCount) -> Self {
        Self {
            year: y.year,
            count: y.count,
        }
    }
}

pub fn facets(
    list: &SharedPlaylist,
    query: &Mutex<Catalog>,
    lang: &str,
) -> smp_core::Result<PlaylistFacets> {
    let ids: Vec<TrackId> = lock(list).queue.order().iter().map(|e| e.id).collect();
    let f = lock(query).facets(&ids)?;
    Ok(PlaylistFacets {
        genres: f
            .genres
            .into_iter()
            .map(|g| FacetGenre {
                display: if g.key == UNKNOWN_KEY {
                    i18n::tr(lang, "genres.unknown")
                } else {
                    g.display
                },
                key: g.key,
                count: g.count,
            })
            .collect(),
        years: f.years.into_iter().map(Into::into).collect(),
        unknown_year: f.unknown_year,
    })
}

/// Writes the playlist (what `filter` leaves of it) to `path` as `.m3u8`, a batch
/// of tracks at a time; returns how many tracks it holds. Written to a temporary
/// file first, so a failed export leaves an existing file as it was.
pub fn export_m3u8(
    list: &SharedPlaylist,
    query: &Mutex<Catalog>,
    path: &Path,
    filter: Option<&QueueFilter>,
) -> Result<usize, String> {
    let ids: Vec<TrackId> = lock(list).queue.order().iter().map(|e| e.id).collect();
    let keep = matching(&lock(query), filter).map_err(|e| e.to_string())?;
    let ids: Vec<TrackId> = match keep {
        Some(k) => ids.into_iter().filter(|id| k.contains(id)).collect(),
        None => ids,
    };
    let name = path
        .file_name()
        .ok_or_else(|| crate::i18n::t("err.no_file_name"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.part"));
    let written = (|| {
        let file = std::fs::File::create(&tmp)?;
        let mut w = M3uWriter::new(BufWriter::new(file))?;
        for chunk in ids.chunks(EXPORT_BATCH) {
            let rows = lock(query)
                .tracks_by_ids(chunk)
                .map_err(std::io::Error::other)?;
            for r in &rows {
                w.track(&M3uTrack {
                    rel_path: &r.rel_path,
                    artist: r.meta.artist.as_deref(),
                    title: r.meta.title.as_deref(),
                    duration_ms: r.meta.duration_ms,
                })?;
            }
        }
        let n = w.written();
        w.finish()?
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok::<usize, std::io::Error>(n)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(|e| e.to_string())
}

/// A file name for `title` (characters no file system takes become `-`).
pub fn file_stem(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let s = s.trim().trim_start_matches('.').trim();
    if s.is_empty() {
        "Playlist".into()
    } else {
        s.to_string()
    }
}

/// What "Previous" does at `position_ms` into the current track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreviousAction {
    /// Start the current track again.
    Restart,
    /// Play the track before it.
    Back,
}

/// More than 3 s into the track (or nothing to go back to): start it again;
/// otherwise go back.
pub fn previous_action(position_ms: u64, can_go_back: bool) -> PreviousAction {
    if position_ms > RESTART_AFTER_MS || !can_go_back {
        PreviousAction::Restart
    } else {
        PreviousAction::Back
    }
}

/// A playlist save: the order when it changed since the last save, and the cursor.
pub struct SaveJob {
    list: SharedPlaylist,
    /// The playlist version the order is of.
    version: (u64, u64),
    order: Option<(String, bool, Vec<Entry>)>,
    cursor: PlaylistCursor,
}

impl SaveJob {
    /// Writes the order (if any) and the cursor together; the order counts as
    /// saved only once this succeeded.
    pub fn run(self, c: &Catalog) -> smp_core::Result<()> {
        let order = self
            .order
            .as_ref()
            .map(|(selection, shuffle, order)| (selection.as_str(), *shuffle, order.as_slice()));
        c.save_playlist(order, &self.cursor)?;
        if self.order.is_some() {
            lock(&self.list).saved_order = Some(self.version);
        }
        Ok(())
    }
}

/// What to save of `list`; `position` is the last reported position (track, ms).
pub fn save_job(list: &SharedPlaylist, position: Option<(TrackId, u64)>) -> SaveJob {
    let l = lock(list);
    let version = l.version();
    let order = (l.saved_order != Some(version)).then(|| {
        let selection = serde_json::to_string(&l.selection).unwrap_or_else(|_| "{}".into());
        (selection, l.queue.is_shuffle(), l.queue.order().to_vec())
    });
    let state = l.queue.state();
    let current = l.queue.current();
    let position_ms = match (position, l.resume_at) {
        (Some((id, ms)), _) if Some(id) == current => ms,
        // Restored but not played yet: where it was.
        (_, Some((id, ms))) if Some(id) == current => ms,
        _ => 0,
    };
    SaveJob {
        list: list.clone(),
        version,
        order,
        cursor: PlaylistCursor {
            cursor: state.cursor,
            history: state.history,
            history_pos: state.history_pos,
            position_ms,
            resume: !l.ended && current.is_some(),
        },
    }
}

/// The saved playlist as it was, without tracks that are gone or banned meanwhile,
/// or left out of the shuffle (unless added by hand). Its current track is due again at its position, unless the playlist
/// had ended or that track is gone (then the next one is due, from its start).
pub fn restore(query: &Catalog, saved: SavedPlaylist) -> smp_core::Result<Playlist> {
    let selection = serde_json::from_str(&saved.selection).unwrap_or(Selection::Manual);
    let resume = saved.cursor.resume;
    let state = QueueState {
        shuffle: saved.shuffle,
        order: saved.order,
        cursor: saved.cursor.cursor,
        history: saved.cursor.history,
        history_pos: saved.cursor.history_pos,
    };
    let mut queue = Queue::restore(state, resume, entropy_rng());
    let saved_version = queue.version();
    let mut playable: HashSet<TrackId> = query
        .filtered_tracks(&TrackFilter::default())?
        .into_iter()
        .collect();
    // Added by hand (from the Unreadable list, say): kept unless gone or banned.
    let manual: Vec<TrackId> = queue
        .order()
        .iter()
        .filter(|e| e.manual && !playable.contains(&e.id))
        .map(|e| e.id)
        .collect();
    if !manual.is_empty() {
        let banned: HashSet<TrackId> = query.banned()?.into_iter().map(|t| t.id).collect();
        playable.extend(
            query
                .tracks_by_ids(&manual)?
                .into_iter()
                .map(|t| t.id)
                .filter(|id| !banned.contains(id)),
        );
    }
    queue.forget_where(|id| !playable.contains(&id));
    let current = queue.current().filter(|id| playable.contains(id));
    let mut list = Playlist {
        queue,
        selection,
        ..Playlist::default()
    };
    list.saved_order = Some((list.generation, saved_version));
    list.ended = !resume;
    list.first_recorded = true;
    list.resume_at = current
        .filter(|_| resume)
        .map(|id| (id, saved.cursor.position_ms));
    Ok(list)
}

#[cfg(test)]
mod tests {
    /// Every library reachable, read from its primary server.
    struct AllOnline;
    impl crate::provider::LibraryHealth for AllOnline {
        fn active_server(&self, _: smp_core::LibraryId) -> Option<Which> {
            Some(Which::Primary)
        }
        fn offline(&self, _: smp_core::LibraryId) -> bool {
            false
        }
        fn any_online(&self) -> bool {
            true
        }
    }

    use super::*;
    use crate::provider::{BannedSet, CatalogProvider, Playback};
    use crate::writer::CatalogWriter;
    use smp_core::audio::player::TrackProvider;
    use smp_core::catalog::TrackMeta;
    use smp_core::source::Which;

    struct Fx {
        _dir: tempfile::TempDir,
        query: Mutex<Catalog>,
        ctx: Playback,
        list: SharedPlaylist,
    }

    fn track(c: &Catalog, path: &str, artist: &str, genre: &str, year: Option<u16>) -> TrackId {
        let meta = TrackMeta {
            artist: Some(artist.into()),
            title: Some(format!("Song {path}")),
            raw_genre: Some(genre.into()),
            duration_ms: Some(200_000),
            year,
            ..Default::default()
        };
        c.upsert_track(smp_core::DEFAULT_LIBRARY, path, 1, 1, &meta)
            .unwrap()
    }

    impl Fx {
        /// 30 Trance tracks (years 1990.., every third without a year) and 10 Jazz.
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("catalog.db");
            let query = Catalog::open(&path).unwrap();
            for i in 0..30 {
                let year = (i % 3 != 0).then_some(1990 + i as u16);
                let artist = if i < 5 { "Alpha" } else { "Beta" };
                track(&query, &format!("t/{i:02}.mp3"), artist, "Trance", year);
            }
            for i in 0..10 {
                track(&query, &format!("j/{i}.mp3"), "Gamma", "Jazz", Some(2000));
            }
            query.rebuild_genres().unwrap();
            let ctx = Playback {
                db: Arc::new(Mutex::new(Catalog::open(&path).unwrap())),
                writer: CatalogWriter::spawn(Catalog::open(&path).unwrap()).unwrap(),
                banned: BannedSet::default(),
                libraries: Arc::new(AllOnline),
            };
            Self {
                _dir: dir,
                query: Mutex::new(query),
                ctx,
                list: SharedPlaylist::default(),
            }
        }

        fn play_genres(&self, keys: &[&str]) -> CatalogProvider {
            let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            let q = genres_queue(&lock(&self.query), &keys).unwrap();
            lock(&self.list).replace(Selection::Genres { keys }, q);
            self.provider()
        }

        fn provider(&self) -> CatalogProvider {
            CatalogProvider::new(self.ctx.clone(), self.list.clone())
        }

        fn page(&self, offset: usize, limit: usize, f: Option<&QueueFilter>) -> PlaylistPage {
            page(&self.list, &self.query, "en", offset, limit, f).unwrap()
        }
    }

    /// As the player does: ask for the next track, then start it.
    fn take(p: &mut CatalogProvider) -> Option<TrackId> {
        let item = p.next_track()?;
        p.on_started(item.id);
        Some(item.id)
    }

    fn row_ids(page: &PlaylistPage) -> Vec<TrackId> {
        page.rows.iter().map(|r| r.id).collect()
    }

    #[test]
    fn pages_follow_the_play_order() {
        let fx = Fx::new();
        let mut p = fx.play_genres(&["trance"]);
        let first = fx.page(0, 10, None);
        assert_eq!(first.count, 30);
        assert_eq!(first.title, "Trance");
        assert!(first.rows.iter().all(|r| r.state == RowState::Upcoming));
        let played: Vec<_> = (0..3).map(|_| take(&mut p).unwrap()).collect();
        assert_eq!(played, row_ids(&first)[..3]);

        let page = fx.page(0, 50, None);
        assert_eq!(page.rows.len(), 30);
        assert_eq!(page.current_index, Some(2));
        assert_eq!(page.current_id, Some(played[2]));
        let states: Vec<RowState> = page.rows[..4].iter().map(|r| r.state).collect();
        use RowState::*;
        assert_eq!(states, [Played, Played, Current, Upcoming]);
        // The next N tracks are the next N rows.
        let upcoming = row_ids(&page)[3..23].to_vec();
        let next: Vec<_> = (0..20).map(|_| take(&mut p).unwrap()).collect();
        assert_eq!(next, upcoming);
        // Pages of a page size.
        let second = fx.page(10, 10, None);
        assert_eq!(second.offset, 10);
        assert_eq!(row_ids(&second), row_ids(&page)[10..20]);
        assert_eq!(second.rows[0].index, 10);
        assert!(fx.page(0, 10_000, None).rows.len() <= MAX_PAGE);
    }

    #[test]
    fn jumping_to_a_row_keeps_the_order() {
        let fx = Fx::new();
        let mut p = fx.play_genres(&[]);
        let before = row_ids(&fx.page(0, 100, None));
        take(&mut p);
        let target = lock(&fx.list).queue.jump(20);
        assert_eq!(target, Some(before[20]));
        assert_eq!(take(&mut p), Some(before[20]));
        assert_eq!(take(&mut p), Some(before[21]));
        let after = fx.page(0, 100, None);
        assert_eq!(row_ids(&after), before, "not reshuffled");
        assert_eq!(after.current_index, Some(21));
        assert_eq!(after.rows[5].state, RowState::Played, "jumped over");
    }

    #[test]
    fn banned_tracks_leave_the_playlist() {
        let fx = Fx::new();
        let mut p = fx.play_genres(&["jazz"]);
        let ids = row_ids(&fx.page(0, 100, None));
        // Banned while queued: skipped when due, and gone from the list.
        fx.ctx.banned.lock().unwrap().insert(ids[0]);
        assert_eq!(take(&mut p), Some(ids[1]));
        assert!(!row_ids(&fx.page(0, 100, None)).contains(&ids[0]));
        // Banned in the catalog before a restore: not restored.
        lock(&fx.query).set_banned(ids[5], true).unwrap();
        let job = save_job(&fx.list, None);
        let c = lock(&fx.query);
        job.run(&c).unwrap();
        let restored = restore(&c, c.load_playlist().unwrap().unwrap()).unwrap();
        assert!(!restored.queue.order().iter().any(|e| e.id == ids[5]));
        assert_eq!(restored.queue.len(), 8);
    }

    #[test]
    fn three_failures_or_a_bad_file_drop_the_track_network_trouble_does_not() {
        use smp_core::failure::{FailCode, Failure};
        let fx = Fx::new();
        let mut p = fx.play_genres(&["jazz"]);
        let ids = row_ids(&fx.page(0, 100, None));
        let item = p.next_track().unwrap();
        p.on_failed(item.id, &Failure::new(FailCode::Timeout, "slow"));
        assert!(row_ids(&fx.page(0, 100, None)).contains(&ids[0]), "stays");
        assert_eq!(take(&mut p), Some(ids[1]), "but is passed over");
        let item = p.next_track().unwrap();
        p.on_failed(item.id, &Failure::new(FailCode::Decode, "bad"));
        assert!(
            !row_ids(&fx.page(0, 100, None)).contains(&ids[2]),
            "dropped"
        );
        assert_eq!(take(&mut p), Some(ids[3]));
    }

    #[test]
    fn filters_narrow_the_rows_and_play_filtered_shuffles_them() {
        let fx = Fx::new();
        let mut p = fx.play_genres(&[]);
        take(&mut p);
        let all = fx.page(0, 100, None);
        let f = QueueFilter {
            text: "alpha".into(),
            ..Default::default()
        };
        let page = fx.page(0, 100, Some(&f));
        assert_eq!(page.total, 5);
        assert_eq!(page.count, 40);
        assert!(page.rows.iter().all(|r| r.artist == "Alpha"));
        // Rows keep the play order and their place in the whole list.
        let indices: Vec<usize> = page.rows.iter().map(|r| r.index).collect();
        assert!(indices.windows(2).all(|w| w[0] < w[1]));
        for r in &page.rows {
            assert_eq!(all.rows[r.index].id, r.id);
        }
        let years = QueueFilter {
            genres: vec!["trance".into()],
            years: Some((1990, 1995)),
            unknown_year: true,
            ..Default::default()
        };
        let page = fx.page(0, 100, Some(&years));
        // 1990..1995 with a year: i = 1, 2, 4, 5; without a year: i = 0, 3, 6, … 27.
        assert_eq!(page.total, 4 + 10);
        let page2 = fx.page(10, 3, Some(&years));
        assert_eq!(page2.rows.len(), 3);
        assert_eq!(page2.total, 14);

        let (sel, q) = filtered_queue(&lock(&fx.query), &fx.list, &f).unwrap();
        assert_eq!(q.len(), 5);
        lock(&fx.list).replace(sel, q);
        let page = fx.page(0, 100, None);
        assert_eq!(page.title, "All (filtered)");
        let mut p = fx.provider();
        let played: Vec<_> = (0..5).map(|_| take(&mut p).unwrap()).collect();
        assert_eq!(played, row_ids(&page));
    }

    #[test]
    fn facets_list_the_genres_and_years_in_the_playlist() {
        let fx = Fx::new();
        fx.play_genres(&["jazz"]);
        let f = facets(&fx.list, &fx.query, "en").unwrap();
        assert_eq!(f.genres.len(), 1);
        assert_eq!((f.genres[0].key.as_str(), f.genres[0].count), ("jazz", 10));
        assert_eq!(
            f.years,
            [YearCountPayload {
                year: 2000,
                count: 10
            }]
        );
        assert_eq!(f.unknown_year, 0);
    }

    #[test]
    fn manual_edits_through_the_provider() {
        let fx = Fx::new();
        lock(&fx.list).replace(Selection::Manual, Queue::fixed(&[], entropy_rng()));
        let mut p = fx.provider();
        assert_eq!(p.next_track(), None);
        lock(&fx.list).queue.append(&[1, 2]);
        lock(&fx.list).queue.insert_next(&[3]);
        let got: Vec<_> = std::iter::from_fn(|| take(&mut p)).collect();
        assert_eq!(got, [3, 1, 2]);
        let page = fx.page(0, 10, None);
        assert_eq!(page.title, "My playlist");
        assert!(page.rows.iter().all(|r| r.manual));
    }

    #[test]
    fn the_playlist_is_saved_and_resumes_where_it_stopped() {
        let fx = Fx::new();
        let mut p = fx.play_genres(&["trance", "jazz"]);
        for _ in 0..4 {
            take(&mut p);
        }
        let current = lock(&fx.list).queue.current().unwrap();
        let order = row_ids(&fx.page(0, 100, None));
        let job = save_job(&fx.list, Some((current, 83_250)));
        fx.ctx.writer.call(move |c| job.run(c)).unwrap();
        // A later save without order changes writes only the cursor.
        let job = save_job(&fx.list, Some((current, 90_000)));
        assert!(job.order.is_none());
        fx.ctx.writer.call(move |c| job.run(c)).unwrap();

        let saved = lock(&fx.query).load_playlist().unwrap().unwrap();
        let list: SharedPlaylist = Arc::new(Mutex::new(restore(&lock(&fx.query), saved).unwrap()));
        {
            let l = lock(&list);
            assert_eq!(
                l.selection,
                Selection::Genres {
                    keys: vec!["trance".into(), "jazz".into()]
                }
            );
            assert_eq!(l.resume_at, Some((current, 90_000)));
            assert!(!l.ended);
        }
        let page = page(&list, &fx.query, "en", 0, 100, None).unwrap();
        assert_eq!(row_ids(&page), order);
        assert_eq!(page.title, "Trance + Jazz");
        let mut p = CatalogProvider::new(fx.ctx.clone(), list.clone());
        let item = p.next_track().unwrap();
        assert_eq!((item.id, item.start_ms), (current, 90_000));
        p.on_started(item.id);
        assert_eq!(lock(&list).resume_at, None);
        assert_eq!(take(&mut p), Some(order[4]), "then the order goes on");
        // Going back still works after the restart.
        assert!(lock(&list).queue.go_back());
    }

    #[test]
    fn a_saved_track_that_is_gone_moves_on_to_the_next() {
        let fx = Fx::new();
        let mut p = fx.play_genres(&["jazz"]);
        take(&mut p);
        let current = take(&mut p).unwrap();
        let order = row_ids(&fx.page(0, 100, None));
        let job = save_job(&fx.list, Some((current, 5_000)));
        fx.ctx.writer.call(move |c| job.run(c)).unwrap();
        lock(&fx.query).set_banned(current, true).unwrap();
        let saved = lock(&fx.query).load_playlist().unwrap().unwrap();
        let list: SharedPlaylist = Arc::new(Mutex::new(restore(&lock(&fx.query), saved).unwrap()));
        assert_eq!(lock(&list).resume_at, None);
        let mut p = CatalogProvider::new(fx.ctx.clone(), list);
        let item = p.next_track().unwrap();
        assert_eq!((item.id, item.start_ms), (order[2], 0));
    }

    /// Saves the playlist with `current` at `ms` and restores it, as on a restart.
    fn saved_and_restored(fx: &Fx, current: TrackId, ms: u64) -> SharedPlaylist {
        let job = save_job(&fx.list, Some((current, ms)));
        fx.ctx.writer.call(move |c| job.run(c)).unwrap();
        let saved = lock(&fx.query).load_playlist().unwrap().unwrap();
        Arc::new(Mutex::new(restore(&lock(&fx.query), saved).unwrap()))
    }

    #[test]
    fn the_resume_track_survives_two_network_failures_then_the_next_one_plays() {
        use smp_core::failure::{FailCode, Failure};
        for saved_ms in [42_000, 0] {
            let fx = Fx::new();
            let mut p = fx.play_genres(&["jazz"]);
            take(&mut p);
            let current = take(&mut p).unwrap();
            let order = row_ids(&fx.page(0, 100, None));
            let list = saved_and_restored(&fx, current, saved_ms);
            let mut p = CatalogProvider::new(fx.ctx.clone(), list.clone());
            let unreachable = Failure::new(FailCode::Unreachable, "no route");
            for _ in 0..RESUME_RETRIES - 1 {
                let item = p.next_track().unwrap();
                assert_eq!((item.id, item.start_ms), (current, saved_ms), "@{saved_ms}");
                p.on_failed(item.id, &unreachable);
            }
            assert_eq!(lock(&list).resume_at, Some((current, saved_ms)), "kept");
            let item = p.next_track().unwrap();
            assert_eq!(item.id, current);
            p.on_failed(item.id, &unreachable);
            let next = p.next_track().unwrap();
            assert_eq!(
                (next.id, next.start_ms),
                (order[2], 0),
                "@{saved_ms}: moved on"
            );
            assert_eq!(lock(&list).resume_at, None);
        }
    }

    #[test]
    fn a_started_track_resets_the_resume_retries() {
        use smp_core::failure::{FailCode, Failure};
        let fx = Fx::new();
        let mut p = fx.play_genres(&["jazz"]);
        let current = take(&mut p).unwrap();
        let list = saved_and_restored(&fx, current, 1_000);
        let mut p = CatalogProvider::new(fx.ctx.clone(), list.clone());
        let item = p.next_track().unwrap();
        p.on_failed(item.id, &Failure::new(FailCode::Timeout, "slow"));
        assert_eq!(lock(&list).resume_failures, 1);
        p.on_started(current);
        assert_eq!(lock(&list).resume_failures, 0);
    }

    #[test]
    fn the_order_counts_as_saved_only_once_written() {
        let fx = Fx::new();
        fx.play_genres(&["jazz"]);
        let lost = save_job(&fx.list, None);
        assert!(lost.order.is_some());
        drop(lost); // never ran (the write failed or was not made)
        let job = save_job(&fx.list, None);
        assert!(job.order.is_some(), "still to be saved");
        fx.ctx.writer.call(move |c| job.run(c)).unwrap();
        assert!(save_job(&fx.list, None).order.is_none(), "saved now");
    }

    #[test]
    fn restore_keeps_hand_added_tracks_that_failed() {
        use smp_core::failure::{FailCode, Failure};
        let fx = Fx::new();
        let mut p = fx.play_genres(&["jazz"]);
        let current = take(&mut p).unwrap();
        let shuffled = row_ids(&fx.page(0, 100, None))[3];
        // Track 1 (Trance) is in the Unreadable list and added by hand.
        lock(&fx.list).queue.append(&[1]);
        for id in [1, shuffled] {
            for _ in 0..3 {
                lock(&fx.query)
                    .record_failure(id, &Failure::new(FailCode::Decode, "bad"), None)
                    .unwrap();
            }
        }
        let list = saved_and_restored(&fx, current, 0);
        let order: Vec<TrackId> = lock(&list).queue.order().iter().map(|e| e.id).collect();
        assert!(order.contains(&1), "added by hand: kept");
        assert!(
            !order.contains(&shuffled),
            "left out of the shuffle: dropped"
        );
    }

    #[test]
    fn a_restored_playlist_leaves_out_libraries_that_are_off_or_gone() {
        let fx = Fx::new();
        let meta = smp_core::catalog::TrackMeta::default();
        let (usb, lost) = {
            let q = lock(&fx.query);
            (
                q.upsert_track(2, "usb/a.mp3", 1, 1, &meta).unwrap(),
                q.upsert_track(9, "lost/b.mp3", 1, 1, &meta).unwrap(),
            )
        };
        let mut p = fx.play_genres(&["jazz"]);
        let current = take(&mut p).unwrap();
        // Added by hand: one track of a USB library, one of a library the
        // settings no longer have.
        lock(&fx.list).queue.append(&[usb, lost]);
        // At start the app marks them off before the playlist is restored.
        lock(&fx.query).set_disabled_libraries(&[2, 9]).unwrap();
        let list = saved_and_restored(&fx, current, 0);
        let order: Vec<TrackId> = lock(&list).queue.order().iter().map(|e| e.id).collect();
        assert!(!order.contains(&usb) && !order.contains(&lost), "{order:?}");
        assert!(order.contains(&current));
    }

    #[test]
    fn every_genre_run_shuffles_anew_and_avoids_the_last_first_track() {
        let fx = Fx::new();
        let key = selection_key(&["trance".to_string()]);
        let last = lock(&fx.query).playable_tracks(&["trance".into()]).unwrap()[0];
        lock(&fx.query).set_last_first(&key, last).unwrap();
        let firsts: HashSet<TrackId> = (0..40)
            .map(|_| {
                let mut q = genres_queue(&lock(&fx.query), &["trance".into()]).unwrap();
                let first = q.successor().unwrap();
                assert_ne!(first, last, "not the first track of the last run");
                first
            })
            .collect();
        assert!(firsts.len() > 5, "entropy-seeded: {firsts:?}");
    }

    #[test]
    fn an_ended_playlist_is_restored_without_a_track_to_resume() {
        let fx = Fx::new();
        lock(&fx.list).replace(Selection::Manual, Queue::fixed(&[1], entropy_rng()));
        let mut p = fx.provider();
        take(&mut p);
        assert_eq!(p.next_track(), None);
        lock(&fx.list).ended = true;
        let job = save_job(&fx.list, Some((1, 100)));
        fx.ctx.writer.call(move |c| job.run(c)).unwrap();
        let saved = lock(&fx.query).load_playlist().unwrap().unwrap();
        let mut l = restore(&lock(&fx.query), saved).unwrap();
        assert!(l.ended);
        assert_eq!(l.resume_at, None);
        assert_eq!(l.queue.successor(), None);
    }

    #[test]
    fn export_writes_m3u8_of_the_playlist_or_the_filtered_rows() {
        let fx = Fx::new();
        lock(&fx.list).replace(Selection::Manual, Queue::fixed(&[3, 31, 1], entropy_rng()));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Mix 2026-10-25.m3u8");
        assert_eq!(export_m3u8(&fx.list, &fx.query, &path, None).unwrap(), 3);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#EXTM3U\r\n\
             #EXTINF:200,Alpha - Song t/02.mp3\r\nt/02.mp3\r\n\
             #EXTINF:200,Gamma - Song j/0.mp3\r\nj/0.mp3\r\n\
             #EXTINF:200,Alpha - Song t/00.mp3\r\nt/00.mp3\r\n"
        );
        let f = QueueFilter {
            genres: vec!["jazz".into()],
            ..Default::default()
        };
        assert_eq!(
            export_m3u8(&fx.list, &fx.query, &path, Some(&f)).unwrap(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#EXTM3U\r\n#EXTINF:200,Gamma - Song j/0.mp3\r\nj/0.mp3\r\n"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file left"
        );
        let bad = dir.path().join("no/such/dir/x.m3u8");
        assert!(export_m3u8(&fx.list, &fx.query, &bad, None).is_err());
        assert_eq!(
            export_m3u8(&fx.list, &fx.query, Path::new("/"), None),
            Err(crate::i18n::t("err.no_file_name")),
            "translated"
        );
    }

    #[test]
    fn the_previous_button_restarts_after_3_seconds() {
        use PreviousAction::*;
        assert_eq!(previous_action(0, true), Back);
        assert_eq!(previous_action(3_000, true), Back);
        assert_eq!(previous_action(3_001, true), Restart);
        assert_eq!(
            previous_action(500, false),
            Restart,
            "nothing to go back to"
        );
    }

    #[test]
    fn titles_and_file_names() {
        let fx = Fx::new();
        let q = lock(&fx.query);
        let t = |s: Selection| title(&q, "hr", &s);
        assert_eq!(t(Selection::Genres { keys: vec![] }), "Sve");
        assert_eq!(
            t(Selection::Genres {
                keys: vec!["jazz".into(), UNKNOWN_KEY.into()]
            }),
            "Jazz + Nepoznato"
        );
        assert_eq!(
            t(Selection::Album {
                artist: "A".into(),
                album: "B".into()
            }),
            "A – B"
        );
        assert_eq!(file_stem("AC/DC: Live? \"x\""), "AC-DC- Live- -x-");
        assert_eq!(file_stem(" .. "), "Playlist");
        let json = serde_json::to_string(&Selection::Filtered {
            base: Box::new(Selection::Manual),
            filter: QueueFilter::default(),
        })
        .unwrap();
        assert!(
            json.starts_with(r#"{"kind":"filtered","base":{"kind":"manual"}"#),
            "{json}"
        );
    }
}
