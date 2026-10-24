//! The catalog writer: one thread with its own connection that performs every write
//! the app makes (playback bookkeeping, bans, genre rules). Writes can wait for a scan's
//! transaction (SQLite allows one writer); doing them here keeps that wait off the
//! audio engine thread and off the connections that commands read from.

use std::sync::mpsc::{self, Sender};

use smp_core::catalog::Catalog;

type Job = Box<dyn FnOnce(&mut Catalog) + Send>;

#[derive(Clone)]
pub struct CatalogWriter {
    tx: Sender<Job>,
}

impl CatalogWriter {
    pub fn spawn(mut catalog: Catalog) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("catalog-writer".into())
            .spawn(move || {
                for job in rx {
                    job(&mut catalog);
                }
            })?;
        Ok(Self { tx })
    }

    /// Queues a write and returns at once; a failure is only logged.
    pub fn send(
        &self,
        what: &'static str,
        f: impl FnOnce(&mut Catalog) -> smp_core::Result<()> + Send + 'static,
    ) {
        let job: Job = Box::new(move |c| {
            if let Err(e) = f(c) {
                log::warn!("catalog write ({what}) failed: {e}");
            }
        });
        if self.tx.send(job).is_err() {
            log::warn!("catalog writer gone; dropped {what}");
        }
    }

    /// Runs `f` on the writer thread and waits for its result. Never call this from
    /// the audio engine thread.
    pub fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Catalog) -> smp_core::Result<T> + Send + 'static,
    ) -> smp_core::Result<T> {
        let (tx, rx) = mpsc::channel();
        let job: Job = Box::new(move |c| {
            let _ = tx.send(f(c));
        });
        let gone = || smp_core::Error::Other("catalog writer stopped".into());
        self.tx.send(job).map_err(|_| gone())?;
        rx.recv().map_err(|_| gone())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_through_the_writer_empties_what_readers_see() {
        use smp_core::catalog::TrackMeta;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let reader = Catalog::open(&path).unwrap();
        let w = CatalogWriter::spawn(Catalog::open(&path).unwrap()).unwrap();
        w.call(|c| {
            let id = c.upsert_track("a.mp3", 1, 1, &TrackMeta::default())?;
            c.rebuild_genres()?;
            c.set_banned(id, true)
        })
        .unwrap();
        assert!(!reader.is_empty().unwrap());
        w.call(|c| c.clear()).unwrap();
        assert!(reader.is_empty().unwrap());
        assert!(reader.banned().unwrap().is_empty());
        assert!(reader.genres().unwrap().is_empty());
    }

    #[test]
    fn jobs_run_in_order_and_call_returns_results() {
        let w = CatalogWriter::spawn(Catalog::open_in_memory().unwrap()).unwrap();
        w.call(|c| {
            for p in ["1.mp3", "2.mp3"] {
                c.upsert_track(p, 1, 1, &smp_core::catalog::TrackMeta::default())?;
            }
            Ok(())
        })
        .unwrap();
        w.send("first", |c| c.set_last_first("k", 1));
        w.send("second", |c| c.set_last_first("k", 2));
        assert_eq!(w.call(|c| c.last_first("k")).unwrap(), Some(2));
    }
}
