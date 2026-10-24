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
    primary: Box<dyn MusicSource>,
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
            primary,
            secondary,
            on_secondary: AtomicBool::new(false),
        }
    }

    /// The source that served the most recent successful operation.
    pub fn active(&self) -> Which {
        if self.on_secondary.load(Ordering::Relaxed) {
            Which::Secondary
        } else {
            Which::Primary
        }
    }

    fn mark(&self, which: Which) {
        self.on_secondary
            .store(which == Which::Secondary, Ordering::Relaxed);
    }
}

impl MusicSource for FailoverSource {
    /// Walks the primary; entries are buffered so a mid-walk failure never
    /// delivers a partial listing before the secondary's full one.
    /// On failure the primary's error is returned if the secondary fails too.
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
        let mut buf = Vec::new();
        let err = match self.primary.walk(&mut |e| buf.push(e)) {
            Ok(report) => {
                self.mark(Which::Primary);
                buf.into_iter().for_each(on_entry);
                return Ok(report);
            }
            Err(e) => e,
        };
        let Some(secondary) = self.secondary.as_ref().filter(|_| walk_falls_back(&err)) else {
            return Err(err);
        };
        let mut buf = Vec::new();
        let Ok(report) = secondary.walk(&mut |e| buf.push(e)) else {
            return Err(err);
        };
        self.mark(Which::Secondary);
        buf.into_iter().for_each(on_entry);
        Ok(report)
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        let err = match self.primary.open(rel_path) {
            Ok(r) => {
                self.mark(Which::Primary);
                return Ok(r);
            }
            Err(e) => e,
        };
        let Some(secondary) = self.secondary.as_ref().filter(|_| open_falls_back(&err)) else {
            return Err(err);
        };
        match secondary.open(rel_path) {
            Ok(r) => {
                self.mark(Which::Secondary);
                Ok(r)
            }
            Err(_) => Err(err),
        }
    }

    fn describe(&self) -> String {
        match &self.secondary {
            Some(s) => format!("{} (fallback: {})", self.primary.describe(), s.describe()),
            None => self.primary.describe(),
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
        // A later success on the primary flips it back.
        assert_eq!(content(&f, "b.mp3").unwrap(), "p");
        assert_eq!(f.active(), Which::Primary);
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
