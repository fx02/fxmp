//! Music source backed by a local directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::{is_supported, FileEntry, MusicSource, ReadSeek, SourceError};

pub struct LocalSource {
    root: PathBuf,
}

impl LocalSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn walk_dir(
        &self,
        dir: &Path,
        rel: &str,
        on_entry: &mut dyn FnMut(FileEntry),
    ) -> Result<(), SourceError> {
        for item in fs::read_dir(dir).map_err(map_io)? {
            let item = item.map_err(map_io)?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel_path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            // fs::metadata follows symlinks, so linked files and folders work.
            let md = match fs::metadata(item.path()) {
                Ok(md) => md,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // dangling link
                Err(e) => return Err(map_io(e)),
            };
            if md.is_dir() {
                self.walk_dir(&item.path(), &rel_path, on_entry)?;
            } else if md.is_file() && is_supported(&rel_path) {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                on_entry(FileEntry {
                    rel_path,
                    size: md.len(),
                    mtime,
                });
            }
        }
        Ok(())
    }
}

fn map_io(e: io::Error) -> SourceError {
    match e.kind() {
        io::ErrorKind::NotFound => SourceError::NotFound,
        io::ErrorKind::TimedOut => SourceError::Timeout,
        _ => SourceError::Io(e.to_string()),
    }
}

impl MusicSource for LocalSource {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<(), SourceError> {
        self.walk_dir(&self.root, "", on_entry)
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        let file = fs::File::open(self.root.join(rel_path)).map_err(map_io)?;
        Ok(Box::new(file))
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn local_walk_finds_supported_only() {
        let tmp = tempfile::tempdir().unwrap();
        for f in ["tone.mp3", "tone.flac", "tone.ogg", "broken.mp3"] {
            fs::copy(fixtures().join(f), tmp.path().join(f)).unwrap();
        }
        fs::write(tmp.path().join("notes.txt"), "x").unwrap();
        fs::write(tmp.path().join(".hidden.mp3"), "x").unwrap();
        fs::create_dir(tmp.path().join(".secret")).unwrap();
        fs::write(tmp.path().join(".secret/in.mp3"), "x").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::copy(
            fixtures().join("tone.mp3"),
            tmp.path().join("sub/UPPER.MP3"),
        )
        .unwrap();

        let src = LocalSource::new(tmp.path());
        let mut got = Vec::new();
        src.walk(&mut |e| got.push(e)).unwrap();
        let mut paths: Vec<_> = got.iter().map(|e| e.rel_path.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "broken.mp3",
                "sub/UPPER.MP3",
                "tone.flac",
                "tone.mp3",
                "tone.ogg"
            ]
        );
        let tone = got.iter().find(|e| e.rel_path == "tone.mp3").unwrap();
        assert_eq!(
            tone.size,
            fs::metadata(fixtures().join("tone.mp3")).unwrap().len()
        );
        assert!(tone.mtime > 0);
    }

    #[test]
    fn local_fixtures_dir_has_exactly_four_audio_files() {
        let src = LocalSource::new(fixtures());
        let mut n = 0;
        src.walk(&mut |_| n += 1).unwrap();
        assert_eq!(n, 4);
    }

    #[test]
    fn local_open_reads_and_maps_not_found() {
        let src = LocalSource::new(fixtures());
        let mut buf = Vec::new();
        src.open("tone.mp3").unwrap().read_to_end(&mut buf).unwrap();
        assert!(!buf.is_empty());
        assert_eq!(src.open("nope.mp3").err(), Some(SourceError::NotFound));
        assert_eq!(
            LocalSource::new(fixtures().join("missing"))
                .walk(&mut |_| {})
                .err(),
            Some(SourceError::NotFound)
        );
        assert!(src.describe().contains("fixtures"));
    }
}
