//! Primary/secondary source pair: falls back to the secondary when the primary
//! is down or lacks a file.

use std::sync::atomic::{AtomicBool, Ordering};

use super::{FileEntry, MusicSource, ReadSeek, SourceError, WalkReport};

/// Which server served the most recent successful operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Primary,
    Secondary,
}

pub struct FailoverSource {
    /// `None`: switched off, everything is read from the secondary.
    primary: Option<Box<dyn MusicSource>>,
    secondary: Option<Box<dyn MusicSource>>,
    on_secondary: AtomicBool,
}

/// Errors that justify trying the secondary. `Auth` and `Io` do not: they
/// point at a configuration or data problem the secondary would not fix.
fn walk_falls_back(e: &SourceError) -> bool {
    matches!(e, SourceError::Unreachable | SourceError::Timeout)
}

fn open_falls_back(e: &SourceError) -> bool {
    walk_falls_back(e) || matches!(e, SourceError::NotFound)
}

impl FailoverSource {
    pub fn new(primary: Box<dyn MusicSource>, secondary: Option<Box<dyn MusicSource>>) -> Self {
        Self {
            primary: Some(primary),
            secondary,
            on_secondary: AtomicBool::new(false),
        }
    }

    /// A pair whose primary is switched off: only `secondary` is read, and its
    /// listings count as the backup's.
    pub fn backup_only(secondary: Box<dyn MusicSource>) -> Self {
        Self {
            primary: None,
            secondary: Some(secondary),
            on_secondary: AtomicBool::new(true),
        }
    }

    /// The server that answered last: the one files are opened from first.
    pub fn active(&self) -> Which {
        if self.on_secondary.load(Ordering::Relaxed) {
            Which::Secondary
        } else {
            Which::Primary
        }
    }

    /// Opens files from `which` first from now on (a check found it answering).
    /// Ignored for a server that is not there.
    pub fn prefer(&self, which: Which) {
        let there = match which {
            Which::Primary => self.primary.is_some(),
            Which::Secondary => self.secondary.is_some(),
        };
        if there {
            self.on_secondary
                .store(which == Which::Secondary, Ordering::Relaxed);
        }
    }

    fn server(&self, which: Which) -> Option<&dyn MusicSource> {
        match which {
            Which::Primary => self.primary.as_deref(),
            Which::Secondary => self.secondary.as_deref(),
        }
    }
}

fn other(which: Which) -> Which {
    match which {
        Which::Primary => Which::Secondary,
        Which::Secondary => Which::Primary,
    }
}

impl MusicSource for FailoverSource {
    /// Walks the primary first, whichever server answered last: only its listing
    /// may sweep files that are gone. Entries are buffered so a mid-walk failure
    /// never delivers a partial listing before the secondary's full one. A listing
    /// by the secondary is reported as [`WalkReport::from_backup`].
    /// On failure the primary's error is returned if the secondary fails too.
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
        let err = match &self.primary {
            Some(primary) => {
                let mut buf = Vec::new();
                match primary.walk(&mut |e| buf.push(e)) {
                    Ok(report) => {
                        self.prefer(Which::Primary);
                        buf.into_iter().for_each(on_entry);
                        return Ok(report);
                    }
                    Err(e) => Some(e),
                }
            }
            None => None,
        };
        let secondary = match (&self.secondary, &err) {
            (Some(s), None) => s,
            (Some(s), Some(e)) if walk_falls_back(e) => s,
            _ => return Err(err.unwrap_or(SourceError::Unreachable)),
        };
        let mut buf = Vec::new();
        let report = match secondary.walk(&mut |e| buf.push(e)) {
            Ok(report) => report,
            Err(e) => return Err(err.unwrap_or(e)),
        };
        self.prefer(Which::Secondary);
        buf.into_iter().for_each(on_entry);
        Ok(WalkReport {
            from_backup: true,
            ..report
        })
    }

    /// Opens from the server that answered last, then from the other one. A day
    /// on the backup thus never waits on a dead primary for each track.
    /// On failure the primary's error is returned if the secondary fails too.
    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        let first = self.active();
        let err = match self.server(first).map(|s| s.open(rel_path)) {
            Some(Ok(r)) => return Ok(r),
            Some(Err(e)) => e,
            None => SourceError::Unreachable,
        };
        // A password the backup rejects is no reason to skip the primary.
        let falls_back = open_falls_back(&err)
            || (first == Which::Secondary && matches!(err, SourceError::Auth));
        let second = other(first);
        let Some(server) = self.server(second).filter(|_| falls_back) else {
            return Err(err);
        };
        match server.open(rel_path) {
            Ok(r) => {
                self.prefer(second);
                Ok(r)
            }
            Err(e) => Err(if first == Which::Primary { err } else { e }),
        }
    }

    fn describe(&self) -> String {
        match (&self.primary, &self.secondary) {
            (Some(p), Some(s)) => format!("{} (fallback: {})", p.describe(), s.describe()),
            (Some(p), None) => p.describe(),
            (None, Some(s)) => s.describe(),
            (None, None) => "(no server)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;

    /// Fake source: per-path errors, optional walk error, fixed walk entries.
    struct FakeSource {
        name: &'static str,
        open_errors: HashMap<String, SourceError>,
        walk_error: Option<SourceError>,
        entries: Vec<FileEntry>,
        skipped: Vec<String>,
    }

    impl FakeSource {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                open_errors: HashMap::new(),
                walk_error: None,
                entries: vec![],
                skipped: vec![],
            }
        }
        fn skip(mut self, dir: &str) -> Self {
            self.skipped.push(dir.into());
            self
        }
        fn open_err(mut self, path: &str, e: SourceError) -> Self {
            self.open_errors.insert(path.into(), e);
            self
        }
        fn walk_err(mut self, e: SourceError) -> Self {
            self.walk_error = Some(e);
            self
        }
        fn entry(mut self, path: &str) -> Self {
            self.entries.push(FileEntry {
                rel_path: path.into(),
                size: 1,
                mtime: 1,
            });
            self
        }
    }

    impl MusicSource for FakeSource {
        fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
            // Emit entries first, then fail: simulates a mid-walk failure.
            for e in &self.entries {
                on_entry(e.clone());
            }
            match self.walk_error.clone() {
                Some(e) => Err(e),
                None => Ok(WalkReport {
                    skipped_dirs: self.skipped.clone(),
                    from_backup: false,
                }),
            }
        }
        fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
            match self.open_errors.get(rel_path) {
                Some(e) => Err(e.clone()),
                None => Ok(Box::new(Cursor::new(self.name.as_bytes().to_vec()))),
            }
        }
        fn describe(&self) -> String {
            self.name.into()
        }
    }

    fn failover(p: FakeSource, s: Option<FakeSource>) -> FailoverSource {
        FailoverSource::new(Box::new(p), s.map(|s| Box::new(s) as Box<dyn MusicSource>))
    }

    fn content(src: &FailoverSource, path: &str) -> Result<String, SourceError> {
        let mut r = src.open(path)?;
        let mut s = String::new();
        std::io::Read::read_to_string(&mut r, &mut s).unwrap();
        Ok(s)
    }

    #[test]
    fn open_falls_back_on_unreachable() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::Unreachable),
            Some(FakeSource::new("s")),
        );
        assert_eq!(f.active(), Which::Primary);
        assert_eq!(content(&f, "a.mp3").unwrap(), "s");
        assert_eq!(f.active(), Which::Secondary);
        // Sticky: the next track comes from the backup too, without asking the primary.
        assert_eq!(content(&f, "b.mp3").unwrap(), "s");
        assert_eq!(f.active(), Which::Secondary);
        // A check that finds the primary answering brings it back.
        f.prefer(Which::Primary);
        assert_eq!(content(&f, "b.mp3").unwrap(), "p");
    }

    #[test]
    fn on_the_backup_the_primary_is_not_asked_first() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::Timeout),
            Some(FakeSource::new("s")),
        );
        f.prefer(Which::Secondary);
        assert_eq!(content(&f, "a.mp3").unwrap(), "s");
        assert_eq!(f.active(), Which::Secondary);
    }

    #[test]
    fn a_file_the_backup_lacks_comes_from_the_primary() {
        let f = failover(
            FakeSource::new("p"),
            Some(FakeSource::new("s").open_err("new.mp3", SourceError::NotFound)),
        );
        f.prefer(Which::Secondary);
        assert_eq!(content(&f, "new.mp3").unwrap(), "p");
        assert_eq!(f.active(), Which::Primary, "the primary answered last");
    }

    #[test]
    fn a_password_the_backup_rejects_falls_back_to_the_primary() {
        let f = failover(
            FakeSource::new("p"),
            Some(FakeSource::new("s").open_err("a.mp3", SourceError::Auth)),
        );
        f.prefer(Which::Secondary);
        assert_eq!(content(&f, "a.mp3").unwrap(), "p");
    }

    #[test]
    fn both_failing_from_the_backup_returns_the_primarys_error() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::NotFound),
            Some(FakeSource::new("s").open_err("a.mp3", SourceError::Unreachable)),
        );
        f.prefer(Which::Secondary);
        assert_eq!(content(&f, "a.mp3").err(), Some(SourceError::NotFound));
        assert_eq!(f.active(), Which::Secondary);
    }

    #[test]
    fn a_walk_asks_the_primary_first_even_on_the_backup() {
        let f = failover(
            FakeSource::new("p").entry("a.mp3"),
            Some(FakeSource::new("s").entry("x.mp3")),
        );
        f.prefer(Which::Secondary);
        let mut got = vec![];
        let report = f.walk(&mut |e| got.push(e.rel_path)).unwrap();
        assert_eq!(got, ["a.mp3"]);
        assert!(!report.from_backup);
        assert_eq!(f.active(), Which::Primary);
    }

    #[test]
    fn a_switched_off_primary_is_never_read() {
        let f = FailoverSource::backup_only(Box::new(FakeSource::new("s").entry("x.mp3")));
        assert_eq!(f.active(), Which::Secondary);
        assert_eq!(content(&f, "a.mp3").unwrap(), "s");
        f.prefer(Which::Primary);
        assert_eq!(f.active(), Which::Secondary, "no primary to prefer");
        let report = f.walk(&mut |_| {}).unwrap();
        assert!(report.from_backup, "a backup listing never sweeps");
        let f = FailoverSource::backup_only(Box::new(
            FakeSource::new("s")
                .open_err("a.mp3", SourceError::Timeout)
                .walk_err(SourceError::Unreachable),
        ));
        assert_eq!(content(&f, "a.mp3").err(), Some(SourceError::Timeout));
        assert_eq!(f.walk(&mut |_| {}).err(), Some(SourceError::Unreachable));
    }

    #[test]
    fn open_falls_back_on_not_found() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::NotFound),
            Some(FakeSource::new("s")),
        );
        assert_eq!(content(&f, "a.mp3").unwrap(), "s");
        assert_eq!(f.active(), Which::Secondary);
    }

    #[test]
    fn open_falls_back_on_timeout() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::Timeout),
            Some(FakeSource::new("s")),
        );
        assert_eq!(content(&f, "a.mp3").unwrap(), "s");
    }

    #[test]
    fn auth_error_does_not_fall_back() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::Auth),
            Some(FakeSource::new("s")),
        );
        assert_eq!(content(&f, "a.mp3").err(), Some(SourceError::Auth));
        assert_eq!(f.active(), Which::Primary);
    }

    #[test]
    fn walk_uses_secondary_when_primary_down() {
        // Primary emits an entry before failing; it must not leak into the result.
        let f = failover(
            FakeSource::new("p")
                .entry("partial.mp3")
                .walk_err(SourceError::Unreachable),
            Some(FakeSource::new("s").entry("x.mp3").entry("y.flac")),
        );
        let mut got = vec![];
        f.walk(&mut |e| got.push(e.rel_path)).unwrap();
        assert_eq!(got, ["x.mp3", "y.flac"]);
        assert_eq!(f.active(), Which::Secondary);
    }

    #[test]
    fn walk_forwards_primary_entries_on_success() {
        let f = failover(
            FakeSource::new("p").entry("a.mp3"),
            Some(FakeSource::new("s")),
        );
        let mut got = vec![];
        f.walk(&mut |e| got.push(e.rel_path)).unwrap();
        assert_eq!(got, ["a.mp3"]);
        assert_eq!(f.active(), Which::Primary);
    }

    #[test]
    fn walk_reports_the_folders_the_walking_server_skipped() {
        let f = failover(
            FakeSource::new("p").entry("a.mp3").skip("Jazz"),
            Some(FakeSource::new("s").skip("Rock")),
        );
        let report = f.walk(&mut |_| {}).unwrap();
        assert_eq!(report.skipped_dirs, ["Jazz"]);
        let f = failover(
            FakeSource::new("p")
                .walk_err(SourceError::Unreachable)
                .skip("Jazz"),
            Some(FakeSource::new("s").entry("x.mp3").skip("Rock")),
        );
        let report = f.walk(&mut |_| {}).unwrap();
        assert_eq!(report.skipped_dirs, ["Rock"], "the secondary's");
    }

    #[test]
    fn a_walk_served_by_the_secondary_says_so() {
        let f = failover(
            FakeSource::new("p").entry("a.mp3"),
            Some(FakeSource::new("s")),
        );
        assert!(!f.walk(&mut |_| {}).unwrap().from_backup);
        let f = failover(
            FakeSource::new("p").walk_err(SourceError::Timeout),
            Some(FakeSource::new("s").entry("x.mp3")),
        );
        assert!(f.walk(&mut |_| {}).unwrap().from_backup);
    }

    #[test]
    fn no_secondary_returns_primary_error() {
        let f = failover(
            FakeSource::new("p")
                .open_err("a.mp3", SourceError::Unreachable)
                .walk_err(SourceError::Timeout),
            None,
        );
        assert_eq!(content(&f, "a.mp3").err(), Some(SourceError::Unreachable));
        assert_eq!(f.walk(&mut |_| {}).err(), Some(SourceError::Timeout));
    }

    #[test]
    fn both_failing_returns_primary_error() {
        let f = failover(
            FakeSource::new("p").open_err("a.mp3", SourceError::NotFound),
            Some(FakeSource::new("s").open_err("a.mp3", SourceError::Unreachable)),
        );
        assert_eq!(content(&f, "a.mp3").err(), Some(SourceError::NotFound));
        assert_eq!(f.active(), Which::Primary);
    }
}
