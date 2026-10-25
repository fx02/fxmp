//! The track provider over the catalog: hands the player the playlist's tracks.
//!
//! The player calls a provider on its audio engine thread, so a provider only reads
//! (on a connection of its own, which a scan's write transaction never blocks under
//! WAL) and hands every write to the [`CatalogWriter`]. The playlist itself is
//! shared with the commands (see [`crate::playlist`]); its lock is held only briefly.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use smp_core::audio::dsp::track_gain_db;
use smp_core::audio::player::{PlayItem, TrackProvider};
use smp_core::catalog::{selection_key, Catalog};
use smp_core::failure::{FailCode, Failure};
use smp_core::source::Which;
use smp_core::{LibraryId, TrackId};

use crate::playlist::{Selection, SharedPlaylist, RESUME_RETRIES};
use crate::state::lock;
use crate::writer::CatalogWriter;

/// Tracks banned while the app runs, so a running shuffle stops handing them out.
pub type BannedSet = Arc<Mutex<HashSet<TrackId>>>;

/// What the provider needs to know about the libraries while playing.
pub trait LibraryHealth: Send + Sync {
    /// The server an SMB library is reading from, if known (recorded with a failure).
    fn active_server(&self, library: LibraryId) -> Option<Which>;
    /// The library cannot be read now (a disk unplugged, a server down): a failure
    /// of one of its tracks is not the file's fault. Called on the audio engine
    /// thread, so it must answer at once (no file system or network access).
    fn offline(&self, library: LibraryId) -> bool;
    /// A track of `library` failed as if its file were bad while the library
    /// counted as online: have the library checked soon (without waiting).
    fn recheck(&self, _library: LibraryId) {}
    /// Some library can be read now.
    fn any_online(&self) -> bool;
}

/// What a provider needs while playing.
#[derive(Clone)]
pub struct Playback {
    /// Read connection used only on the playback path.
    pub db: Arc<Mutex<Catalog>>,
    pub writer: CatalogWriter,
    pub banned: BannedSet,
    pub libraries: Arc<dyn LibraryHealth>,
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
    /// Tracks gone, banned or of a library switched off leave the playlist. While
    /// some library answers, tracks of an offline one are passed over here (they
    /// stay in the playlist), never handed to the player: a long run of them
    /// (an album on an unplugged disk) would count as failures in a row and stop
    /// playback. With no library answering they are handed out as before (the
    /// network may just be back). At most one round of the playlist is passed
    /// over; then there is nothing to play.
    fn next_track(&mut self) -> Option<PlayItem> {
        let mut any_online: Option<bool> = None;
        let mut offline: HashMap<LibraryId, bool> = HashMap::new();
        let mut passed = 0;
        let limit = lock(&self.list).queue.order().len() + 1;
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
            let row = {
                let db = lock(&self.ctx.db);
                db.track(id)
                    .and_then(|row| Ok((db.disabled_libraries()?, row)))
            };
            let row = match row {
                Ok((off, row)) if off.contains(&row.library_id) => {
                    // Its library was switched off since the playlist was made.
                    log::debug!("track {id}: library {} is off", row.library_id);
                    lock(&self.list).queue.forget(id);
                    continue;
                }
                Ok((_, row))
                    if *offline
                        .entry(row.library_id)
                        .or_insert_with(|| self.ctx.libraries.offline(row.library_id))
                        && *any_online.get_or_insert_with(|| self.ctx.libraries.any_online()) =>
                {
                    passed += 1;
                    if passed > limit {
                        log::info!("nothing to play: every due track is in an offline library");
                        return None;
                    }
                    log::debug!(
                        "track {id}: library {} is offline, passed over",
                        row.library_id
                    );
                    lock(&self.list).queue.pass_over(id);
                    continue;
                }
                Ok((_, row)) => row,
                Err(e) => {
                    // Removed by a rescan since the playlist was made.
                    log::debug!("track {id} unavailable: {e}");
                    lock(&self.list).queue.forget(id);
                    continue;
                }
            };
            return Some(PlayItem {
                id,
                library: row.library_id,
                rel_path: row.rel_path,
                gain_db: track_gain_db(row.meta.rg_track_gain_db, row.measured_lufs),
                start_ms,
            });
        }
    }

    fn on_started(&mut self, id: TrackId) {
        let mut l = lock(&self.list);
        let expected = l.queue.started(id);
        l.ended = false;
        l.resume_failures = 0;
        if l.resume_at.is_some_and(|r| r.0 == id) {
            l.resume_at = None;
        }
        let key = match &l.selection {
            // Not a track of an older selection that started just as this one came.
            Selection::Genres { keys } if expected && !l.first_recorded => {
                Some(selection_key(keys))
            }
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
        let library = lock(&self.ctx.db).track(id).map(|r| r.library_id).ok();
        let failure = match library {
            // Its library is offline (a disk unplugged): "not found" and the like
            // say nothing about the file, so it counts as network trouble.
            Some(lib) if !failure.code.is_transient() && self.ctx.libraries.offline(lib) => {
                log::info!("track {id} failed while library {lib} is offline: not counted");
                Failure::new(FailCode::Unreachable, &failure.message)
            }
            Some(lib) if !failure.code.is_transient() => {
                self.ctx.libraries.recheck(lib);
                failure.clone()
            }
            _ => failure.clone(),
        };
        let transient = failure.code.is_transient();
        let server = library.and_then(|lib| self.ctx.libraries.active_server(lib));
        self.ctx.writer.send("failure", move |c| {
            c.record_failure(id, &failure, server).map(drop)
        });
        let mut l = lock(&self.list);
        let resuming = transient && l.resume_at.is_some_and(|r| r.0 == id);
        if resuming && l.resume_failures + 1 < RESUME_RETRIES {
            // The share is not quite up yet: keep the track to resume and where,
            // rather than walking the playlist.
            l.resume_failures += 1;
        } else if transient {
            if resuming {
                // It keeps failing: go on with the next track, from its start.
                l.resume_at = None;
                l.resume_failures = 0;
            }
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
        cat.upsert_track(smp_core::DEFAULT_LIBRARY, path, 1, 1, &meta)
            .unwrap()
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
                libraries: Arc::new(FakeLibraries::default()),
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

    /// Library 2 is offline when `lib2_offline`; every library plays from the secondary.
    #[derive(Default)]
    struct FakeLibraries {
        lib2_offline: std::sync::atomic::AtomicBool,
        rechecks: Mutex<Vec<smp_core::LibraryId>>,
    }

    impl LibraryHealth for FakeLibraries {
        fn active_server(&self, _: smp_core::LibraryId) -> Option<Which> {
            Some(Which::Secondary)
        }
        fn offline(&self, library: smp_core::LibraryId) -> bool {
            library == 2 && self.lib2_offline.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn recheck(&self, library: smp_core::LibraryId) {
            lock(&self.rechecks).push(library);
        }
        fn any_online(&self) -> bool {
            true
        }
    }

    /// Nothing answers at all.
    struct AllDown;
    impl LibraryHealth for AllDown {
        fn active_server(&self, _: smp_core::LibraryId) -> Option<Which> {
            None
        }
        fn offline(&self, _: smp_core::LibraryId) -> bool {
            true
        }
        fn any_online(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_long_run_of_offline_tracks_is_passed_by_to_an_online_one() {
        let fx = Fx::new();
        let health = Arc::new(FakeLibraries::default());
        health
            .lib2_offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let meta = TrackMeta::default();
        // An album on the unplugged disk (25 tracks, more than the player's 20
        // failures in a row), then a track on the server.
        let mut ids: Vec<TrackId> = (0..25)
            .map(|i| {
                fx.query
                    .upsert_track(2, &format!("usb/{i}.mp3"), 1, 1, &meta)
                    .unwrap()
            })
            .collect();
        let online = fx.query.upsert_track(1, "nas/a.mp3", 1, 1, &meta).unwrap();
        ids.push(online);
        let list = SharedPlaylist::default();
        lock(&list).replace(
            Selection::Manual,
            smp_core::queue::Queue::fixed(&ids, smp_core::shuffle::entropy_rng()),
        );
        let ctx = Playback {
            libraries: health.clone(),
            ..fx.ctx.clone()
        };
        let mut p = CatalogProvider::new(ctx, list.clone());
        let item = take_item(&mut p);
        assert_eq!(
            (item.id, item.library),
            (online, 1),
            "never handed to the player"
        );
        assert_eq!(lock(&list).queue.order().len(), 26, "kept in the playlist");
        fx.flush();
        assert!(fx.query.unreadable().unwrap().is_empty(), "nothing counted");
        assert_eq!(take(&mut p), None);

        // Nothing answers at all: tracks are handed out as before (the network
        // may just be back), and fail as network trouble.
        let list = SharedPlaylist::default();
        lock(&list).replace(
            Selection::Manual,
            smp_core::queue::Queue::fixed(&ids[..2], smp_core::shuffle::entropy_rng()),
        );
        let ctx = Playback {
            libraries: Arc::new(AllDown),
            ..fx.ctx.clone()
        };
        let mut p = CatalogProvider::new(ctx, list);
        assert_eq!(p.next_track().unwrap().id, ids[0]);
    }

    #[test]
    fn an_offline_shuffle_does_not_spin() {
        let fx = Fx::new();
        let health = Arc::new(FakeLibraries::default());
        health
            .lib2_offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let meta = TrackMeta {
            raw_genre: Some("Rock".into()),
            ..Default::default()
        };
        for i in 0..5 {
            fx.query
                .upsert_track(2, &format!("usb/{i}.mp3"), 1, 1, &meta)
                .unwrap();
        }
        fx.query.rebuild_genres().unwrap();
        let ctx = Playback {
            libraries: health,
            ..fx.ctx.clone()
        };
        let q = crate::playlist::genres_queue(&fx.query, &[]).unwrap();
        let list = SharedPlaylist::default();
        lock(&list).replace(Selection::Genres { keys: vec![] }, q);
        let mut p = CatalogProvider::new(ctx, list);
        assert_eq!(
            p.next_track(),
            None,
            "every track offline: nothing, not a loop"
        );
    }

    #[test]
    fn failures_while_a_library_is_offline_are_not_counted() {
        let fx = Fx::new();
        let health = Arc::new(FakeLibraries::default());
        let ctx = Playback {
            libraries: health.clone(),
            ..fx.ctx.clone()
        };
        let meta = TrackMeta {
            raw_genre: Some("Rock".into()),
            ..Default::default()
        };
        let on_disk = fx.query.upsert_track(2, "a.mp3", 1, 1, &meta).unwrap();
        let on_nas = fx.query.upsert_track(1, "b.mp3", 1, 1, &meta).unwrap();
        fx.query.rebuild_genres().unwrap();
        let list = SharedPlaylist::default();
        lock(&list).replace(
            Selection::Manual,
            smp_core::queue::Queue::fixed(&[on_disk, on_nas], smp_core::shuffle::entropy_rng()),
        );
        let mut p = CatalogProvider::new(ctx, list.clone());
        // The disk is unplugged: its file is "not found", but that is not the file's fault.
        health
            .lib2_offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let gone = Failure::new(FailCode::NotFound, "no such file");
        for _ in 0..3 {
            p.on_failed(on_disk, &gone);
        }
        fx.flush();
        assert!(fx.query.unreadable().unwrap().is_empty(), "not listed");
        assert!(
            fx.query.playable_tracks(&[]).unwrap().contains(&on_disk),
            "not excluded"
        );
        assert!(
            lock(&list).queue.order().iter().any(|e| e.id == on_disk),
            "kept in the playlist"
        );
        assert!(lock(&health.rechecks).is_empty(), "known to be offline");
        // Plugged in again: a real failure counts.
        health
            .lib2_offline
            .store(false, std::sync::atomic::Ordering::SeqCst);
        p.on_failed(on_disk, &gone);
        // Library 1 is fine: its failures count as ever.
        p.on_failed(on_nas, &gone);
        assert_eq!(
            *lock(&health.rechecks),
            [2, 1],
            "each failure asks for a check"
        );
        p.on_failed(on_nas, &Failure::new(FailCode::Timeout, "slow"));
        assert_eq!(lock(&health.rechecks).len(), 2, "not for network trouble");
        fx.flush();
        let u = fx.query.unreadable().unwrap();
        assert_eq!(u.len(), 2);
        assert!(u.iter().all(|t| t.fail_count == 1));
    }

    #[test]
    fn tracks_of_a_switched_off_library_are_passed_by() {
        let fx = Fx::new();
        let meta = TrackMeta::default();
        let off = fx.query.upsert_track(2, "a.mp3", 1, 1, &meta).unwrap();
        let on = fx.query.upsert_track(1, "b.mp3", 1, 1, &meta).unwrap();
        fx.query.set_disabled_libraries(&[2]).unwrap();
        let list = SharedPlaylist::default();
        lock(&list).replace(
            Selection::Manual,
            smp_core::queue::Queue::fixed(&[off, on], smp_core::shuffle::entropy_rng()),
        );
        let mut p = CatalogProvider::new(fx.ctx.clone(), list);
        let item = take_item(&mut p);
        assert_eq!((item.id, item.library), (on, 1));
        assert_eq!(take(&mut p), None);
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
    fn a_track_of_the_old_selection_is_not_recorded_as_the_first() {
        let (fx, trance) = Fx::with_genres();
        let keys = vec!["jazz".to_string()];
        let mut p = fx.shuffle(&keys);
        // The old selection's prepared track starts before Play reaches the player.
        p.on_started(trance[0]);
        fx.flush();
        assert_eq!(fx.query.last_first(&selection_key(&keys)).unwrap(), None);
        let first = take_item(&mut p);
        fx.flush();
        assert_eq!(
            fx.query.last_first(&selection_key(&keys)).unwrap(),
            Some(first.id)
        );
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
        let rg = fx
            .query
            .upsert_track(smp_core::DEFAULT_LIBRARY, "a.mp3", 1, 1, &meta)
            .unwrap();
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
