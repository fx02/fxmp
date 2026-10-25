//! "Look up online" and "Copy 'Artist – Title'": search URLs on a few music sites,
//! opened in the default browser. Only artist and title are ever sent, and only
//! URLs on the hosts in [`ALLOWED_HOSTS`] (and the project page) are opened.
//! Shared by the window's context menu and the tray.

use serde::{Deserialize, Deserializer, Serialize};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::i18n;
use crate::state::{lock, AppState};

/// A site to look a track up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    #[default]
    Lastfm,
    Musicbrainz,
    Discogs,
    Wikipedia,
    Youtube,
    Genius,
}

impl Service {
    /// In menu order.
    pub const ALL: [Service; 6] = [
        Service::Lastfm,
        Service::Musicbrainz,
        Service::Discogs,
        Service::Wikipedia,
        Service::Youtube,
        Service::Genius,
    ];

    /// The site's name, as shown in menus (not translated).
    pub fn name(self) -> &'static str {
        match self {
            Service::Lastfm => "Last.fm",
            Service::Musicbrainz => "MusicBrainz",
            Service::Discogs => "Discogs",
            Service::Wikipedia => "Wikipedia",
            Service::Youtube => "YouTube",
            Service::Genius => "Genius",
        }
    }
}

/// Reads a saved service; an unknown name (a hand-edited file) is the default
/// rather than making the whole settings file unreadable.
pub fn lenient_service<'de, D: Deserializer<'de>>(d: D) -> Result<Service, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// The only hosts a lookup may open.
pub const ALLOWED_HOSTS: [&str; 6] = [
    "www.last.fm",
    "musicbrainz.org",
    "www.discogs.com",
    "en.wikipedia.org",
    "www.youtube.com",
    "genius.com",
];

/// Percent-encodes a query value: everything but RFC 3986 unreserved characters,
/// as UTF-8 bytes (a space is `%20`).
pub fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// "artist title", trimmed, without empty parts.
fn search_words(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The search for a recording (`artist` and `title`) on `service`.
pub fn track_url(service: Service, artist: &str, title: &str) -> String {
    let q = encode_query(&search_words(&[artist, title]));
    match service {
        Service::Lastfm => format!("https://www.last.fm/search?q={q}"),
        Service::Musicbrainz => {
            format!("https://musicbrainz.org/search?type=recording&query={q}")
        }
        Service::Discogs => format!("https://www.discogs.com/search/?q={q}&type=all"),
        Service::Wikipedia => format!("https://en.wikipedia.org/w/index.php?search={q}"),
        Service::Youtube => format!("https://www.youtube.com/results?search_query={q}"),
        Service::Genius => format!("https://genius.com/search?q={q}"),
    }
}

/// The search for an artist on `service`.
pub fn artist_url(service: Service, artist: &str) -> String {
    let q = encode_query(artist.trim());
    match service {
        Service::Lastfm => format!("https://www.last.fm/search/artists?q={q}"),
        Service::Musicbrainz => format!("https://musicbrainz.org/search?type=artist&query={q}"),
        Service::Discogs => format!("https://www.discogs.com/search/?q={q}&type=artist"),
        Service::Wikipedia => format!("https://en.wikipedia.org/w/index.php?search={q}"),
        Service::Youtube => format!("https://www.youtube.com/results?search_query={q}"),
        Service::Genius => format!("https://genius.com/search?q={q}"),
    }
}

/// Whether `url`, parsed as a browser would, is `https` on one of
/// [`ALLOWED_HOSTS`] exactly, with no user info and no explicit port, or is the
/// project page ([`crate::about::PROJECT_URL`]) exactly.
pub fn is_allowed(url: &str) -> bool {
    let Ok(u) = tauri::Url::parse(url) else {
        return false;
    };
    if u.as_str() == crate::about::PROJECT_URL {
        return true;
    }
    u.scheme() == "https"
        && u.username().is_empty()
        && u.password().is_none()
        && u.port().is_none()
        && u.host_str().is_some_and(|h| ALLOWED_HOSTS.contains(&h))
}

/// "Artist – Title (1991)" (no year when unknown): what "Copy" puts on the
/// clipboard. A missing artist or title is left out.
pub fn song_text(artist: &str, title: &str, year: Option<u16>) -> String {
    let text = [artist.trim(), title.trim()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" – ");
    match year {
        Some(y) if !text.is_empty() => format!("{text} ({y})"),
        _ => text,
    }
}

/// Opens the search for `artist` and `title` (just the artist when `title` is
/// `None`) on `service` (the default one from Settings when `None`).
pub fn open(
    app: &AppHandle,
    st: &AppState,
    service: Option<Service>,
    artist: &str,
    title: Option<&str>,
) -> Result<(), String> {
    let service = service.unwrap_or(st.settings.get().lookup_service);
    if artist.trim().is_empty() && title.is_none_or(|t| t.trim().is_empty()) {
        return Err(i18n::t("err.lookup_nothing"));
    }
    let url = match title {
        Some(title) => track_url(service, artist, title),
        None => artist_url(service, artist),
    };
    if !is_allowed(&url) {
        return Err(i18n::t("err.lookup_nothing"));
    }
    app.opener().open_url(&url, None::<&str>).map_err(|e| {
        log::warn!("cannot open the browser: {e}");
        i18n::tf("err.open_browser", &[("error", &e.to_string())])
    })
}

/// Looks the current song up on the default service (tray "Look up current song").
pub fn open_current(app: &AppHandle, st: &AppState) -> Result<(), String> {
    let np = lock(&st.now_playing)
        .clone()
        .ok_or_else(|| i18n::t("err.nothing_playing"))?;
    open(app, st, None, &np.artist, Some(&np.title))
}

/// The current song as "Artist – Title (Year)" (tray "Copy current song").
pub fn current_text(st: &AppState) -> Option<String> {
    lock(&st.now_playing)
        .as_ref()
        .map(|n| song_text(&n.artist, &n.title, n.year))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRICKY: &str = "AC/DC & Friends #1 + \"Hi\" 'yo' ž?=%";
    const TRICKY_ENC: &str =
        "AC%2FDC%20%26%20Friends%20%231%20%2B%20%22Hi%22%20%27yo%27%20%C5%BE%3F%3D%25";

    #[test]
    fn queries_are_percent_encoded() {
        assert_eq!(encode_query(TRICKY), TRICKY_ENC);
        assert_eq!(encode_query("Ćiro Đurić"), "%C4%86iro%20%C4%90uri%C4%87");
        assert_eq!(encode_query("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn every_service_has_its_search_url() {
        let golden = [
            (
                Service::Lastfm,
                "https://www.last.fm/search?q=Metallica%20Enter%20Sandman",
            ),
            (
                Service::Musicbrainz,
                "https://musicbrainz.org/search?type=recording&query=Metallica%20Enter%20Sandman",
            ),
            (
                Service::Discogs,
                "https://www.discogs.com/search/?q=Metallica%20Enter%20Sandman&type=all",
            ),
            (
                Service::Wikipedia,
                "https://en.wikipedia.org/w/index.php?search=Metallica%20Enter%20Sandman",
            ),
            (
                Service::Youtube,
                "https://www.youtube.com/results?search_query=Metallica%20Enter%20Sandman",
            ),
            (
                Service::Genius,
                "https://genius.com/search?q=Metallica%20Enter%20Sandman",
            ),
        ];
        for (service, url) in golden {
            assert_eq!(track_url(service, " Metallica ", "Enter Sandman"), url);
            assert!(is_allowed(url), "{url}");
        }
        assert_eq!(golden.len(), Service::ALL.len());
    }

    #[test]
    fn special_characters_stay_inside_the_query() {
        for service in Service::ALL {
            let url = track_url(service, TRICKY, "Ž & #");
            assert!(
                url.contains(&format!("{TRICKY_ENC}%20%C5%BD%20%26%20%23")),
                "{url}"
            );
            assert!(is_allowed(&url), "{url}");
            // No second query parameter, fragment or path smuggled in.
            assert!(!url.contains('#'), "{url}");
        }
        assert_eq!(
            track_url(Service::Lastfm, "", "Only Title"),
            "https://www.last.fm/search?q=Only%20Title"
        );
    }

    #[test]
    fn artist_searches() {
        assert_eq!(
            artist_url(Service::Lastfm, "Dino Merlin"),
            "https://www.last.fm/search/artists?q=Dino%20Merlin"
        );
        assert_eq!(
            artist_url(Service::Musicbrainz, "Björk"),
            "https://musicbrainz.org/search?type=artist&query=Bj%C3%B6rk"
        );
        assert_eq!(
            artist_url(Service::Discogs, "R&B Band"),
            "https://www.discogs.com/search/?q=R%26B%20Band&type=artist"
        );
        for service in Service::ALL {
            assert!(is_allowed(&artist_url(service, "x")));
        }
    }

    #[test]
    fn only_the_listed_hosts_are_allowed() {
        assert_eq!(ALLOWED_HOSTS.len(), Service::ALL.len());
        for bad in [
            "http://www.last.fm/search?q=x",
            "https://last.fm/search?q=x",
            "https://www.last.fm.evil.com/x",
            "https://evil.com/?u=https://www.last.fm",
            "https://www.last.fm:8443/x",
            "https://user@www.last.fm/x",
            "smb://10.111.100.2/F",
            "file:///etc/passwd",
            "https://de.wikipedia.org/w/index.php?search=x",
        ] {
            assert!(!is_allowed(bad), "{bad}");
        }
        assert!(is_allowed("https://genius.com"));
        for bad in [
            "https://evil.com\\.www.last.fm/",
            "https://www.last.fm%2Fevil.com/",
            "https://:pw@www.last.fm/x",
            "https://www.last.fm:1/x",
            "not a url",
        ] {
            assert!(!is_allowed(bad), "{bad}");
        }
        // Parsed like a browser does: the same host in other letters is that host,
        // and a backslash ends the host (it is a path separator in https URLs).
        assert!(is_allowed("https://www.last.fm\\@evil.com/"));
        assert!(is_allowed("https://WWW.Last.FM/search?q=x"));
    }

    #[test]
    fn copy_text_has_the_year_only_when_known() {
        assert_eq!(
            song_text("Metallica", "Enter Sandman", Some(1991)),
            "Metallica – Enter Sandman (1991)"
        );
        assert_eq!(
            song_text("Metallica", "Enter Sandman", None),
            "Metallica – Enter Sandman"
        );
        assert_eq!(song_text("", "Intro", Some(2001)), "Intro (2001)");
        assert_eq!(song_text(" ", " ", Some(2001)), "");
    }

    #[test]
    fn an_unknown_saved_service_reads_as_the_default() {
        #[derive(Deserialize)]
        struct S {
            #[serde(deserialize_with = "lenient_service")]
            s: Service,
        }
        let read = |json: &str| serde_json::from_str::<S>(json).unwrap().s;
        assert_eq!(read(r#"{"s":"genius"}"#), Service::Genius);
        assert_eq!(read(r#"{"s":"altavista"}"#), Service::Lastfm);
        assert_eq!(read(r#"{"s":3}"#), Service::Lastfm);
    }
}
