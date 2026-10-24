//! Tag reading via lofty.

use crate::catalog::TrackMeta;
use crate::source::ReadSeek;
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::id3::v2::FrameId;
use lofty::mpeg::MpegFile;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};

use crate::{Error, Result};

/// An I/O failure underneath the parser stays `Error::Io` (so callers can tell a
/// network hiccup from a broken file); anything else is `Error::Decode`.
fn parse_error(e: &(dyn std::error::Error + 'static)) -> Error {
    let mut cur = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Error::Io(std::io::Error::new(io.kind(), io.to_string()));
        }
        cur = err.source();
    }
    Error::Decode(e.to_string())
}

/// Reads tags and duration from a stream. `ext` (without dot, any case) selects
/// the format; when unknown, the format is guessed from the content.
pub fn read_meta(reader: &mut dyn ReadSeek, ext: &str) -> Result<TrackMeta> {
    let mut probe = Probe::new(reader);
    probe = match FileType::from_ext(ext.to_ascii_lowercase()) {
        Some(ft) => probe.set_file_type(ft),
        None => probe.guess_file_type().map_err(Error::Io)?,
    };
    // lofty maps ID3v2 genre codes ("(148)") with its own table when it builds the
    // generic tag; MP3s are read as such so the raw TCON text reaches `split_genres`.
    let (file, raw_tcon) = if probe.file_type() == Some(FileType::Mpeg) {
        let mut reader = probe.into_inner();
        reader.rewind().map_err(Error::Io)?;
        let mpeg =
            MpegFile::read_from(&mut reader, ParseOptions::new()).map_err(|e| parse_error(&e))?;
        let tcon = mpeg
            .id3v2()
            .and_then(|t| t.get_text(&FrameId::Valid("TCON".into())))
            .map(str::to_string);
        (TaggedFile::from(mpeg), tcon)
    } else {
        (probe.read().map_err(|e| parse_error(&e))?, None)
    };

    let mut meta = TrackMeta {
        duration_ms: Some(file.properties().duration().as_millis() as u64),
        ..TrackMeta::default()
    };
    if let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) {
        meta.artist = tag.artist().map(|s| s.into_owned());
        meta.album = tag.album().map(|s| s.into_owned());
        meta.title = tag.title().map(|s| s.into_owned());
        meta.track_no = tag.track();
        meta.raw_genre = raw_tcon.or_else(|| tag.genre().map(|s| s.into_owned()));
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
        File::open(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn read_meta_reads_tags() {
        for (file, ext) in [
            ("tone.mp3", "mp3"),
            ("tone.flac", "flac"),
            ("tone.ogg", "ogg"),
        ] {
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

    /// A copy of a fixture with its genre tag set to `genre`, as written by lofty.
    fn with_genre(name: &str, genre: &str) -> tempfile::NamedTempFile {
        use lofty::config::WriteOptions;
        use lofty::tag::TagExt;
        let tmp = tempfile::Builder::new()
            .suffix(&format!(".{}", name.rsplit('.').next().unwrap()))
            .tempfile()
            .unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
            tmp.path(),
        )
        .unwrap();
        if name.ends_with(".mp3") {
            use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
            use lofty::TextEncoding;
            let mut tag = Id3v2Tag::new();
            tag.set_title("Test Title".into());
            tag.insert(Frame::Text(TextInformationFrame::new(
                FrameId::Valid("TCON".into()),
                TextEncoding::UTF8,
                genre.to_string(),
            )));
            tag.save_to_path(tmp.path(), WriteOptions::default())
                .unwrap();
        } else {
            let mut file = lofty::read_from_path(tmp.path()).unwrap();
            let tag = file.primary_tag_mut().unwrap();
            tag.set_genre(genre.to_string());
            tag.save_to_path(tmp.path(), WriteOptions::default())
                .unwrap();
        }
        tmp
    }

    #[test]
    fn read_meta_keeps_raw_genre_codes() {
        // lofty would turn "(148)" into its own name for 148 ("Abstract").
        for (file, genre) in [
            ("tone.mp3", "(148)"),
            ("tone.mp3", "(17)Rock"),
            ("tone.mp3", "Trance\0Ambient"),
            ("tone.flac", "(17)Rock"),
            ("tone.ogg", "(148)"),
        ] {
            let tmp = with_genre(file, genre);
            let ext = file.rsplit('.').next().unwrap();
            let m = read_meta(&mut File::open(tmp.path()).unwrap(), ext).unwrap();
            assert_eq!(m.raw_genre.as_deref(), Some(genre), "{file} {genre:?}");
            assert_eq!(
                m.title.as_deref(),
                Some("Test Title"),
                "{file}: other tags kept"
            );
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
