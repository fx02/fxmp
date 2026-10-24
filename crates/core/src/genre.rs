//! Genre splitting and normalization.

use std::collections::HashMap;
use std::sync::OnceLock;

use lofty::id3::v1::GENRES;

/// Aliases applied to the normalized key (already lowercase alphanumeric).
pub const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("dnb", "drumandbass"),
    ("dandb", "drumandbass"),
    ("drumnbass", "drumandbass"),
    ("hiphop", "hiphop"),
    ("raphiphop", "hiphop"),
    ("rnb", "randb"),
    ("rhythmandblues", "randb"),
    ("psychedelictrance", "psytrance"),
    ("goamusic", "goa"),
    ("goatrance", "goa"),
    ("rocknroll", "rockandroll"),
    ("worldmusic", "world"),
    ("electronica", "electronic"),
    ("children", "childrens"),
];

/// Common genres missing from the ID3v1/Winamp list, or spelled better here.
/// Listed first, so these spellings win over the ID3 ones with the same key.
const MODERN_GENRES: &[&str] = &[
    "Chillout",
    "Downtempo",
    "Lounge",
    "Trip-Hop",
    "Deep House",
    "Progressive House",
    "Tech House",
    "Electro House",
    "Minimal",
    "Techno",
    "Dubstep",
    "Breakbeat",
    "Drum & Bass",
    "Jungle",
    "Hardstyle",
    "Psytrance",
    "Goa",
    "Progressive Trance",
    "Uplifting Trance",
    "Ambient",
    "IDM",
    "Synthpop",
    "Indie",
    "Indie Rock",
    "Alternative",
    "Post-Rock",
    "Metalcore",
    "Death Metal",
    "Black Metal",
    "Thrash Metal",
    "Nu Metal",
    "Punk",
    "Grunge",
    "Blues",
    "Funk",
    "Disco",
    "Soul",
    "R&B",
    "Hip-Hop",
    "Rap",
    "Trap",
    "Reggaeton",
    "Latin",
    "World",
    "Folk",
    "Country",
    "Singer-Songwriter",
    "Classical",
    "Opera",
    "Soundtrack",
    "Jazz",
    "Smooth Jazz",
    "Nu Jazz",
    "Acid Jazz",
    "Swing",
    "Bossa Nova",
    "Gospel",
    "Christmas",
    "Children's",
    "Oldies",
    "Schlager",
    "Chanson",
    "K-Pop",
    "J-Pop",
    "Ska",
    "Reggae",
    "Dub",
    "Electronic",
    "Dance",
    "Pop",
    "Rock",
    "Hard Rock",
    "Heavy Metal",
    "New Wave",
    "Rock & Roll",
    "National Folk",
    "Classic Rock",
];

/// Placeholder or offensive names that never count as a genre, by key.
const NOT_GENRES: &[&str] = &[
    "other",
    "misc",
    "miscellaneous",
    "unknown",
    "genre",
    "none",
    "negerpunk",
];

/// Split a raw genre tag value into individual genre names.
///
/// Separators: `;`, `/`, NUL and ` | `. Parts are trimmed and empty ones dropped.
/// An ID3v1 reference (`(N)` or bare `N`) is mapped to its genre name.
pub fn split_genres(raw: &str) -> Vec<String> {
    raw.replace(" | ", ";")
        .split([';', '/', '\0'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            id3v1_name(p)
                .map(str::to_string)
                .unwrap_or_else(|| p.to_string())
        })
        .collect()
}

fn id3v1_name(part: &str) -> Option<&'static str> {
    let digits = part
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(part);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    GENRES.get(digits.parse::<usize>().ok()?).copied()
}

/// Normalized comparison key for a genre name.
pub fn genre_key(name: &str) -> String {
    let lower = name.to_lowercase();
    let spaced = lower.replace("'n'", " and ").replace(['&', '+'], " and ");
    let mut key = String::new();
    for tok in spaced.split_whitespace() {
        let tok = if matches!(tok, "n" | "n'" | "'n") {
            "and"
        } else {
            tok
        };
        key.extend(tok.chars().filter(|c| c.is_alphanumeric()));
    }
    BUILTIN_ALIASES
        .iter()
        .find(|(from, _)| *from == key)
        .map(|(_, to)| to.to_string())
        .unwrap_or(key)
}

/// Key of the genre that holds tracks with no known genre.
pub const UNKNOWN_KEY: &str = "_none";
/// Display name of [`UNKNOWN_KEY`].
pub const UNKNOWN_DISPLAY: &str = "Unknown";

/// A known genre: its normalized key and canonical display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownGenre {
    pub key: String,
    pub display: String,
}

/// Known genres by key: [`MODERN_GENRES`], then the ID3v1/Winamp list (codes 0–191).
fn known_genres() -> &'static HashMap<String, String> {
    static KNOWN: OnceLock<HashMap<String, String>> = OnceLock::new();
    KNOWN.get_or_init(|| {
        let mut map = HashMap::new();
        for name in MODERN_GENRES.iter().chain(GENRES.iter()) {
            let key = genre_key(name);
            if !NOT_GENRES.contains(&key.as_str()) {
                map.entry(key).or_insert_with(|| title_case(name));
            }
        }
        map
    })
}

/// "Classic rock" -> "Classic Rock"; other letters are left as they are.
fn title_case(name: &str) -> String {
    name.split(' ')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// The known genre `name` stands for (by key or alias), or `None`.
pub fn known_genre(name: &str) -> Option<KnownGenre> {
    let key = genre_key(name);
    let display = known_genres().get(&key)?.clone();
    Some(KnownGenre { key, display })
}

/// Display names of all known genres, sorted case-insensitively.
pub fn known_genre_names() -> Vec<String> {
    let mut names: Vec<String> = known_genres().values().cloned().collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_normalize_edge_cases() {
        assert_eq!(split_genres(""), Vec::<String>::new());
        assert_eq!(split_genres("   "), Vec::<String>::new());
        assert_eq!(split_genres("Trance; Chillout"), vec!["Trance", "Chillout"]);
        assert_eq!(
            split_genres("Trance/Progressive"),
            vec!["Trance", "Progressive"]
        );
        assert_eq!(split_genres("Jazz\0Blues"), vec!["Jazz", "Blues"]);
        assert_eq!(split_genres("Rock | Pop"), vec!["Rock", "Pop"]);
        assert_eq!(split_genres("(31)"), vec!["Trance"]);
        assert_eq!(split_genres("31"), vec!["Trance"]);
        assert_eq!(split_genres("9999"), vec!["9999"]);
    }

    #[test]
    fn keys_merge_common_variants() {
        for v in [
            "Drum & Bass",
            "DnB",
            "drum n bass",
            "Drum'n'Bass",
            "DRUM AND BASS",
        ] {
            assert_eq!(genre_key(v), "drumandbass");
        }
        assert_eq!(genre_key("Chill Out"), genre_key("Chillout"));
        assert_ne!(genre_key("Trance"), genre_key("Techno"));
        assert_eq!(genre_key("R&B"), "randb");
        assert_eq!(genre_key("RnB"), "randb");
        assert_eq!(genre_key("Hip-Hop"), "hiphop");
    }

    #[test]
    fn n_rule_does_not_mangle_words() {
        assert_eq!(genre_key("Ambient"), "ambient");
        assert_eq!(genre_key("New Age"), "newage");
        assert_eq!(genre_key("Drum n' Bass"), "drumandbass");
    }

    fn known(name: &str) -> Option<String> {
        known_genre(name).map(|g| g.display)
    }

    #[test]
    fn unknown_and_junk_names_are_not_genres() {
        for n in [
            "ZZ Test Preimenovano",
            "Other",
            "other",
            "Misc",
            "Unknown",
            "Genre",
            "",
            "  ",
            "9999",
            "Foo",
        ] {
            assert_eq!(known(n), None, "{n:?}");
        }
    }

    #[test]
    fn aliases_map_to_canonical_names() {
        let cases = [
            ("dnb", "Drum & Bass"),
            ("D&B", "Drum & Bass"),
            ("drum n bass", "Drum & Bass"),
            ("Drum'n'Bass", "Drum & Bass"),
            ("hip hop", "Hip-Hop"),
            ("HipHop", "Hip-Hop"),
            ("Rap/Hip-Hop", "Hip-Hop"),
            ("rnb", "R&B"),
            ("R'n'B", "R&B"),
            ("chill out", "Chillout"),
            ("Chill-Out", "Chillout"),
            ("psy trance", "Psytrance"),
            ("Psy-Trance", "Psytrance"),
            ("trip hop", "Trip-Hop"),
            ("synth-pop", "Synthpop"),
            ("Rock \u{2019}n\u{2019} Roll", "Rock & Roll"),
            ("rock n roll", "Rock & Roll"),
            ("classic rock", "Classic Rock"),
            ("TRANCE", "Trance"),
            ("deep house", "Deep House"),
            ("Goa music", "Goa"),
        ];
        for (name, want) in cases {
            assert_eq!(known(name).as_deref(), Some(want), "{name:?}");
        }
        let dnb = known_genre("dnb").unwrap();
        assert_eq!(dnb.key, genre_key("Drum & Bass"));
    }

    #[test]
    fn id3_codes_resolve_to_known_genres_except_other() {
        assert_eq!(known(&split_genres("(31)")[0]).as_deref(), Some("Trance"));
        assert_eq!(known(&split_genres("12")[0]), None, "12 is Other");
        for (i, name) in GENRES.iter().enumerate() {
            if !matches!(*name, "Other" | "Negerpunk") {
                assert!(known(name).is_some(), "ID3 {i} {name}");
            }
        }
    }

    #[test]
    fn known_names_are_sorted_unique_and_include_modern_genres() {
        let names = known_genre_names();
        for n in [
            "Chillout",
            "Drum & Bass",
            "Hip-Hop",
            "K-Pop",
            "Children's",
            "Unknown",
        ] {
            assert_eq!(names.iter().any(|x| x == n), n != "Unknown", "{n}");
        }
        assert!(!names.iter().any(|x| x == "Other"));
        let mut sorted = names.clone();
        sorted.sort_by_key(|s| s.to_lowercase());
        assert_eq!(names, sorted);
        let keys: std::collections::HashSet<String> = names.iter().map(|n| genre_key(n)).collect();
        assert_eq!(keys.len(), names.len(), "one display per key");
        for n in &names {
            assert_eq!(known(n).as_deref(), Some(n.as_str()), "canonical {n}");
        }
    }
}
