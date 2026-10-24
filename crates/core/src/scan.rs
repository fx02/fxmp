//! Incremental scanner: walks a source and keeps the catalog in sync.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::catalog::{Catalog, TrackMeta};
use crate::source::{FileEntry, MusicSource};
use crate::tags::read_meta;
use crate::{Error, Result};

/// Files committed per catalog transaction.
const CHUNK: usize = 500;
/// Minimum time between progress callbacks.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub seen: usize,
    pub read: usize,
    pub failed: usize,
    pub current: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub total: usize,
    pub read: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub removed: usize,
}

/// Throttles progress callbacks to one per `PROGRESS_INTERVAL`.
struct Reporter<'a> {
    cb: &'a mut dyn FnMut(&ScanProgress),
    state: ScanProgress,
    last: Option<Instant>,
}

impl Reporter<'_> {
    fn tick(&mut self) {
        if self.last.map_or(true, |t| t.elapsed() >= PROGRESS_INTERVAL) {
            self.last = Some(Instant::now());
            (self.cb)(&self.state);
        }
    }
}

/// Syncs `catalog` with `source`: unchanged files are only marked seen, new or
/// modified ones are tagged and upserted, vanished ones are removed. A file whose
/// tags cannot be read stays in the catalog with empty metadata and counts as failed.
/// On cancel the walk stops and nothing is removed (`finish_scan` is skipped).
pub fn scan(
    source: &dyn MusicSource,
    catalog: &mut Catalog,
    progress: &mut dyn FnMut(&ScanProgress),
    cancel: &AtomicBool,
) -> Result<ScanReport> {
    catalog.begin_scan()?;

    let mut entries: Vec<FileEntry> = Vec::new();
    source.walk(&mut |e| {
        if !cancel.load(Ordering::Relaxed) {
            entries.push(e);
        }
    })?;

    let mut report = ScanReport { total: entries.len(), ..Default::default() };
    let mut rep = Reporter { cb: progress, state: ScanProgress::default(), last: None };

    'chunks: for chunk in entries.chunks(CHUNK) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        catalog.transaction(|c| {
            for e in chunk {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                rep.state.seen += 1;
                rep.state.current.clone_from(&e.rel_path);
                if c.file_unchanged(&e.rel_path, e.size, e.mtime)? {
                    c.mark_seen(&e.rel_path)?;
                    report.unchanged += 1;
                } else {
                    let ext = e.rel_path.rsplit_once('.').map_or("", |(_, x)| x);
                    let meta = source
                        .open(&e.rel_path)
                        .map_err(Error::from)
                        .and_then(|mut r| read_meta(&mut *r, ext));
                    let meta = meta.unwrap_or_else(|_| {
                        report.failed += 1;
                        rep.state.failed += 1;
                        TrackMeta::default()
                    });
                    c.upsert_track(&e.rel_path, e.size, e.mtime, &meta)?;
                    report.read += 1;
                    rep.state.read += 1;
                }
                rep.tick();
            }
            Ok(())
        })?;
        if cancel.load(Ordering::Relaxed) {
            break 'chunks;
        }
    }

    if cancel.load(Ordering::Relaxed) {
        (rep.cb)(&rep.state);
        return Ok(report);
    }
    report.removed = catalog.finish_scan()?;
    catalog.rebuild_genres()?;
    (rep.cb)(&rep.state);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::LocalSource;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// Tempdir holding copies of the four audio fixtures (not make.sh).
    fn music_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in ["tone.mp3", "tone.flac", "tone.ogg", "broken.mp3"] {
            fs::copy(fixtures().join(name), dir.path().join(name)).unwrap();
        }
        dir
    }

    fn run(dir: &Path, cat: &mut Catalog, cancel: &AtomicBool) -> ScanReport {
        let src = LocalSource::new(dir);
        scan(&src, cat, &mut |_| {}, cancel).unwrap()
    }

    #[test]
    fn first_scan_reads_all() {
        let dir = music_dir();
        let mut cat = Catalog::open_in_memory().unwrap();
        let r = run(dir.path(), &mut cat, &AtomicBool::new(false));
        assert_eq!(r.total, 4);
        assert_eq!(r.read, 4);
        assert_eq!(r.unchanged, 0);
        assert_eq!(r.removed, 0);
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 4);
        assert!(!cat.genres().unwrap().is_empty());
    }

    #[test]
    fn second_scan_reads_nothing_unchanged() {
        let dir = music_dir();
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        run(dir.path(), &mut cat, &no);
        let r = run(dir.path(), &mut cat, &no);
        assert_eq!(r.read, 0);
        assert_eq!(r.unchanged, 4);
        assert_eq!(r.total, 4);
    }

    #[test]
    fn modified_file_is_reread() {
        let dir = music_dir();
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        run(dir.path(), &mut cat, &no);
        let f = fs::OpenOptions::new().write(true).open(dir.path().join("tone.mp3")).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(3600)).unwrap();
        drop(f);
        let r = run(dir.path(), &mut cat, &no);
        assert_eq!(r.read, 1);
        assert_eq!(r.unchanged, 3);
    }

    #[test]
    fn deleted_file_removed() {
        let dir = music_dir();
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        run(dir.path(), &mut cat, &no);
        fs::remove_file(dir.path().join("tone.ogg")).unwrap();
        let r = run(dir.path(), &mut cat, &no);
        assert_eq!(r.removed, 1);
        assert_eq!(r.total, 3);
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 3);
    }

    #[test]
    fn broken_file_counted_not_fatal() {
        let dir = music_dir();
        let mut cat = Catalog::open_in_memory().unwrap();
        let r = run(dir.path(), &mut cat, &AtomicBool::new(false));
        assert_eq!(r.failed, 1);
        assert_eq!(r.total, 4);
        // still in the catalog
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 4);
    }

    #[test]
    fn cancel_keeps_catalog() {
        let dir = music_dir();
        let mut cat = Catalog::open_in_memory().unwrap();
        run(dir.path(), &mut cat, &AtomicBool::new(false));
        fs::remove_file(dir.path().join("tone.ogg")).unwrap();
        let r = run(dir.path(), &mut cat, &AtomicBool::new(true));
        assert_eq!(r.removed, 0);
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 4);
    }
}
