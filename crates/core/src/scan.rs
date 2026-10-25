//! Incremental scanner: walks a source and keeps the catalog in sync.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::catalog::{Catalog, TrackMeta};
use crate::failure::{FailCode, Failure};
use crate::source::{FileEntry, MusicSource};
use crate::tags::read_meta;
use crate::{Error, LibraryId, Result, SetOnDrop};

/// Files committed per catalog transaction.
const CHUNK: usize = 500;
/// Minimum time between progress callbacks.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Longest wait for tagged files before committing what has arrived.
const BATCH_WINDOW: Duration = Duration::from_millis(100);

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
    /// Folders that could not be listed; their tracks were kept as they were.
    pub skipped_dirs: Vec<String>,
    pub read: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub removed: usize,
    /// Listed by the backup server (the primary was down): nothing was removed,
    /// since the backup may lag behind; removals wait for a scan from the primary.
    pub from_backup: bool,
    /// The listing found no file although the library has tracks (an empty
    /// mount point where a disk used to be, say): nothing was removed.
    pub looks_empty: bool,
}

/// Throttles progress callbacks to one per `PROGRESS_INTERVAL`.
struct Reporter<'a> {
    cb: &'a mut dyn FnMut(&ScanProgress),
    state: ScanProgress,
    last: Option<Instant>,
}

impl Reporter<'_> {
    fn tick(&mut self) {
        if self.last.is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL) {
            self.last = Some(Instant::now());
            (self.cb)(&self.state);
        }
    }
}

/// How [`scan_with`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Threads opening and tagging new or modified files at once (at least 1).
    pub workers: usize,
    /// Rebuild genres after this many newly committed files, so a long first
    /// scan becomes playable before it ends (`usize::MAX` = only at the end).
    pub rebuild_every: usize,
    /// Also rebuild once this much time has passed since the last rebuild and
    /// something new was committed, so genres appear early even on a slow share.
    pub rebuild_interval: Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            workers: 8,
            rebuild_every: 5000,
            rebuild_interval: Duration::from_secs(30),
        }
    }
}

/// [`scan_with`] using [`ScanOptions::default`].
pub fn scan(
    library: LibraryId,
    source: &dyn MusicSource,
    catalog: &mut Catalog,
    progress: &mut dyn FnMut(&ScanProgress),
    cancel: &AtomicBool,
) -> Result<ScanReport> {
    scan_with(
        library,
        source,
        catalog,
        progress,
        cancel,
        &ScanOptions::default(),
    )
}

/// Syncs `library`'s tracks in `catalog` with `source` (other libraries are not
/// touched): unchanged files are only marked seen, new or modified ones (and those
/// marked for a new tag read) are tagged and upserted, vanished ones are removed,
/// unless a backup server did the listing ([`ScanReport::from_backup`]). A file whose tags cannot be read counts as failed and
/// is read again by the next scan; a known track keeps everything it had, a new
/// one is stored with empty metadata (see [`Catalog::record_scan_failure`]).
/// Tagging runs on `opts.workers` threads; only the calling thread writes to the catalog.
/// On cancel the walk stops and nothing is removed (`finish_scan` is skipped).
pub fn scan_with(
    library: LibraryId,
    source: &dyn MusicSource,
    catalog: &mut Catalog,
    progress: &mut dyn FnMut(&ScanProgress),
    cancel: &AtomicBool,
    opts: &ScanOptions,
) -> Result<ScanReport> {
    // Walk first: tracks are only marked unseen once the listing has succeeded.
    let mut entries: Vec<FileEntry> = Vec::new();
    let walk = source.walk(&mut |e| {
        if !cancel.load(Ordering::Relaxed) {
            entries.push(e);
        }
    })?;
    if !walk.skipped_dirs.is_empty() {
        log::warn!(
            "scan: {} folder(s) could not be listed; their tracks are kept: {:?}",
            walk.skipped_dirs.len(),
            walk.skipped_dirs
        );
    }
    if walk.from_backup {
        log::warn!("scan: listed by the backup server; nothing is removed this time");
    }
    let looks_empty = entries.is_empty()
        && !cancel.load(Ordering::Relaxed)
        && catalog
            .library_track_counts()?
            .get(&library)
            .copied()
            .unwrap_or(0)
            > 0;
    if looks_empty {
        log::warn!("scan: library {library} lists no file but has tracks; nothing is removed");
    }
    let report = ScanReport {
        total: entries.len(),
        skipped_dirs: walk.skipped_dirs,
        from_backup: walk.from_backup,
        looks_empty,
        ..Default::default()
    };
    if cancel.load(Ordering::Relaxed) {
        progress(&ScanProgress::default());
        return Ok(report);
    }
    catalog.begin_scan(library)?;
    let result = sync_catalog(
        library, source, catalog, &entries, report, progress, cancel, opts,
    );
    // Stopped early: nothing may stay marked unseen.
    if result.is_err() || cancel.load(Ordering::Relaxed) {
        if let Err(e) = catalog.abort_scan(library) {
            log::warn!("scan: cannot restore the tracks after stopping: {e}");
        }
    }
    result
}

/// The part of [`scan_with`] after the walk: marks, tags and upserts `entries`,
/// then sweeps what was not seen (unless cancelled).
#[allow(clippy::too_many_arguments)]
fn sync_catalog(
    library: LibraryId,
    source: &dyn MusicSource,
    catalog: &mut Catalog,
    entries: &[FileEntry],
    mut report: ScanReport,
    progress: &mut dyn FnMut(&ScanProgress),
    cancel: &AtomicBool,
    opts: &ScanOptions,
) -> Result<ScanReport> {
    let mut rep = Reporter {
        cb: progress,
        state: ScanProgress::default(),
        last: None,
    };

    // Unchanged files only need marking; the rest go to the workers.
    let mut to_read = Vec::new();
    'chunks: for chunk in entries.chunks(CHUNK) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        catalog.transaction(|c| {
            for e in chunk {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if c.file_unchanged(library, &e.rel_path, e.size, e.mtime)? {
                    c.mark_seen(library, &e.rel_path)?;
                    report.unchanged += 1;
                    rep.state.seen += 1;
                    rep.state.current.clone_from(&e.rel_path);
                    rep.tick();
                } else {
                    to_read.push(e);
                }
            }
            Ok(())
        })?;
        if cancel.load(Ordering::Relaxed) {
            break 'chunks;
        }
    }

    if !cancel.load(Ordering::Relaxed) {
        read_parallel(
            library,
            source,
            catalog,
            &to_read,
            opts,
            cancel,
            |e, failed| {
                report.read += 1;
                rep.state.read += 1;
                if failed {
                    report.failed += 1;
                    rep.state.failed += 1;
                }
                rep.state.seen += 1;
                rep.state.current.clone_from(&e.rel_path);
                rep.tick();
            },
        )?;
    }

    if cancel.load(Ordering::Relaxed) {
        (rep.cb)(&rep.state);
        return Ok(report);
    }
    // A backup's listing, or an empty one of a library with tracks, removes
    // nothing: the whole library counts as unseen-but-kept.
    let keep_under = if report.from_backup || report.looks_empty {
        vec![String::new()]
    } else {
        report.skipped_dirs.clone()
    };
    report.removed = catalog.finish_scan(library, &keep_under)?;
    catalog.rebuild_genres()?;
    (rep.cb)(&rep.state);
    Ok(report)
}

/// Outcome of opening and tagging one file.
struct ReadResult {
    index: usize,
    meta: std::result::Result<TrackMeta, Failure>,
}

/// Opens and tags `entry`, classifying failures (see [`scan_with`]). A panic
/// while doing so (a tag-parser bug, say) counts as a tag failure.
fn read_one(source: &dyn MusicSource, index: usize, e: &FileEntry) -> ReadResult {
    let ext = e.rel_path.rsplit_once('.').map_or("", |(_, x)| x);
    let read = || match source.open(&e.rel_path) {
        Ok(mut r) => read_meta(&mut *r, ext).map_err(|err| Failure::from_tag_error(&err)),
        Err(err) => Err(Failure::from_tag_error(&Error::Source(err))),
    };
    let meta = std::panic::catch_unwind(AssertUnwindSafe(read)).unwrap_or_else(|_| {
        log::warn!("scan: reading tags of '{}' panicked", e.rel_path);
        Err(Failure::new(FailCode::Tag, "reading the tags panicked"))
    });
    ReadResult { index, meta }
}

/// Tags `files` on `opts.workers` threads and upserts the results from the
/// calling thread in transactions of at most [`CHUNK`] files (whatever arrived
/// within [`BATCH_WINDOW`]), rebuilding genres after every
/// `opts.rebuild_every` committed files or `opts.rebuild_interval`, whichever
/// comes first, and on cancel. `on_written(entry, failed)` runs after
/// each upsert. Stops early (keeping what was written) when `cancel` is set.
fn read_parallel(
    library: LibraryId,
    source: &dyn MusicSource,
    catalog: &mut Catalog,
    files: &[&FileEntry],
    opts: &ScanOptions,
    cancel: &AtomicBool,
    mut on_written: impl FnMut(&FileEntry, bool),
) -> Result<()> {
    let next = AtomicUsize::new(0);
    // Set when the writer gives up, so workers stop picking new files.
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<ReadResult>();
    std::thread::scope(|s| {
        for _ in 0..opts.workers.max(1).min(files.len()) {
            let (tx, next, stop) = (tx.clone(), &next, &stop);
            s.spawn(move || {
                while !cancel.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(e) = files.get(i) else { break };
                    if tx.send(read_one(source, i, e)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        // On return or panic, workers finish the file in hand and exit.
        let _stop_on_exit = SetOnDrop(&stop);

        let mut finished = false;
        let mut since_rebuild = 0;
        let mut last_rebuild = Instant::now();
        let mut batch: Vec<ReadResult> = Vec::with_capacity(CHUNK);
        while !finished && !cancel.load(Ordering::Relaxed) {
            // Collect results first: a write transaction is never held while
            // waiting on the network, so other connections can write meanwhile.
            let until = Instant::now() + BATCH_WINDOW;
            while batch.len() < CHUNK && !cancel.load(Ordering::Relaxed) {
                match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                    Ok(r) => batch.push(r),
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
            if batch.is_empty() {
                continue;
            }
            let written = batch.len();
            catalog.transaction(|c| {
                for r in batch.drain(..) {
                    let e = files[r.index];
                    let failed = r.meta.is_err();
                    // A file failing because its disk went away is not the file's
                    // fault: the scan of this library stops, keeping it as it was.
                    if matches!(&r.meta, Err(f) if !f.code.is_transient()) && !source.available() {
                        log::warn!("scan: library {library} went away mid-scan; stopping");
                        return Err(Error::Source(crate::source::SourceError::Unreachable));
                    }
                    match &r.meta {
                        Ok(meta) => c.upsert_track(library, &e.rel_path, e.size, e.mtime, meta)?,
                        Err(f) => {
                            c.record_scan_failure(library, &e.rel_path, e.size, e.mtime, f)?
                        }
                    };
                    on_written(e, failed);
                }
                Ok(())
            })?;
            // Keep a long first scan playable. Not needed once all files are
            // in: the caller rebuilds after `finish_scan`. Never deletes tracks.
            since_rebuild += written;
            let due = since_rebuild >= opts.rebuild_every
                || last_rebuild.elapsed() >= opts.rebuild_interval;
            if due && !finished {
                catalog.rebuild_genres()?;
                since_rebuild = 0;
                last_rebuild = Instant::now();
            }
        }
        // A cancelled scan returns without the final rebuild: make what it
        // wrote playable by genre.
        if cancel.load(Ordering::Relaxed) && since_rebuild > 0 {
            catalog.rebuild_genres()?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{LocalSource, ReadSeek, SourceError, WalkReport};
    use crate::DEFAULT_LIBRARY;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicI64;
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
        scan(DEFAULT_LIBRARY, &src, cat, &mut |_| {}, cancel).unwrap()
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
        let f = fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("tone.mp3"))
            .unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
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

    /// How a [`FlakySource`] file behaves.
    #[derive(Clone)]
    enum Behaviour {
        OpenFails(SourceError),
        /// Opens, but every read fails with this kind (as SmbReader's `to_io` produces).
        ReadFails(std::io::ErrorKind),
        /// Reads fail as SmbReader's do: `kind` wrapping the source error.
        ReadFailsWith(std::io::ErrorKind, SourceError),
        Good(Vec<u8>),
        Garbage,
        /// Opens, but the first read panics (a bug in a tag parser, say).
        ReadPanics,
    }

    /// Reader that panics when read.
    struct PanickingReader;

    impl std::io::Read for PanickingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("simulated parser bug");
        }
    }

    impl std::io::Seek for PanickingReader {
        fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    /// Reader whose reads always fail.
    struct FailingReader(std::io::ErrorKind);

    impl std::io::Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "simulated network failure"))
        }
    }

    impl std::io::Seek for FailingReader {
        fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    /// Reader whose reads fail with a source error inside an I/O error.
    struct WrappedFailingReader(std::io::ErrorKind, SourceError);

    impl std::io::Read for WrappedFailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, self.1.clone()))
        }
    }

    impl std::io::Seek for WrappedFailingReader {
        fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    /// Source whose files fail in a per-file way.
    struct FlakySource(Vec<(FileEntry, Behaviour)>);

    impl MusicSource for FlakySource {
        fn walk(
            &self,
            on_entry: &mut dyn FnMut(FileEntry),
        ) -> std::result::Result<WalkReport, SourceError> {
            self.0.iter().for_each(|(e, _)| on_entry(e.clone()));
            Ok(WalkReport::default())
        }
        fn open(&self, rel_path: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
            let (_, b) = self.0.iter().find(|(e, _)| e.rel_path == rel_path).unwrap();
            match b.clone() {
                Behaviour::OpenFails(err) => Err(err),
                Behaviour::ReadFails(kind) => Ok(Box::new(FailingReader(kind))),
                Behaviour::ReadFailsWith(kind, e) => Ok(Box::new(WrappedFailingReader(kind, e))),
                Behaviour::Good(bytes) => Ok(Box::new(std::io::Cursor::new(bytes))),
                Behaviour::Garbage => Ok(Box::new(std::io::Cursor::new(b"not audio".to_vec()))),
                Behaviour::ReadPanics => Ok(Box::new(PanickingReader)),
            }
        }
        fn describe(&self) -> String {
            "flaky".into()
        }
    }

    #[test]
    fn network_failures_are_retried_next_scan() {
        use std::io::ErrorKind;
        let entry = |name: &str| FileEntry {
            rel_path: name.into(),
            size: 1000,
            mtime: 1_700_000_000,
        };
        let retried = [
            ("timeout.mp3", Behaviour::OpenFails(SourceError::Timeout)),
            (
                "unreachable.mp3",
                Behaviour::OpenFails(SourceError::Unreachable),
            ),
            (
                "io.mp3",
                Behaviour::OpenFails(SourceError::Io("reset".into())),
            ),
            (
                "read-timeout.mp3",
                Behaviour::ReadFails(ErrorKind::TimedOut),
            ),
            (
                "read-dropped.flac",
                Behaviour::ReadFails(ErrorKind::NotConnected),
            ),
            (
                "read-reset.ogg",
                Behaviour::ReadFails(ErrorKind::ConnectionReset),
            ),
        ];
        let kept = [
            ("garbage.mp3", Behaviour::Garbage),
            ("missing.mp3", Behaviour::OpenFails(SourceError::NotFound)),
        ];
        let src = FlakySource(
            retried
                .iter()
                .chain(&kept)
                .map(|(n, b)| (entry(n), b.clone()))
                .collect(),
        );
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        let first = scan(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no).unwrap();
        assert_eq!((first.read, first.failed), (8, 8));
        // Network trouble (and a file that went missing) is retried by the next
        // scan; a broken new file waits until it changes. Size and time are the
        // real ones either way.
        for (name, _) in retried.iter().chain(&kept[1..]) {
            assert!(
                !cat.file_unchanged(DEFAULT_LIBRARY, name, 1000, 1_700_000_000)
                    .unwrap(),
                "{name}"
            );
        }
        assert!(cat
            .file_unchanged(DEFAULT_LIBRARY, "garbage.mp3", 1000, 1_700_000_000)
            .unwrap());
        let codes: std::collections::BTreeMap<String, String> = cat
            .unreadable()
            .unwrap()
            .into_iter()
            .map(|t| (t.rel_path, t.code.unwrap()))
            .collect();
        // Network trouble is not a file problem: not listed.
        let want = [
            ("io.mp3", "io"),
            ("missing.mp3", "not_found"),
            ("garbage.mp3", "tag"),
        ];
        assert_eq!(
            codes,
            want.iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect()
        );
        let second = scan(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no).unwrap();
        assert_eq!((second.read, second.unchanged), (7, 1));
    }

    #[test]
    fn a_failing_re_read_keeps_tags_and_loudness_and_retries() {
        use std::io::ErrorKind;
        let tone = fs::read(fixtures().join("tone.mp3")).unwrap();
        let names = [
            "auth.mp3",
            "io-open.mp3",
            "io-read.mp3",
            "missing.mp3",
            "garbage.mp3",
            "timeout.mp3",
        ];
        let entry = |name: &str| FileEntry {
            rel_path: name.into(),
            size: tone.len() as u64,
            mtime: 1_700_000_000,
        };
        let good = FlakySource(
            names
                .iter()
                .map(|n| (entry(n), Behaviour::Good(tone.clone())))
                .collect(),
        );
        let failing = FlakySource(vec![
            (entry("auth.mp3"), Behaviour::OpenFails(SourceError::Auth)),
            (
                entry("io-open.mp3"),
                Behaviour::OpenFails(SourceError::Io("boom".into())),
            ),
            (
                entry("io-read.mp3"),
                Behaviour::ReadFailsWith(ErrorKind::Other, SourceError::Io("boom".into())),
            ),
            (
                entry("missing.mp3"),
                Behaviour::OpenFails(SourceError::NotFound),
            ),
            (entry("garbage.mp3"), Behaviour::Garbage),
            (
                entry("timeout.mp3"),
                Behaviour::OpenFails(SourceError::Timeout),
            ),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("catalog.db");
        let mut cat = Catalog::open(&db).unwrap();
        let no = AtomicBool::new(false);
        scan(DEFAULT_LIBRARY, &good, &mut cat, &mut |_| {}, &no).unwrap();
        let ids = cat.playable_tracks(&[]).unwrap();
        for &id in &ids {
            cat.set_measured_lufs(id, -10.0).unwrap();
        }
        let before: Vec<_> = ids.iter().map(|&id| cat.track(id).unwrap()).collect();
        assert!(before.iter().all(|t| t.meta.title.is_some()));
        // The migration to version 4 marks every track for a new read.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("UPDATE tracks SET needs_retag=1", [])
            .unwrap();

        let r = scan(DEFAULT_LIBRARY, &failing, &mut cat, &mut |_| {}, &no).unwrap();
        assert_eq!((r.read, r.failed, r.removed), (6, 6, 0));
        for t in &before {
            assert_eq!(&cat.track(t.id).unwrap(), t, "kept: {}", t.rel_path);
            let e = entry(&t.rel_path);
            // A broken file (unreadable tags) waits until it changes.
            let retried = t.rel_path != "garbage.mp3";
            assert_eq!(
                cat.file_unchanged(DEFAULT_LIBRARY, &e.rel_path, e.size, e.mtime)
                    .unwrap(),
                !retried,
                "{}",
                t.rel_path
            );
        }
        let codes: std::collections::BTreeMap<String, String> = cat
            .unreadable()
            .unwrap()
            .into_iter()
            .map(|t| (t.rel_path, t.code.unwrap()))
            .collect();
        let want = [
            ("garbage.mp3", "tag"),
            // Opening or reading: the same code for the same source error.
            ("io-open.mp3", "io"),
            ("io-read.mp3", "io"),
            ("missing.mp3", "not_found"),
        ];
        assert_eq!(
            codes,
            want.iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect()
        );

        // The share works again: the retried files are read once, loudness kept,
        // and only the broken one is still listed.
        let r = scan(DEFAULT_LIBRARY, &good, &mut cat, &mut |_| {}, &no).unwrap();
        assert_eq!((r.read, r.failed, r.unchanged), (5, 0, 1));
        for t in &before {
            assert_eq!(cat.track(t.id).unwrap().measured_lufs, Some(-10.0));
        }
        let listed: Vec<_> = cat
            .unreadable()
            .unwrap()
            .into_iter()
            .map(|t| t.rel_path)
            .collect();
        assert_eq!(listed, ["garbage.mp3"]);
        let r = scan(DEFAULT_LIBRARY, &good, &mut cat, &mut |_| {}, &no).unwrap();
        assert_eq!((r.read, r.unchanged), (0, 6));
    }

    #[test]
    fn tracks_marked_for_retag_are_read_again_keeping_bans_and_loudness() {
        let dir = music_dir();
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("catalog.db");
        let mut cat = Catalog::open(&db).unwrap();
        let no = AtomicBool::new(false);
        run(dir.path(), &mut cat, &no);
        let ids = cat.playable_tracks(&[]).unwrap();
        cat.set_banned(ids[0], true).unwrap();
        cat.set_measured_lufs(ids[1], -12.0).unwrap();
        // What the migration to catalog version 4 does.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute("UPDATE tracks SET needs_retag=1", [])
            .unwrap();
        let r = run(dir.path(), &mut cat, &no);
        assert_eq!((r.read, r.unchanged, r.removed), (4, 0, 0));
        assert_eq!(cat.banned().unwrap().len(), 1);
        assert_eq!(cat.track(ids[1]).unwrap().measured_lufs, Some(-12.0));
        let r = run(dir.path(), &mut cat, &no);
        assert_eq!((r.read, r.unchanged), (0, 4), "read once");
    }

    /// Tempdir with `copies` copies of the four fixtures spread over nested folders.
    fn nested_music_dir(copies: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..copies {
            let sub = dir.path().join(format!("g{}/album{i}", i % 5));
            fs::create_dir_all(&sub).unwrap();
            for name in ["tone.mp3", "tone.flac", "tone.ogg", "broken.mp3"] {
                fs::copy(fixtures().join(name), sub.join(name)).unwrap();
            }
        }
        dir
    }

    fn scan_workers(src: &dyn MusicSource, cat: &mut Catalog, workers: usize) -> ScanReport {
        let opts = ScanOptions {
            workers,
            ..Default::default()
        };
        scan_with(
            DEFAULT_LIBRARY,
            src,
            cat,
            &mut |_| {},
            &AtomicBool::new(false),
            &opts,
        )
        .unwrap()
    }

    /// Track rows keyed by path, without the insertion-order-dependent id.
    fn rows_by_path(cat: &Catalog) -> std::collections::BTreeMap<String, (TrackMeta, Option<f32>)> {
        cat.playable_tracks(&[])
            .unwrap()
            .into_iter()
            .map(|id| {
                let r = cat.track(id).unwrap();
                (r.rel_path, (r.meta, r.measured_lufs))
            })
            .collect()
    }

    #[test]
    fn default_options_use_eight_workers() {
        assert_eq!(ScanOptions::default().workers, 8);
    }

    #[test]
    fn parallel_scan_matches_sequential() {
        let dir = nested_music_dir(60);
        let src = LocalSource::new(dir.path());
        let (mut a, mut b) = (
            Catalog::open_in_memory().unwrap(),
            Catalog::open_in_memory().unwrap(),
        );
        let ra = scan_workers(&src, &mut a, 1);
        let rb = scan_workers(&src, &mut b, 8);
        assert_eq!(ra, rb);
        assert_eq!((ra.total, ra.read, ra.failed), (240, 240, 60));
        assert_eq!(rows_by_path(&a), rows_by_path(&b));
        assert_eq!(rows_by_path(&a).len(), 240);
        let mut entries = Vec::new();
        src.walk(&mut |e| entries.push(e)).unwrap();
        for e in &entries {
            assert!(
                a.file_unchanged(DEFAULT_LIBRARY, &e.rel_path, e.size, e.mtime)
                    .unwrap(),
                "{}",
                e.rel_path
            );
            assert!(
                b.file_unchanged(DEFAULT_LIBRARY, &e.rel_path, e.size, e.mtime)
                    .unwrap(),
                "{}",
                e.rel_path
            );
        }
        assert_eq!(a.genres().unwrap(), b.genres().unwrap());
        assert!(!a.genres().unwrap().is_empty());
    }

    /// Source of `files` fixture-tone MP3s; `open` sleeps `delay` and records
    /// the peak number of concurrent opens. Only the first `visible` files are
    /// listed, all with modification time `mtime`.
    struct SlowSource {
        files: usize,
        visible: AtomicUsize,
        mtime: AtomicI64,
        delay: Duration,
        bytes: Vec<u8>,
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    impl SlowSource {
        fn new(files: usize, delay: Duration) -> Self {
            Self {
                files,
                visible: AtomicUsize::new(files),
                mtime: AtomicI64::new(1_700_000_000),
                delay,
                bytes: fs::read(fixtures().join("tone.mp3")).unwrap(),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }
    }

    impl MusicSource for SlowSource {
        fn walk(
            &self,
            on_entry: &mut dyn FnMut(FileEntry),
        ) -> std::result::Result<WalkReport, SourceError> {
            let n = self.visible.load(Ordering::SeqCst).min(self.files);
            for i in 0..n {
                on_entry(FileEntry {
                    rel_path: format!("d{}/f{i}.mp3", i % 4),
                    size: self.bytes.len() as u64,
                    mtime: self.mtime.load(Ordering::SeqCst),
                });
            }
            Ok(WalkReport::default())
        }
        fn open(&self, _: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
        fn describe(&self) -> String {
            "slow".into()
        }
    }

    #[test]
    fn parallel_scan_uses_multiple_workers() {
        let src = SlowSource::new(40, Duration::from_millis(50));
        let mut cat = Catalog::open_in_memory().unwrap();
        let started = Instant::now();
        let r = scan_workers(&src, &mut cat, 8);
        let elapsed = started.elapsed();
        assert_eq!((r.read, r.failed), (40, 0));
        let peak = src.peak.load(Ordering::SeqCst);
        assert!(peak >= 4, "peak concurrency {peak}");
        assert!(
            elapsed < Duration::from_millis(40 * 50 / 3),
            "took {elapsed:?}"
        );
    }

    #[test]
    fn panicking_reader_counts_as_decode_failure() {
        let entry = |name: &str| FileEntry {
            rel_path: name.into(),
            size: 1000,
            mtime: 1_700_000_000,
        };
        let src = FlakySource(vec![
            (entry("panics.mp3"), Behaviour::ReadPanics),
            (entry("garbage.mp3"), Behaviour::Garbage),
        ]);
        for workers in [1, 8] {
            let mut cat = Catalog::open_in_memory().unwrap();
            let r = scan_workers(&src, &mut cat, workers);
            assert_eq!((r.read, r.failed), (2, 2));
            // Real size/mtime kept: not retried until the file changes.
            assert!(cat
                .file_unchanged(DEFAULT_LIBRARY, "panics.mp3", 1000, 1_700_000_000)
                .unwrap());
            assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 2);
            let codes: Vec<_> = cat
                .unreadable()
                .unwrap()
                .into_iter()
                .map(|t| t.code.unwrap())
                .collect();
            assert_eq!(codes, ["tag", "tag"]);
        }
    }

    /// In-memory source of `files` tone MP3s that sets `cancel` when file
    /// number `cancel_after` is opened.
    struct CancellingSource<'a> {
        files: usize,
        cancel_after: usize,
        opens: AtomicUsize,
        cancel: &'a AtomicBool,
        bytes: Vec<u8>,
    }

    impl MusicSource for CancellingSource<'_> {
        fn walk(
            &self,
            on_entry: &mut dyn FnMut(FileEntry),
        ) -> std::result::Result<WalkReport, SourceError> {
            for i in 0..self.files {
                on_entry(FileEntry {
                    rel_path: format!("Rock/f{i}.mp3"),
                    size: self.bytes.len() as u64,
                    mtime: 1_700_000_000,
                });
            }
            Ok(WalkReport::default())
        }
        fn open(&self, _: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
            if self.opens.fetch_add(1, Ordering::SeqCst) + 1 == self.cancel_after {
                // Workers run ahead of the writer; let it commit what was read.
                std::thread::sleep(Duration::from_millis(500));
                self.cancel.store(true, Ordering::SeqCst);
            }
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
        fn describe(&self) -> String {
            "cancelling".into()
        }
    }

    /// First scan of 1200 files cancelled after ~700 opens (no timed rebuild).
    fn cancelled_first_scan_genres(rebuild_every: usize) -> Vec<crate::catalog::GenreCount> {
        let cancel = AtomicBool::new(false);
        let src = CancellingSource {
            files: 1200,
            cancel_after: 700,
            opens: AtomicUsize::new(0),
            cancel: &cancel,
            bytes: fs::read(fixtures().join("tone.mp3")).unwrap(),
        };
        let mut cat = Catalog::open_in_memory().unwrap();
        let opts = ScanOptions {
            workers: 1,
            rebuild_every,
            rebuild_interval: Duration::from_secs(3600),
        };
        let r = scan_with(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &cancel, &opts).unwrap();
        assert!(r.read >= 500 && r.read < 1200, "{r:?}");
        assert_eq!(r.removed, 0);
        cat.genres().unwrap()
    }

    #[test]
    fn genres_rebuilt_periodically_during_first_scan() {
        assert_eq!(ScanOptions::default().rebuild_every, 5000);
        assert!(!cancelled_first_scan_genres(500).is_empty());
    }

    #[test]
    fn cancelled_scan_rebuilds_genres_of_what_it_wrote() {
        // No periodic rebuild at all: the cancel itself rebuilds.
        assert!(!cancelled_first_scan_genres(usize::MAX).is_empty());
    }

    /// Source of `files` tone MP3s; when file number `probe_at` is opened it waits
    /// for the writer to commit, then counts genres through its own connection
    /// to the catalog file `db`, as the UI would see them mid-scan.
    struct ProbeSource {
        files: usize,
        probe_at: usize,
        opens: AtomicUsize,
        db: PathBuf,
        genres_seen: std::sync::Mutex<Option<usize>>,
        bytes: Vec<u8>,
    }

    impl MusicSource for ProbeSource {
        fn walk(
            &self,
            on_entry: &mut dyn FnMut(FileEntry),
        ) -> std::result::Result<WalkReport, SourceError> {
            for i in 0..self.files {
                on_entry(FileEntry {
                    rel_path: format!("Rock/f{i}.mp3"),
                    size: self.bytes.len() as u64,
                    mtime: 1_700_000_000,
                });
            }
            Ok(WalkReport::default())
        }
        fn open(&self, _: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
            if self.opens.fetch_add(1, Ordering::SeqCst) + 1 == self.probe_at {
                std::thread::sleep(Duration::from_millis(500));
                let n = Catalog::open(&self.db).unwrap().genres().unwrap().len();
                *self.genres_seen.lock().unwrap() = Some(n);
            }
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
        fn describe(&self) -> String {
            "probe".into()
        }
    }

    /// Genres visible mid-scan (at file 50 of 100) with only the time trigger.
    fn genres_mid_scan(rebuild_interval: Duration) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        let mut cat = Catalog::open(&db).unwrap();
        let src = ProbeSource {
            files: 100,
            probe_at: 50,
            opens: AtomicUsize::new(0),
            db,
            genres_seen: std::sync::Mutex::new(None),
            bytes: fs::read(fixtures().join("tone.mp3")).unwrap(),
        };
        let opts = ScanOptions {
            workers: 1,
            rebuild_every: usize::MAX,
            rebuild_interval,
        };
        let no = AtomicBool::new(false);
        let r = scan_with(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no, &opts).unwrap();
        assert_eq!(r.read, 100);
        let seen = src.genres_seen.lock().unwrap().expect("probe ran");
        seen
    }

    #[test]
    fn genres_rebuilt_on_a_timer_during_scan() {
        assert_eq!(
            ScanOptions::default().rebuild_interval,
            Duration::from_secs(30)
        );
        assert!(genres_mid_scan(Duration::ZERO) > 0);
        // Control: neither trigger due, nothing rebuilt before the end.
        assert_eq!(genres_mid_scan(Duration::from_secs(3600)), 0);
    }

    #[test]
    fn single_worker_opens_one_at_a_time() {
        let src = SlowSource::new(10, Duration::from_millis(5));
        let mut cat = Catalog::open_in_memory().unwrap();
        let r = scan_workers(&src, &mut cat, 1);
        assert_eq!(r.read, 10);
        assert_eq!(src.peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancel_mid_scan_keeps_catalog() {
        // Slow enough that the 30 rescanned files do not all arrive within one
        // BATCH_WINDOW: progress is only reported after a batch is written.
        let src = SlowSource::new(40, Duration::from_millis(60));
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        let full = scan(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no).unwrap();
        assert_eq!(full.read, 40);

        // Rescan where every listed file changed and ten files vanished; cancel
        // from the first progress report.
        src.mtime.store(1_800_000_000, Ordering::SeqCst);
        src.visible.store(30, Ordering::SeqCst);
        let cancel = AtomicBool::new(false);
        let r = scan(
            DEFAULT_LIBRARY,
            &src,
            &mut cat,
            &mut |p| {
                if p.seen > 0 {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .unwrap();
        assert!(r.read < 30, "{r:?}");
        assert_eq!(r.removed, 0);
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 40);
    }

    /// Music folder with `Rock/` and `Jazz/` (two copies of tone.mp3 each).
    fn two_folders() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Rock", "Jazz"] {
            fs::create_dir(dir.path().join(d)).unwrap();
            for f in ["1.mp3", "2.mp3"] {
                fs::copy(fixtures().join("tone.mp3"), dir.path().join(d).join(f)).unwrap();
            }
        }
        dir
    }

    #[test]
    #[cfg(unix)] // folder permissions
    fn an_unreadable_folder_keeps_its_tracks_bans_and_loudness() {
        use std::os::unix::fs::PermissionsExt;
        let dir = two_folders();
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        run(dir.path(), &mut cat, &no);
        let by_path = |cat: &Catalog, p: &str| {
            cat.playable_tracks(&[])
                .unwrap()
                .into_iter()
                .chain(cat.banned().unwrap().into_iter().map(|t| t.id))
                .find(|&id| cat.track(id).unwrap().rel_path == p)
        };
        let j1 = by_path(&cat, "Jazz/1.mp3").unwrap();
        let j2 = by_path(&cat, "Jazz/2.mp3").unwrap();
        cat.set_banned(j1, true).unwrap();
        cat.set_measured_lufs(j2, -14.0).unwrap();
        fs::remove_file(dir.path().join("Rock/2.mp3")).unwrap();
        let jazz = dir.path().join("Jazz");
        fs::set_permissions(&jazz, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&jazz).is_ok() {
            fs::set_permissions(&jazz, fs::Permissions::from_mode(0o755)).unwrap();
            return; // running as root: cannot make a folder unreadable
        }
        let r = run(dir.path(), &mut cat, &no);
        fs::set_permissions(&jazz, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.skipped_dirs, ["Jazz"]);
        assert_eq!(r.removed, 1, "Rock/2.mp3 is gone; Jazz is not swept");
        assert_eq!(cat.banned().unwrap()[0].id, j1);
        assert_eq!(cat.track(j2).unwrap().measured_lufs, Some(-14.0));
        assert!(by_path(&cat, "Rock/2.mp3").is_none());
        assert_eq!(cat.absent_count(), 0);
    }

    #[test]
    fn a_failed_walk_leaves_no_track_hidden() {
        let dir = two_folders();
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        run(dir.path(), &mut cat, &no);
        let src = LocalSource::new(dir.path().join("no-such-root"));
        assert!(scan(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no).is_err());
        assert_eq!(cat.absent_count(), 0);
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 4);
    }

    #[test]
    fn a_cancelled_scan_leaves_no_track_hidden() {
        // Cancelled while reading (after the walk): see cancel_mid_scan_keeps_catalog.
        let src = SlowSource::new(40, Duration::from_millis(30));
        let mut cat = Catalog::open_in_memory().unwrap();
        let no = AtomicBool::new(false);
        scan(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no).unwrap();
        src.mtime.store(1_800_000_000, Ordering::SeqCst);
        let cancel = AtomicBool::new(false);
        scan(
            DEFAULT_LIBRARY,
            &src,
            &mut cat,
            &mut |p| {
                if p.seen > 0 {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .unwrap();
        assert!(cancel.load(Ordering::SeqCst));
        assert_eq!(cat.absent_count(), 0);
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

    #[test]
    fn scan_never_holds_write_lock_while_waiting_for_reads() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.db");
        let mut cat = Catalog::open(&db).unwrap();
        let other = Catalog::open(&db).unwrap();
        let src = SlowSource::new(8, Duration::from_millis(150));
        let opts = ScanOptions {
            workers: 1,
            rebuild_every: usize::MAX,
            rebuild_interval: Duration::from_secs(3600),
        };
        let no = AtomicBool::new(false);
        std::thread::scope(|s| {
            let scan = s.spawn(|| {
                scan_with(DEFAULT_LIBRARY, &src, &mut cat, &mut |_| {}, &no, &opts).unwrap()
            });
            let mut worst = Duration::ZERO;
            while !scan.is_finished() {
                let t = Instant::now();
                other.set_last_first("probe", 1).unwrap();
                worst = worst.max(t.elapsed());
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(scan.join().unwrap().read, 8);
            assert!(
                worst < Duration::from_millis(100),
                "a write waited {worst:?}"
            );
        });
    }

    /// A temp dir with `tone.mp3` copied to each of `paths` and their genre in the
    /// first folder name (the folder fallback).
    fn library_dir(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let to = dir.path().join(p);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(fixtures().join("tone.mp3"), to).unwrap();
        }
        dir
    }

    fn scan_lib(
        lib: crate::LibraryId,
        src: &dyn MusicSource,
        cat: &mut Catalog,
    ) -> Result<ScanReport> {
        scan(lib, src, cat, &mut |_| {}, &AtomicBool::new(false))
    }

    #[test]
    fn two_libraries_scan_into_one_catalog_with_combined_genres() {
        let one = library_dir(&["Jazz/a.mp3", "Rock/b.mp3"]);
        // The same path in both, and unicode names with an AppleDouble file.
        let two = library_dir(&["Jazz/a.mp3", "Trance/Čuvarkuća 🎶.mp3"]);
        fs::write(two.path().join("Trance/._Čuvarkuća 🎶.mp3"), "x").unwrap();
        fs::write(two.path().join("Trance/.hidden.mp3"), "x").unwrap();
        let mut cat = Catalog::open_in_memory().unwrap();
        let r1 = scan_lib(1, &LocalSource::new(one.path()), &mut cat).unwrap();
        let r2 = scan_lib(2, &LocalSource::new(two.path()), &mut cat).unwrap();
        assert_eq!((r1.total, r2.total), (2, 2), "the ._ file is skipped");
        // The fixture is tagged "Trance; Chillout": the genres count both libraries.
        let genres = |cat: &Catalog| -> Vec<(String, usize)> {
            cat.genres()
                .unwrap()
                .into_iter()
                .map(|g| (g.key, g.count))
                .collect()
        };
        assert_eq!(
            genres(&cat),
            [("chillout".to_string(), 4), ("trance".to_string(), 4)]
        );
        assert_eq!(cat.library_track_counts().unwrap(), [(1, 2), (2, 2)].into());
        // Scanning one again leaves the other alone.
        fs::remove_file(one.path().join("Rock/b.mp3")).unwrap();
        let r1 = scan_lib(1, &LocalSource::new(one.path()), &mut cat).unwrap();
        assert_eq!((r1.removed, r1.unchanged), (1, 1));
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 3);
        // Disabling library 2 hides its tracks.
        cat.set_disabled_libraries(&[2]).unwrap();
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 1);
        assert_eq!(
            genres(&cat),
            [("chillout".to_string(), 1), ("trance".to_string(), 1)]
        );
    }

    #[test]
    fn an_empty_listing_of_a_library_with_tracks_removes_nothing() {
        let disk = library_dir(&["Jazz/a.mp3", "Rock/b.mp3"]);
        let mut cat = Catalog::open_in_memory().unwrap();
        scan_lib(2, &LocalSource::new(disk.path()), &mut cat).unwrap();
        cat.set_banned(cat.playable_tracks(&[]).unwrap()[0], true)
            .unwrap();
        // A stale, empty mount point where the disk used to be: it lists fine.
        let empty = tempfile::tempdir().unwrap();
        let r = scan_lib(2, &LocalSource::new(empty.path()), &mut cat).unwrap();
        assert!(r.looks_empty);
        assert_eq!(r.removed, 0);
        assert_eq!(cat.library_track_counts().unwrap()[&2], 2);
        assert_eq!(cat.banned().unwrap().len(), 1, "the ban is kept");
        assert_eq!(cat.absent_count(), 0);
        // A library with no tracks yet that lists nothing is just empty.
        let r = scan_lib(3, &LocalSource::new(empty.path()), &mut cat).unwrap();
        assert!(!r.looks_empty);
    }

    /// A disk pulled out mid-scan: every file it still has to read is "not
    /// found", and the source says it is no longer there.
    struct Unplugged {
        files: Vec<FileEntry>,
        there: AtomicBool,
    }

    impl MusicSource for Unplugged {
        fn walk(
            &self,
            on_entry: &mut dyn FnMut(FileEntry),
        ) -> std::result::Result<WalkReport, SourceError> {
            self.files.iter().cloned().for_each(on_entry);
            // Pulled right after the listing.
            self.there.store(false, Ordering::SeqCst);
            Ok(WalkReport::default())
        }
        fn open(&self, _: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
            Err(SourceError::NotFound)
        }
        fn available(&self) -> bool {
            self.there.load(Ordering::SeqCst)
        }
        fn describe(&self) -> String {
            "unplugged".into()
        }
    }

    #[test]
    fn a_disk_pulled_mid_scan_stops_its_scan_without_failures() {
        let disk = library_dir(&["Jazz/a.mp3", "Rock/b.mp3"]);
        let mut cat = Catalog::open_in_memory().unwrap();
        scan_lib(2, &LocalSource::new(disk.path()), &mut cat).unwrap();
        // The files changed on the disk (so they are read again), then it is pulled.
        let src = Unplugged {
            files: ["Jazz/a.mp3", "Rock/b.mp3", "Pop/new.mp3"]
                .iter()
                .map(|p| FileEntry {
                    rel_path: p.to_string(),
                    size: 5,
                    mtime: 5,
                })
                .collect(),
            there: AtomicBool::new(true),
        };
        let r = scan_lib(2, &src, &mut cat);
        assert!(
            matches!(r, Err(Error::Source(SourceError::Unreachable))),
            "{r:?}"
        );
        assert!(cat.unreadable().unwrap().is_empty(), "no not_found flood");
        assert_eq!(
            cat.library_track_counts().unwrap()[&2],
            2,
            "kept, nothing added"
        );
        assert_eq!(cat.absent_count(), 0);
    }

    #[test]
    fn an_offline_library_keeps_its_tracks() {
        let disk = library_dir(&["Jazz/a.mp3", "Rock/b.mp3"]);
        let mut cat = Catalog::open_in_memory().unwrap();
        scan_lib(2, &LocalSource::new(disk.path()), &mut cat).unwrap();
        let other = library_dir(&["Pop/c.mp3"]);
        scan_lib(1, &LocalSource::new(other.path()), &mut cat).unwrap();
        // The disk is unplugged: the scan of its library fails, nothing goes.
        let unplugged = LocalSource::new(disk.path().join("not-mounted"));
        assert!(scan_lib(2, &unplugged, &mut cat).is_err());
        assert_eq!(cat.library_track_counts().unwrap()[&2], 2);
        assert_eq!(cat.playable_tracks(&[]).unwrap().len(), 3);
    }

    /// A failover pair whose primary is down: the listing comes from the backup.
    fn backup_walk(backup: &Path) -> crate::source::FailoverSource {
        struct Down;
        impl MusicSource for Down {
            fn walk(
                &self,
                _: &mut dyn FnMut(FileEntry),
            ) -> std::result::Result<WalkReport, SourceError> {
                Err(SourceError::Unreachable)
            }
            fn open(&self, _: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
                Err(SourceError::Unreachable)
            }
            fn describe(&self) -> String {
                "down".into()
            }
        }
        crate::source::FailoverSource::new(Box::new(Down), Some(Box::new(LocalSource::new(backup))))
    }

    #[test]
    fn a_scan_from_the_backup_server_removes_nothing() {
        let primary = library_dir(&["Jazz/a.mp3", "Rock/new-on-primary.mp3"]);
        let mut cat = Catalog::open_in_memory().unwrap();
        scan_lib(1, &LocalSource::new(primary.path()), &mut cat).unwrap();
        // The backup lags: it lacks the newest file but has one of its own.
        let backup = library_dir(&["Jazz/a.mp3", "Pop/only-on-backup.mp3"]);
        let r = scan_lib(1, &backup_walk(backup.path()), &mut cat).unwrap();
        assert!(r.from_backup);
        assert_eq!(r.removed, 0, "removals wait for the primary");
        assert_eq!(
            cat.playable_tracks(&[]).unwrap().len(),
            3,
            "added, none removed"
        );
        assert_eq!(cat.absent_count(), 0, "nothing left marked unseen");
        // Back on the primary: what it lacks goes.
        let r = scan_lib(1, &LocalSource::new(primary.path()), &mut cat).unwrap();
        assert!(!r.from_backup);
        assert_eq!(r.removed, 1);
    }
}
