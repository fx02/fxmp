//! Music source backed by a local directory.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::{is_supported, FileEntry, MusicSource, ReadSeek, SourceError, WalkReport};

pub struct LocalSource {
    root: PathBuf,
}

impl LocalSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Walks `dir`; a subfolder that cannot be listed is skipped and added to
    /// `skipped` (the root failing is an error). Linked folders outside the
    /// library (`root`: its resolved path) are followed; one inside it is not, so
    /// its tracks keep their real path whatever order the folders are listed
    /// in. A folder already walked (`visited`, by its resolved path) is not
    /// walked again, so a link loop ends. Entries are walked in name order.
    fn walk_dir(
        &self,
        dir: &Path,
        rel: &str,
        root: Option<&Path>,
        visited: &mut HashSet<PathBuf>,
        skipped: &mut Vec<String>,
        on_entry: &mut dyn FnMut(FileEntry),
    ) -> Result<(), SourceError> {
        match fs::canonicalize(dir) {
            Ok(real) => {
                if !visited.insert(real) {
                    log::debug!("not walking '{rel}' again (a link to a folder already walked)");
                    return Ok(());
                }
            }
            Err(e) if rel.is_empty() => return Err(map_io(e)),
            Err(e) => {
                log::warn!("skipping unreadable folder '{rel}': {e}");
                skipped.push(rel.to_string());
                return Ok(());
            }
        }
        let items = match fs::read_dir(dir) {
            Ok(items) => items,
            Err(e) if !rel.is_empty() => {
                log::warn!("skipping unreadable folder '{rel}': {e}");
                skipped.push(rel.to_string());
                return Ok(());
            }
            Err(e) => return Err(map_io(e)),
        };
        let mut items = items.collect::<io::Result<Vec<_>>>().map_err(map_io)?;
        items.sort_by_key(|i| i.file_name());
        for item in items {
            // Hidden files, including macOS `._*` AppleDouble companions.
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                log::warn!(
                    "skipping a file whose name is not valid Unicode: {:?}",
                    item.path()
                );
                continue;
            };
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
                let linked = item.file_type().is_ok_and(|t| t.is_symlink());
                if linked && Self::inside(root, &item.path()) {
                    log::debug!("not following '{rel_path}': a link to a folder in the library");
                    continue;
                }
                self.walk_dir(&item.path(), &rel_path, root, visited, skipped, on_entry)?;
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

impl LocalSource {
    /// `path` resolves to a folder inside `root` (resolved).
    fn inside(root: Option<&Path>, path: &Path) -> bool {
        match (root, fs::canonicalize(path)) {
            (Some(root), Ok(real)) => real.starts_with(root),
            _ => false,
        }
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
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<WalkReport, SourceError> {
        let mut skipped_dirs = Vec::new();
        let root = fs::canonicalize(&self.root).ok();
        self.walk_dir(
            &self.root,
            "",
            root.as_deref(),
            &mut HashSet::new(),
            &mut skipped_dirs,
            on_entry,
        )?;
        Ok(WalkReport {
            skipped_dirs,
            from_backup: false,
        })
    }

    fn open(&self, rel_path: &str) -> Result<Box<dyn ReadSeek>, SourceError> {
        let file = fs::File::open(self.root.join(rel_path)).map_err(map_io)?;
        Ok(Box::new(file))
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    /// The music folder can still be listed.
    fn available(&self) -> bool {
        fs::read_dir(&self.root).is_ok()
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

    #[test]
    #[cfg(unix)] // folder permissions
    fn local_walk_skips_and_reports_an_unreadable_subfolder() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        for d in ["ok", "locked"] {
            fs::create_dir(tmp.path().join(d)).unwrap();
            fs::copy(
                fixtures().join("tone.mp3"),
                tmp.path().join(d).join("a.mp3"),
            )
            .unwrap();
        }
        let locked = tmp.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            // Running as root: permissions do not stop us; nothing to test.
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let mut seen = Vec::new();
        let report = LocalSource::new(tmp.path()).walk(&mut |e| seen.push(e.rel_path));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(seen, ["ok/a.mp3"]);
        assert_eq!(report.unwrap().skipped_dirs, ["locked"]);
        // The root itself unreadable is an error.
        assert!(LocalSource::new(tmp.path().join("nope"))
            .walk(&mut |_| {})
            .is_err());
    }

    #[test]
    fn local_walk_takes_unicode_and_spaces_and_skips_apple_double_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Čćž Ђ 🎵 folder");
        fs::create_dir(&dir).unwrap();
        for name in ["Pjesma čćžšđ 🎶.mp3", "a b  c.flac"] {
            fs::copy(fixtures().join("tone.mp3"), dir.join(name)).unwrap();
        }
        // AppleDouble companions (resource forks on non-Mac disks) and hidden files.
        fs::write(dir.join("._Pjesma čćžšđ 🎶.mp3"), "x").unwrap();
        fs::write(tmp.path().join("._root.mp3"), "x").unwrap();
        fs::write(tmp.path().join(".DS_Store"), "x").unwrap();

        let src = LocalSource::new(tmp.path());
        let mut got = Vec::new();
        src.walk(&mut |e| got.push(e.rel_path)).unwrap();
        got.sort();
        assert_eq!(
            got,
            [
                "Čćž Ђ 🎵 folder/Pjesma čćžšđ 🎶.mp3",
                "Čćž Ђ 🎵 folder/a b  c.flac"
            ]
        );
        // And they open by the path the walk gave.
        for rel in &got {
            let mut buf = Vec::new();
            src.open(rel).unwrap().read_to_end(&mut buf).unwrap();
            assert!(!buf.is_empty(), "{rel}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn local_walk_does_not_follow_symlink_loops() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir_all(a.join("b")).unwrap();
        fs::copy(fixtures().join("tone.mp3"), a.join("b/x.mp3")).unwrap();
        // b/up -> a and loop -> the root itself: endless if followed blindly.
        std::os::unix::fs::symlink(&a, a.join("b/up")).unwrap();
        std::os::unix::fs::symlink(tmp.path(), tmp.path().join("loop")).unwrap();
        // A link to a folder outside the root is followed (once).
        let outside = tempfile::tempdir().unwrap();
        fs::copy(fixtures().join("tone.ogg"), outside.path().join("o.ogg")).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("ext")).unwrap();

        let mut got = Vec::new();
        LocalSource::new(tmp.path())
            .walk(&mut |e| got.push(e.rel_path))
            .unwrap();
        got.sort();
        assert_eq!(got, ["a/b/x.mp3", "ext/o.ogg"]);
    }

    #[test]
    #[cfg(unix)]
    fn a_folder_also_linked_from_inside_the_library_keeps_its_real_path() {
        for _ in 0..5 {
            let tmp = tempfile::tempdir().unwrap();
            let real = tmp.path().join("z/real");
            fs::create_dir_all(&real).unwrap();
            fs::copy(fixtures().join("tone.mp3"), real.join("x.mp3")).unwrap();
            // Links that the walk may meet before the folder itself.
            std::os::unix::fs::symlink(&real, tmp.path().join("a-link")).unwrap();
            fs::create_dir(tmp.path().join("m")).unwrap();
            std::os::unix::fs::symlink(&real, tmp.path().join("m/deep-link")).unwrap();
            let mut got = Vec::new();
            LocalSource::new(tmp.path())
                .walk(&mut |e| got.push(e.rel_path))
                .unwrap();
            assert_eq!(got, ["z/real/x.mp3"]);
        }
    }
}
