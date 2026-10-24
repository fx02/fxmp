//! Track providers over the catalog: a genre shuffle or an album in order.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use rand::rngs::StdRng;
use smp_core::audio::dsp::track_gain_db;
use smp_core::audio::player::{PlayItem, TrackProvider};
use smp_core::catalog::{selection_key, Catalog};
use smp_core::shuffle::{entropy_shuffler, Shuffler};
use smp_core::TrackId;

/// Tracks banned while the app runs, so a running shuffle stops handing them out.
pub type BannedSet = Arc<Mutex<HashSet<TrackId>>>;

enum Order {
    Shuffle {
        shuffler: Box<Shuffler<StdRng>>,
        selection_key: String,
        first_recorded: bool,
    },
    Album(VecDeque<TrackId>),
}

pub struct CatalogProvider {
    catalog: Arc<Mutex<Catalog>>,
    banned: BannedSet,
    order: Order,
}

impl CatalogProvider {
    /// Fresh shuffle of the playable tracks in `genre_keys` (empty = everything).
    pub fn shuffle(
        catalog: Arc<Mutex<Catalog>>,
        banned: BannedSet,
        genre_keys: &[String],
    ) -> smp_core::Result<Self> {
        let selection_key = selection_key(genre_keys);
        let shuffler = {
            let c = lock(&catalog);
            entropy_shuffler(
                c.playable_tracks(genre_keys)?,
                c.last_first(&selection_key)?,
            )
        };
        Ok(Self {
            catalog,
            banned,
            order: Order::Shuffle {
                shuffler: Box::new(shuffler),
                selection_key,
                first_recorded: false,
            },
        })
    }

    /// An album in track order, without banned tracks.
    pub fn album(
        catalog: Arc<Mutex<Catalog>>,
        banned: BannedSet,
        artist: &str,
        album: &str,
    ) -> smp_core::Result<Self> {
        let tracks = {
            let c = lock(&catalog);
            let banned: HashSet<TrackId> = c.banned()?.into_iter().map(|t| t.id).collect();
            c.album_tracks(artist, album)?
                .into_iter()
                .filter(|id| !banned.contains(id))
                .collect()
        };
        Ok(Self {
            catalog,
            banned,
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

/// The catalog stays usable after a panic elsewhere; its state is all in SQLite.
fn lock(catalog: &Mutex<Catalog>) -> std::sync::MutexGuard<'_, Catalog> {
    catalog.lock().unwrap_or_else(|e| e.into_inner())
}

impl TrackProvider for CatalogProvider {
    fn next_track(&mut self) -> Option<PlayItem> {
        loop {
            let id = self.next_id()?;
            if self
                .banned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&id)
            {
                self.forget(id);
                continue;
            }
            let catalog = self.catalog.clone();
            let c = lock(&catalog);
            let row = match c.track(id) {
                Ok(row) => row,
                Err(e) => {
                    // Removed by a rescan since the run started.
                    log::debug!("track {id} unavailable: {e}");
                    drop(c);
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
                    if let Err(e) = c.set_last_first(selection_key, id) {
                        log::warn!("cannot remember first track: {e}");
                    }
                }
            }
            return Some(PlayItem {
                id,
                rel_path: row.rel_path,
                gain_db: track_gain_db(row.meta.rg_track_gain_db, row.measured_lufs),
            });
        }
    }

    fn on_failed(&mut self, id: TrackId) {
        if let Err(e) = lock(&self.catalog).record_failure(id) {
            log::warn!("cannot record failure of track {id}: {e}");
        }
        self.forget(id);
    }

    fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>) {
        let c = lock(&self.catalog);
        if let Err(e) = c.record_success(id) {
            log::warn!("cannot reset failures of track {id}: {e}");
        }
        if let Some(lufs) = measured_lufs {
            if let Err(e) = c.set_measured_lufs(id, lufs) {
                log::warn!("cannot store loudness of track {id}: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smp_core::catalog::TrackMeta;

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

    fn catalog() -> (Arc<Mutex<Catalog>>, Vec<TrackId>) {
        let cat = Catalog::open_in_memory().unwrap();
        let ids = (0..5)
            .map(|i| track(&cat, &format!("t/{i}.mp3"), "Trance", None))
            .collect();
        track(&cat, "j/0.mp3", "Jazz", None);
        cat.rebuild_genres().unwrap();
        (Arc::new(Mutex::new(cat)), ids)
    }

    fn banned() -> BannedSet {
        Arc::default()
    }

    #[test]
    fn catalog_provider_records_last_first_and_failures() {
        let (cat, trance) = catalog();
        let keys = vec!["trance".to_string()];
        let key = selection_key(&keys);

        let mut p = CatalogProvider::shuffle(cat.clone(), banned(), &keys).unwrap();
        assert_eq!(cat.lock().unwrap().last_first(&key).unwrap(), None);
        let first = p.next_track().unwrap();
        assert!(trance.contains(&first.id));
        assert_eq!(
            cat.lock().unwrap().last_first(&key).unwrap(),
            Some(first.id)
        );

        // Only the first track of the run is recorded.
        let second = p.next_track().unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(
            cat.lock().unwrap().last_first(&key).unwrap(),
            Some(first.id)
        );

        // A failed track is counted in the catalog and leaves this run's shuffle.
        p.on_failed(second.id);
        for _ in 0..20 {
            assert_ne!(p.next_track().unwrap().id, second.id);
        }
        p.on_failed(second.id);
        p.on_failed(second.id);
        let playable = cat.lock().unwrap().playable_tracks(&keys).unwrap();
        assert!(
            !playable.contains(&second.id),
            "3 failures exclude the track"
        );

        // The next run of the same selection avoids the previous first track.
        for _ in 0..20 {
            let prev = cat.lock().unwrap().last_first(&key).unwrap();
            let mut p = CatalogProvider::shuffle(cat.clone(), banned(), &keys).unwrap();
            let id = p.next_track().unwrap().id;
            assert_ne!(Some(id), prev);
            assert_eq!(cat.lock().unwrap().last_first(&key).unwrap(), Some(id));
        }
    }

    #[test]
    fn on_played_resets_failures_and_stores_loudness() {
        let (cat, _) = catalog();
        let keys = vec!["jazz".to_string()];
        let mut p = CatalogProvider::shuffle(cat.clone(), banned(), &keys).unwrap();
        let item = p.next_track().unwrap();
        assert_eq!(item.gain_db, 0.0, "no ReplayGain, not measured yet");
        p.on_failed(item.id);
        p.on_failed(item.id);
        p.on_played(item.id, Some(-8.0));
        let c = cat.lock().unwrap();
        assert_eq!(c.track(item.id).unwrap().measured_lufs, Some(-8.0));
        assert_eq!(c.record_failure(item.id).unwrap(), 1, "failures were reset");
    }

    #[test]
    fn gain_comes_from_replaygain_or_measured_loudness_clamped() {
        let cat = Catalog::open_in_memory().unwrap();
        let meta = TrackMeta {
            raw_genre: Some("Rock".into()),
            rg_track_gain_db: Some(-30.0),
            ..Default::default()
        };
        let rg = cat.upsert_track("a.mp3", 1, 1, &meta).unwrap();
        let measured = track(&cat, "b.mp3", "Pop", None);
        cat.set_measured_lufs(measured, -24.0).unwrap();
        cat.rebuild_genres().unwrap();
        let cat = Arc::new(Mutex::new(cat));

        let mut p = CatalogProvider::shuffle(cat.clone(), banned(), &["rock".into()]).unwrap();
        let item = p.next_track().unwrap();
        assert_eq!(item.id, rg);
        assert_eq!(item.gain_db, -15.0, "clamped to [-15, +6]");

        let mut p = CatalogProvider::shuffle(cat, banned(), &["pop".into()]).unwrap();
        let item = p.next_track().unwrap();
        assert_eq!(item.rel_path, "b.mp3");
        assert_eq!(item.gain_db, 6.0, "-18 LUFS target from -24, clamped to +6");
    }

    #[test]
    fn album_plays_in_order_without_banned_tracks() {
        let cat = Catalog::open_in_memory().unwrap();
        let t3 = track(&cat, "al/3.mp3", "Rock", Some(("Album", 3)));
        let t1 = track(&cat, "al/1.mp3", "Rock", Some(("Album", 1)));
        let t2 = track(&cat, "al/2.mp3", "Rock", Some(("Album", 2)));
        cat.set_banned(t2, true).unwrap();
        let cat = Arc::new(Mutex::new(cat));

        let mut p = CatalogProvider::album(cat, banned(), "Artist", "Album").unwrap();
        let got: Vec<_> = std::iter::from_fn(|| p.next_track().map(|i| i.id)).collect();
        assert_eq!(got, vec![t1, t3]);
    }

    #[test]
    fn tracks_banned_during_a_run_are_skipped() {
        let (cat, trance) = catalog();
        let banned = banned();
        let mut p = CatalogProvider::shuffle(cat, banned.clone(), &["trance".into()]).unwrap();
        banned.lock().unwrap().insert(trance[0]);
        for _ in 0..30 {
            assert_ne!(p.next_track().unwrap().id, trance[0]);
        }
    }
}
