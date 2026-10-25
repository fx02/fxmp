//! The track provider over the catalog: hands the player the playlist's tracks.
//!
//! The player calls a provider on its audio engine thread, so a provider only reads
//! (on a connection of its own, which a scan's write transaction never blocks under
//! WAL) and hands every write to the [`CatalogWriter`]. The playlist itself is
//! shared with the commands (see [`crate::playlist`]); its lock is held only briefly.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use smp_core::audio::dsp::track_gain_db;
use smp_core::audio::player::{PlayItem, TrackProvider};
use smp_core::catalog::{selection_key, Catalog};
use smp_core::failure::Failure;
use smp_core::source::Which;
use smp_core::TrackId;

use crate::playlist::{Selection, SharedPlaylist};
use crate::state::lock;
use crate::writer::CatalogWriter;

/// Tracks banned while the app runs, so a running shuffle stops handing them out.
pub type BannedSet = Arc<Mutex<HashSet<TrackId>>>;

/// The server playback is using, if known (recorded with a failure).
pub type ActiveServer = Arc<dyn Fn() -> Option<Which> + Send + Sync>;

/// What a provider needs while playing.
#[derive(Clone)]
pub struct Playback {
    /// Read connection used only on the playback path.
    pub db: Arc<Mutex<Catalog>>,
    pub writer: CatalogWriter,
    pub banned: BannedSet,
    pub active_server: ActiveServer,
}

/// Plays the shared playlist. Several can exist (one per `Play`); they all work on
/// the same playlist.
pub struct CatalogProvider {
    ctx: Playback,
    list: SharedPlaylist,
}

impl CatalogProvider {
    pub fn new(ctx: Playback, list: SharedPlaylist) -> Self {
        Self { ctx, list }
    }
}

impl TrackProvider for CatalogProvider {
    fn next_track(&mut self) -> Option<PlayItem> {
        loop {
            let (id, start_ms) = {
                let mut l = lock(&self.list);
                let id = l.queue.successor()?;
                let start = l.resume_at.filter(|r| r.0 == id).map_or(0, |r| r.1);
                (id, start)
            };
            if lock(&self.ctx.banned).contains(&id) {
                lock(&self.list).queue.forget(id);
                continue;
            }
            let row = lock(&self.ctx.db).track(id);
            let row = match row {
                Ok(row) => row,
                Err(e) => {
                    // Removed by a rescan since the playlist was made.
                    log::debug!("track {id} unavailable: {e}");
                    lock(&self.list).queue.forget(id);
                    continue;
                }
            };
            return Some(PlayItem {
                id,
                rel_path: row.rel_path,
                gain_db: track_gain_db(row.meta.rg_track_gain_db, row.measured_lufs),
                start_ms,
            });
        }
    }

    fn on_started(&mut self, id: TrackId) {
        let mut l = lock(&self.list);
        l.queue.started(id);
        l.ended = false;
        if l.resume_at.is_some_and(|r| r.0 == id) {
            l.resume_at = None;
        }
        let key = match &l.selection {
            Selection::Genres { keys } if !l.first_recorded => Some(selection_key(keys)),
            _ => None,
        };
        if let Some(key) = key {
            l.first_recorded = true;
            self.ctx
                .writer
                .send("first track", move |c| c.set_last_first(&key, id));
        }
    }

    fn on_failed(&mut self, id: TrackId, failure: &Failure) {
        let transient = failure.code.is_transient();
        let failure = failure.clone();
        let server = (self.ctx.active_server)();
        self.ctx.writer.send("failure", move |c| {
            c.record_failure(id, &failure, server).map(drop)
        });
        let mut l = lock(&self.list);
        if transient {
            // Network trouble is not the file's fault: it stays in the playlist.
            l.queue.pass_over(id);
        } else {
            l.queue.forget(id);
        }
    }

    fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>) {
        self.ctx.writer.send("played", move |c| {
            c.record_success(id)?;
            match measured_lufs {
                Some(lufs) => c.set_measured_lufs(id, lufs),
                None => Ok(()),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::{album_queue, genres_queue};
    use smp_core::catalog::TrackMeta;
    use smp_core::failure::FailCode;
    use std::time::{Duration, Instant};

    /// As the player does: ask for the next track, then start it.
    fn take_item(p: &mut CatalogProvider) -> PlayItem {
        let item = p.next_track().unwrap();
        p.on_started(item.id);
        item
    }

    fn take(p: &mut CatalogProvider) -> Option<TrackId> {
        let item = p.next_track()?;
        p.on_started(item.id);
        Some(item.id)
    }

    fn decode_failure() -> Failure {
        Failure::new(FailCode::Decode, "bad frame")
    }

    fn track(cat: &Catalog, path: &str, genre: &str, album: Option<(&str, u32)>) -> TrackId {
        let meta = TrackMeta {
            artist: Some("Artist".into()),
            album: album.map(|(a, _)| a.to_string()),
            title: Some(path.into()),
            track_no: album.map(|(_, n)| n),
            raw_genre: Some(genre.into()),
            ..Default::default()
        };
        cat.upsert_track(path, 1, 1, &meta).unwrap()
    }

    /// A catalog file with a query connection (also used for setup and checks) and
    /// the playback context (own read connection, writer thread).
    struct Fx {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        query: Catalog,
        ctx: Playback,
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("catalog.db");
            let query = Catalog::open(&path).unwrap();
            let ctx = Playback {
                db: Arc::new(Mutex::new(Catalog::open(&path).unwrap())),
                writer: CatalogWriter::spawn(Catalog::open(&path).unwrap()).unwrap(),
                banned: BannedSet::default(),
                active_server: Arc::new(|| Some(Which::Secondary)),
            };
            Self {
                _dir: dir,
                path,
                query,
                ctx,
            }
        }

        /// Five Trance tracks and one Jazz track; returns the Trance ids.
        fn with_genres() -> (Self, Vec<TrackId>) {
            let fx = Self::new();
            let ids = (0..5)
                .map(|i| track(&fx.query, &format!("t/{i}.mp3"), "Trance", None))
                .collect();
            track(&fx.query, "j/0.mp3", "Jazz", None);
            fx.query.rebuild_genres().unwrap();
            (fx, ids)
        }

        fn shuffle(&self, keys: &[String]) -> CatalogProvider {
            let q = genres_queue(&self.query, keys).unwrap();
            let list = SharedPlaylist::default();
            lock(&list).replace(
                Selection::Genres {
                    keys: keys.to_vec(),
                },
                q,
            );
            CatalogProvider::new(self.ctx.clone(), list)
        }

        /// Waits until queued writes are done.
        fn flush(&self) {
            self.ctx.writer.call(|_| Ok(())).unwrap();
        }
    }

    #[test]
    fn catalog_provider_records_last_first_and_failures() {
        let (fx, trance) = Fx::with_genres();
        let keys = vec!["trance".to_string()];
        let key = selection_key(&keys);

        let mut p = fx.shuffle(&keys);
        assert_eq!(fx.query.last_first(&key).unwrap(), None);
        let first = take_item(&mut p);
        assert!(trance.contains(&first.id));
        fx.flush();
        assert_eq!(fx.query.last_first(&key).unwrap(), Some(first.id));

        // Only the first track of the run is recorded.
        let second = take_item(&mut p);
        assert_ne!(second.id, first.id);
        fx.flush();
        assert_eq!(fx.query.last_first(&key).unwrap(), Some(first.id));

        // A failed track is counted in the catalog and leaves this run's shuffle.
        p.on_failed(second.id, &decode_failure());
        for _ in 0..20 {
            assert_ne!(take_item(&mut p).id, second.id);
        }
        p.on_failed(second.id, &decode_failure());
        p.on_failed(second.id, &decode_failure());
        fx.flush();
        let playable = fx.query.playable_tracks(&keys).unwrap();
        assert!(
            !playable.contains(&second.id),
            "3 failures exclude the track"
        );

        // The next run of the same selection avoids the previous first track.
        for _ in 0..20 {
            let prev = fx.query.last_first(&key).unwrap();
            let id = take_item(&mut fx.shuffle(&keys)).id;
            assert_ne!(Some(id), prev);
            fx.flush();
            assert_eq!(fx.query.last_first(&key).unwrap(), Some(id));
        }
    }

    #[test]
    fn failures_are_stored_with_reason_and_server() {
        let (fx, _) = Fx::with_genres();
        let mut p = fx.shuffle(&["jazz".to_string()]);
        let item = take_item(&mut p);
        p.on_failed(item.id, &Failure::new(FailCode::NotFound, "not found"));
        fx.flush();
        let u = fx.query.unreadable().unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].id, item.id);
        assert_eq!(u[0].code.as_deref(), Some("not_found"));
        assert_eq!(u[0].message.as_deref(), Some("not found"));
        assert_eq!(u[0].server, Some(Which::Secondary));
    }

    #[test]
    fn on_played_resets_failures_and_stores_loudness() {
        let (fx, _) = Fx::with_genres();
        let mut p = fx.shuffle(&["jazz".to_string()]);
        let item = take_item(&mut p);
        assert_eq!(item.gain_db, 0.0, "no ReplayGain, not measured yet");
        p.on_failed(item.id, &decode_failure());
        p.on_failed(item.id, &decode_failure());
        p.on_played(item.id, Some(-8.0));
        fx.flush();
        assert_eq!(fx.query.track(item.id).unwrap().measured_lufs, Some(-8.0));
        assert_eq!(
            fx.query
                .record_failure(item.id, &decode_failure(), None)
                .unwrap(),
            1,
            "failures were reset"
        );
    }

    #[test]
    fn gain_comes_from_replaygain_or_measured_loudness_clamped() {
        let fx = Fx::new();
        let meta = TrackMeta {
            raw_genre: Some("Rock".into()),
            rg_track_gain_db: Some(-30.0),
            ..Default::default()
        };
        let rg = fx.query.upsert_track("a.mp3", 1, 1, &meta).unwrap();
        let measured = track(&fx.query, "b.mp3", "Pop", None);
        fx.query.set_measured_lufs(measured, -24.0).unwrap();
        fx.query.rebuild_genres().unwrap();

        let item = fx.shuffle(&["rock".into()]).next_track().unwrap();
        assert_eq!(item.id, rg);
        assert_eq!(item.gain_db, -15.0, "clamped to [-15, +6]");

        let item = fx.shuffle(&["pop".into()]).next_track().unwrap();
        assert_eq!(item.rel_path, "b.mp3");
        assert_eq!(item.gain_db, 6.0, "-18 LUFS target from -24, clamped to +6");
    }

    #[test]
    fn album_plays_in_order_without_banned_tracks() {
        let fx = Fx::new();
        let t3 = track(&fx.query, "al/3.mp3", "Rock", Some(("Album", 3)));
        let t1 = track(&fx.query, "al/1.mp3", "Rock", Some(("Album", 1)));
        let t2 = track(&fx.query, "al/2.mp3", "Rock", Some(("Album", 2)));
        fx.query.set_banned(t2, true).unwrap();

        let list = SharedPlaylist::default();
        let q = album_queue(&fx.query, "Artist", "Album").unwrap();
        lock(&list).replace(Selection::Manual, q);
        let mut p = CatalogProvider::new(fx.ctx.clone(), list);
        let got: Vec<_> = std::iter::from_fn(|| take(&mut p)).collect();
        assert_eq!(got, vec![t1, t3]);
    }

    #[test]
    fn tracks_banned_during_a_run_are_skipped() {
        let (fx, trance) = Fx::with_genres();
        let mut p = fx.shuffle(&["trance".into()]);
        fx.ctx.banned.lock().unwrap().insert(trance[0]);
        for _ in 0..30 {
            assert_ne!(take_item(&mut p).id, trance[0]);
        }
    }

    #[test]
    fn provider_calls_do_not_wait_for_another_writer() {
        let (fx, ids) = Fx::with_genres();
        let mut p = fx.shuffle(&["trance".into()]);

        // Another connection (a scan, say) holds the write lock.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut blocker = Catalog::open(&fx.path).unwrap();
        let holder = std::thread::spawn(move || {
            blocker
                .transaction(|c| {
                    c.set_last_first("blocker", 1)?;
                    locked_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });
        locked_rx.recv().unwrap();

        let t = Instant::now();
        let item = take_item(&mut p);
        p.on_played(item.id, Some(-20.0));
        let other = *ids.iter().find(|&&i| i != item.id).unwrap();
        p.on_failed(other, &decode_failure());
        let took = t.elapsed();
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(took < Duration::from_millis(50), "took {took:?}");

        // The writes still land once the lock is free.
        fx.flush();
        let key = selection_key(&["trance".into()]);
        assert_eq!(fx.query.last_first(&key).unwrap(), Some(item.id));
        assert_eq!(fx.query.track(item.id).unwrap().measured_lufs, Some(-20.0));
        assert_eq!(
            fx.query
                .record_failure(other, &decode_failure(), None)
                .unwrap(),
            2
        );
    }
}
