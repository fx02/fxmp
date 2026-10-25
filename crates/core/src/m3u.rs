//! Extended M3U playlists in UTF-8 (`.m3u8`): `#EXTM3U`, then an `#EXTINF` line and
//! a path per track. Paths are relative to the library root with `/` separators, so
//! the file works from the music folder on any system and in any player. Lines end
//! in CRLF, which old Windows players need and every other player accepts.

use std::io::{self, Write};

const EOL: &str = "\r\n";

/// One track of an exported playlist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct M3uTrack<'a> {
    /// Relative to the library root.
    pub rel_path: &'a str,
    pub artist: Option<&'a str>,
    pub title: Option<&'a str>,
    pub duration_ms: Option<u64>,
}

/// Writes an extended M3U playlist, one track at a time.
pub struct M3uWriter<W: Write> {
    out: W,
    written: usize,
}

impl<W: Write> M3uWriter<W> {
    /// Starts the playlist (writes the `#EXTM3U` header; no byte order mark).
    pub fn new(mut out: W) -> io::Result<Self> {
        out.write_all(b"#EXTM3U")?;
        out.write_all(EOL.as_bytes())?;
        Ok(Self { out, written: 0 })
    }

    pub fn track(&mut self, t: &M3uTrack) -> io::Result<()> {
        write!(self.out, "{}{EOL}{}{EOL}", extinf(t), path_line(t.rel_path))?;
        self.written += 1;
        Ok(())
    }

    /// Tracks written so far.
    pub fn written(&self) -> usize {
        self.written
    }

    /// Flushes and hands back the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

/// One line of text: line breaks and other control characters become spaces.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `#EXTINF:<seconds>,<Artist> - <Title>`; seconds rounded, -1 when unknown. The title
/// falls back to the file name without its extension.
fn extinf(t: &M3uTrack) -> String {
    let secs = t
        .duration_ms
        .map_or("-1".to_string(), |ms| ((ms + 500) / 1000).to_string());
    let artist = t.artist.map(one_line).unwrap_or_default();
    let title = t.title.map(one_line).filter(|s| !s.is_empty());
    let title = title.unwrap_or_else(|| {
        let name = t.rel_path.rsplit(['/', '\\']).next().unwrap_or(t.rel_path);
        one_line(name.rsplit_once('.').map_or(name, |(stem, _)| stem))
    });
    if artist.is_empty() {
        format!("#EXTINF:{secs},{title}")
    } else {
        format!("#EXTINF:{secs},{artist} - {title}")
    }
}

/// The relative path with `/` separators; one starting with `#` would read as a
/// comment, so it gets `./` in front.
fn path_line(rel_path: &str) -> String {
    let p = one_line(&rel_path.replace('\\', "/"));
    let p = p.trim_start_matches('/');
    if p.starts_with('#') {
        format!("./{p}")
    } else {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(tracks: &[M3uTrack]) -> String {
        let mut w = M3uWriter::new(Vec::new()).unwrap();
        for t in tracks {
            w.track(t).unwrap();
        }
        assert_eq!(w.written(), tracks.len());
        String::from_utf8(w.finish().unwrap()).unwrap()
    }

    #[test]
    fn writes_extended_m3u_with_relative_paths() {
        let got = export(&[
            M3uTrack {
                rel_path: "Rock/Metallica/01 Enter Sandman.mp3",
                artist: Some("Metallica"),
                title: Some("Enter Sandman"),
                duration_ms: Some(331_499),
            },
            M3uTrack {
                rel_path: "Pop/x.flac",
                artist: None,
                title: Some("Only Title"),
                duration_ms: Some(1_500),
            },
        ]);
        assert_eq!(
            got,
            "#EXTM3U\r\n\
             #EXTINF:331,Metallica - Enter Sandman\r\n\
             Rock/Metallica/01 Enter Sandman.mp3\r\n\
             #EXTINF:2,Only Title\r\n\
             Pop/x.flac\r\n"
        );
    }

    #[test]
    fn unicode_commas_and_quotes_pass_through_unknown_length_is_minus_one() {
        let got = export(&[M3uTrack {
            rel_path: "Domaće/Đorđe Balašević/Ćaletova \"pjesma\", 1.mp3",
            artist: Some("Đorđe Balašević"),
            title: Some("Ne lomite mi bagrenje, \"live\""),
            duration_ms: None,
        }]);
        assert_eq!(
            got,
            "#EXTM3U\r\n\
             #EXTINF:-1,Đorđe Balašević - Ne lomite mi bagrenje, \"live\"\r\n\
             Domaće/Đorđe Balašević/Ćaletova \"pjesma\", 1.mp3\r\n"
        );
        assert!(!got.starts_with('\u{feff}'), "no byte order mark");
    }

    #[test]
    fn missing_tags_fall_back_to_the_file_name_and_lines_stay_lines() {
        let got = export(&[
            M3uTrack {
                rel_path: "a\\b\\Some Song.ogg",
                artist: Some("  "),
                title: Some(""),
                duration_ms: Some(0),
            },
            M3uTrack {
                rel_path: "#1 Hits/x.mp3",
                artist: Some("Two\r\nLines"),
                title: Some("Tab\there"),
                duration_ms: Some(499),
            },
        ]);
        assert_eq!(
            got,
            "#EXTM3U\r\n\
             #EXTINF:0,Some Song\r\n\
             a/b/Some Song.ogg\r\n\
             #EXTINF:0,Two  Lines - Tab here\r\n\
             ./#1 Hits/x.mp3\r\n"
        );
    }

    #[test]
    fn an_empty_playlist_is_just_the_header() {
        assert_eq!(export(&[]), "#EXTM3U\r\n");
    }
}
