//! Track providers over the catalog: a genre shuffle or an album in order.
//!
//! The player calls a provider on its audio engine thread, so a provider only reads
//! (on a connection of its own, which a scan's write transaction never blocks under
//! WAL) and hands every write to the [`CatalogWriter`].

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use rand::rngs::StdRng;
use smp_core::audio::dsp::track_gain_db;
use smp_core::audio::player::{PlayItem, TrackProvider};
use smp_core::catalog::{selection_key, Catalog};
use smp_core::failure::Failure;
use smp_core::shuffle::{entropy_shuffler, Shuffler};
use smp_core::source::Which;
use smp_core::TrackId;

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

enum Order {
    Shuffle {
        shuffler: Box<Shuffler<StdRng>>,
        selection_key: String,
        first_recorded: bool,
    },
    Album(VecDeque<TrackId>),
}

pub struct CatalogProvider {
    ctx: Playback,
    order: Order,
}

impl CatalogProvider {
    /// Fresh shuffle of the playable tracks in `genre_keys` (empty = everything);
    /// `query` is used to build the pool.
    pub fn shuffle(
        query: &Catalog,
        ctx: Playback,
        genre_keys: &[String],
    ) -> smp_core::Result<Self> {
        let selection_key = selection_key(genre_keys);
        let shuffler = entropy_shuffler(
            query.playable_tracks(genre_keys)?,
            query.last_first(&selection_key)?,
        );
        Ok(Self {
            ctx,
            order: Order::Shuffle {
                shuffler: Box::new(shuffler),
                selection_key,
                first_recorded: false,
            },
        })
    }

    /// An album in track order, without banned tracks.
    pub fn album(
        query: &Catalog,
        ctx: Playback,
        artist: &str,
        album: &str,
    ) -> smp_core::Result<Self> {
        let banned: HashSet<TrackId> = query.banned()?.into_iter().map(|t| t.id).collect();
        let tracks = query
            .album_tracks(artist, album)?
            .into_iter()
            .filter(|id| !banned.contains(id))
            .collect();
        Ok(Self {
            ctx,
            order: Order::Album(tracks),
        })
    }

    fn next_id(&mut self) -> Option<TrackId> {
        match &mut self.order {
            Order::Shuffle { shuffler, .. } => shuffler.next(),
            Order::Album(q) => q.pop_front(),
        }
    }

    fn forget(&mut self, id: TrackId) {
        match &mut self.order {
            Order::Shuffle { shuffler, .. } => shuffler.remove(id),
            Order::Album(q) => q.retain(|&t| t != id),
        }
    }
}

impl TrackProvider for CatalogProvider {
    fn next_track(&mut self) -> Option<PlayItem> {
        loop {
            let id = self.next_id()?;
            if crate::state::lock(&self.ctx.banned).contains(&id) {
                self.forget(id);
                continue;
            }
            let row = crate::state::lock(&self.ctx.db).track(id);
            let row = match row {
                Ok(row) => row,
                Err(e) => {
                    // Removed by a rescan since the run started.
                    log::debug!("track {id} unavailable: {e}");
                    self.forget(id);
                    continue;
                }
            };
            if let Order::Shuffle {
                selection_key,
                first_recorded,
                ..
            } = &mut self.order
            {
                if !*first_recorded {
                    *first_recorded = true;
                    let key = selection_key.clone();
                    self.ctx
                        .writer
                        .send("first track", move |c| c.set_last_first(&key, id));
                }
            }
            return Some(PlayItem {
                id,
                rel_path: row.rel_path,
                gain_db: track_gain_db(row.meta.rg_track_gain_db, row.measured_lufs),
                start_ms: 0,
            });
        }
    }

    fn on_failed(&mut self, id: TrackId, failure: &Failure) {
        let failure = failure.clone();
        let server = (self.ctx.active_server)();
        self.ctx.writer.send("failure", move |c| {
            c.record_failure(id, &failure, server).map(drop)
        });
        self.forget(id);
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
    use smp_core::catalog::TrackMeta;
    use smp_core::failure::FailCode;
    use std::time::{Duration, Instant};

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
            CatalogProvider::shuffle(&self.query, self.ctx.clone(), keys).unwrap()
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
        let first = p.next_track().unwrap();
        assert!(trance.contains(&first.id));
        fx.flush();
        assert_eq!(fx.query.last_first(&key).unwrap(), Some(first.id));

        // Only the first track of the run is recorded.
        let second = p.next_track().unwrap();
        assert_ne!(second.id, first.id);
        fx.flush();
        assert_eq!(fx.query.last_first(&key).unwrap(), Some(first.id));

        // A failed track is counted in the catalog and leaves this run's shuffle.
        p.on_failed(second.id, &decode_failure());
        for _ in 0..20 {
            assert_ne!(p.next_track().unwrap().id, second.id);
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
            let id = fx.shuffle(&keys).next_track().unwrap().id;
            assert_ne!(Some(id), prev);
            fx.flush();
            assert_eq!(fx.query.last_first(&key).unwrap(), Some(id));
        }
    }

    #[test]
    fn failures_are_stored_with_reason_and_server() {
        let (fx, _) = Fx::with_genres();
        let mut p = fx.shuffle(&["jazz".to_string()]);
        let item = p.next_track().unwrap();
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
        let item = p.next_track().unwrap();
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

        let mut p = CatalogProvider::album(&fx.query, fx.ctx.clone(), "Artist", "Album").unwrap();
        let got: Vec<_> = std::iter::from_fn(|| p.next_track().map(|i| i.id)).collect();
        assert_eq!(got, vec![t1, t3]);
    }

    #[test]
    fn tracks_banned_during_a_run_are_skipped() {
        let (fx, trance) = Fx::with_genres();
        let mut p = fx.shuffle(&["trance".into()]);
        fx.ctx.banned.lock().unwrap().insert(trance[0]);
        for _ in 0..30 {
            assert_ne!(p.next_track().unwrap().id, trance[0]);
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
        let item = p.next_track().unwrap();
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
