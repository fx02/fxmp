//! Tag reading via lofty.

use crate::catalog::TrackMeta;
use crate::source::ReadSeek;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};

use crate::{Error, Result};

/// Reads tags and duration from a stream. `ext` (without dot, any case) selects
/// the format; when unknown, the format is guessed from the content.
pub fn read_meta(reader: &mut dyn ReadSeek, ext: &str) -> Result<TrackMeta> {
    let mut probe = Probe::new(reader);
    probe = match FileType::from_ext(ext.to_ascii_lowercase()) {
        Some(ft) => probe.set_file_type(ft),
        None => probe.guess_file_type().map_err(Error::Io)?,
    };
    let file = probe.read().map_err(|e| Error::Decode(e.to_string()))?;

    let mut meta = TrackMeta {
        duration_ms: Some(file.properties().duration().as_millis() as u64),
        ..TrackMeta::default()
    };
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        meta.artist = tag.artist().map(|s| s.into_owned());
        meta.album = tag.album().map(|s| s.into_owned());
        meta.title = tag.title().map(|s| s.into_owned());
        meta.track_no = tag.track();
        meta.raw_genre = tag.genre().map(|s| s.into_owned());
        meta.rg_track_gain_db = tag
            .get_string(ItemKey::ReplayGainTrackGain)
            .and_then(parse_gain_db);
    }
    Ok(meta)
}

/// Parses ReplayGain values such as `-6.50 dB`.
fn parse_gain_db(s: &str) -> Option<f32> {
    let s = s.trim();
    let s = s
        .get(s.len().saturating_sub(2)..)
        .filter(|tail| tail.eq_ignore_ascii_case("db"))
        .map_or(s, |_| &s[..s.len() - 2]);
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::PathBuf;

    fn open(name: &str) -> File {
        File::open(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
    }

    #[test]
    fn read_meta_reads_tags() {
        for (file, ext) in [("tone.mp3", "mp3"), ("tone.flac", "flac"), ("tone.ogg", "ogg")] {
            let m = read_meta(&mut open(file), ext).unwrap();
            assert_eq!(m.artist.as_deref(), Some("Test Artist"), "{file}");
            assert_eq!(m.title.as_deref(), Some("Test Title"), "{file}");
            assert_eq!(m.raw_genre.as_deref(), Some("Trance; Chillout"), "{file}");
            let d = m.duration_ms.unwrap();
            assert!((900..=1100).contains(&d), "{file} duration {d}");
            if ext == "flac" {
                assert_eq!(m.rg_track_gain_db, Some(-6.5));
            } else {
                assert_eq!(m.rg_track_gain_db, None, "{file}");
            }
        }
    }

    #[test]
    fn read_meta_unknown_ext_guesses_type() {
        let m = read_meta(&mut open("tone.flac"), "bin").unwrap();
        assert_eq!(m.title.as_deref(), Some("Test Title"));
    }

    #[test]
    fn read_meta_broken_is_err() {
        assert!(read_meta(&mut open("broken.mp3"), "mp3").is_err());
    }
}
