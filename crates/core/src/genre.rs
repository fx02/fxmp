//! Genre splitting and normalization.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The ID3v1 genres with the Winamp extensions, codes 0–191, named as in
/// <https://en.wikipedia.org/wiki/List_of_ID3v1_genres>. These are the known genres,
/// except [`OTHER_CODE`].
pub const ID3V1_GENRES: [&str; 192] = [
    "Blues",
    "Classic rock",
    "Country",
    "Dance",
    "Disco",
    "Funk",
    "Grunge",
    "Hip-hop",
    "Jazz",
    "Metal",
    "New age",
    "Oldies",
    "Other",
    "Pop",
    "Rhythm and blues",
    "Rap",
    "Reggae",
    "Rock",
    "Techno",
    "Industrial",
    "Alternative",
    "Ska",
    "Death metal",
    "Pranks",
    "Soundtrack",
    "Euro-techno",
    "Ambient",
    "Trip-hop",
    "Vocal",
    "Jazz & funk",
    "Fusion",
    "Trance",
    "Classical",
    "Instrumental",
    "Acid",
    "House",
    "Game",
    "Sound clip",
    "Gospel",
    "Noise",
    "Alternative rock",
    "Bass",
    "Soul",
    "Punk",
    "Space",
    "Meditative",
    "Instrumental pop",
    "Instrumental rock",
    "Ethnic",
    "Gothic",
    "Darkwave",
    "Techno-industrial",
    "Electronic",
    "Pop-folk",
    "Eurodance",
    "Dream",
    "Southern rock",
    "Comedy",
    "Cult",
    "Gangsta",
    "Top 40",
    "Christian rap",
    "Pop/funk",
    "Jungle music",
    "Native US",
    "Cabaret",
    "New wave",
    "Psychedelic",
    "Rave",
    "Showtunes",
    "Trailer",
    "Lo-fi",
    "Tribal",
    "Acid punk",
    "Acid jazz",
    "Polka",
    "Retro",
    "Musical",
    "Rock 'n' roll",
    "Hard rock",
    "Folk",
    "Folk rock",
    "National folk",
    "Swing",
    "Fast fusion",
    "Bebop",
    "Latin",
    "Revival",
    "Celtic",
    "Bluegrass",
    "Avantgarde",
    "Gothic rock",
    "Progressive rock",
    "Psychedelic rock",
    "Symphonic rock",
    "Slow rock",
    "Big band",
    "Chorus",
    "Easy listening",
    "Acoustic",
    "Humour",
    "Speech",
    "Chanson",
    "Opera",
    "Chamber music",
    "Sonata",
    "Symphony",
    "Booty bass",
    "Primus",
    "Porn groove",
    "Satire",
    "Slow jam",
    "Club",
    "Tango",
    "Samba",
    "Folklore",
    "Ballad",
    "Power ballad",
    "Rhythmic Soul",
    "Freestyle",
    "Duet",
    "Punk rock",
    "Drum solo",
    "A cappella",
    "Euro-house",
    "Dance hall",
    "Goa music",
    "Drum & bass",
    "Club-house",
    "Hardcore techno",
    "Terror",
    "Indie",
    "Britpop",
    "Negerpunk",
    "Polsk punk",
    "Beat",
    "Christian gangsta rap",
    "Heavy metal",
    "Black metal",
    "Crossover",
    "Contemporary Christian",
    "Christian rock",
    "Merengue",
    "Salsa",
    "Thrash metal",
    "Anime",
    "Jpop",
    "Synthpop",
    "Christmas",
    "Art rock",
    "Baroque",
    "Bhangra",
    "Big beat",
    "Breakbeat",
    "Chillout",
    "Downtempo",
    "Dub",
    "EBM",
    "Eclectic",
    "Electro",
    "Electroclash",
    "Emo",
    "Experimental",
    "Garage",
    "Global",
    "IDM",
    "Illbient",
    "Industro-Goth",
    "Jam band",
    "Krautrock",
    "Leftfield",
    "Lounge",
    "Math rock",
    "New romantic",
    "Nu-breakz",
    "Post-punk",
    "Post-rock",
    "Psytrance",
    "Shoegaze",
    "Space rock",
    "Trop rock",
    "World music",
    "Neoclassical",
    "Audiobook",
    "Audio theatre",
    "Neue Deutsche Welle",
    "Podcast",
    "Indie rock",
    "G-Funk",
    "Dubstep",
    "Garage rock",
    "Psybient",
];

/// "Other" is a placeholder, not a genre: it goes to Unknown.
const OTHER_CODE: usize = 12;

/// Aliases applied to the normalized key (already lowercase alphanumeric). Each one
/// targets the key of a name in [`ID3V1_GENRES`].
pub const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("dnb", "drumandbass"),
    ("dandb", "drumandbass"),
    ("drumnbass", "drumandbass"),
    ("raphiphop", "hiphop"),
    ("rnb", "rhythmandblues"),
    ("randb", "rhythmandblues"),
    ("psychedelictrance", "psytrance"),
    ("goa", "goamusic"),
    ("goatrance", "goamusic"),
    ("rocknroll", "rockandroll"),
    ("jungle", "junglemusic"),
    ("world", "worldmusic"),
    ("electronica", "electronic"),
];

/// Common subgenres that are not in [`ID3V1_GENRES`], filed under their parent
/// genre (by key). Kept short on purpose: only names that are both frequent in tags
/// and clearly part of one listed genre.
const SUBGENRE_PARENTS: &[(&str, &str)] = &[
    ("deephouse", "house"),
    ("progressivehouse", "house"),
    ("techhouse", "house"),
    ("electrohouse", "house"),
    ("progressivetrance", "trance"),
    ("upliftingtrance", "trance"),
    ("vocaltrance", "trance"),
    ("numetal", "metal"),
    ("smoothjazz", "jazz"),
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
    ID3V1_GENRES.get(digits.parse::<usize>().ok()?).copied()
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

/// Known genres by key, with their display names from [`ID3V1_GENRES`].
fn known_genres() -> &'static HashMap<String, &'static str> {
    static KNOWN: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    KNOWN.get_or_init(|| {
        ID3V1_GENRES
            .iter()
            .enumerate()
            .filter(|(code, _)| *code != OTHER_CODE)
            .map(|(_, name)| (genre_key(name), *name))
            .collect()
    })
}

/// The known genre `name` stands for (by key, alias or parent genre), or `None`.
pub fn known_genre(name: &str) -> Option<KnownGenre> {
    let mut key = genre_key(name);
    if let Some((_, parent)) = SUBGENRE_PARENTS.iter().find(|(sub, _)| *sub == key) {
        key = parent.to_string();
    }
    let display = known_genres().get(&key)?.to_string();
    Some(KnownGenre { key, display })
}

/// Display names of all known genres, sorted case-insensitively.
pub fn known_genre_names() -> Vec<String> {
    let mut names: Vec<String> = known_genres().values().map(|n| n.to_string()).collect();
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
        assert_eq!(genre_key("R&B"), "rhythmandblues");
        assert_eq!(genre_key("RnB"), "rhythmandblues");
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
    fn aliases_map_to_names_from_the_list() {
        let cases = [
            ("dnb", "Drum & bass"),
            ("D&B", "Drum & bass"),
            ("drum n bass", "Drum & bass"),
            ("Drum'n'Bass", "Drum & bass"),
            ("hip hop", "Hip-hop"),
            ("HipHop", "Hip-hop"),
            ("Rap/Hip-Hop", "Hip-hop"),
            ("rnb", "Rhythm and blues"),
            ("R&B", "Rhythm and blues"),
            ("R'n'B", "Rhythm and blues"),
            ("chill out", "Chillout"),
            ("Chill-Out", "Chillout"),
            ("psy trance", "Psytrance"),
            ("Psy-Trance", "Psytrance"),
            ("trip hop", "Trip-hop"),
            ("synth-pop", "Synthpop"),
            ("Rock \u{2019}n\u{2019} Roll", "Rock 'n' roll"),
            ("rock n roll", "Rock 'n' roll"),
            ("classic rock", "Classic rock"),
            ("TRANCE", "Trance"),
            ("Goa", "Goa music"),
            ("Jungle", "Jungle music"),
            ("World", "World music"),
        ];
        for (name, want) in cases {
            assert_eq!(known(name).as_deref(), Some(want), "{name:?}");
        }
        let dnb = known_genre("dnb").unwrap();
        assert_eq!(dnb.key, genre_key("Drum & bass"));
    }

    #[test]
    fn common_subgenres_go_to_their_parent() {
        for (name, want) in [
            ("Deep House", "House"),
            ("progressive house", "House"),
            ("Tech-House", "House"),
            ("Progressive Trance", "Trance"),
            ("Uplifting Trance", "Trance"),
            ("Nu Metal", "Metal"),
            ("Nu-Metal", "Metal"),
        ] {
            let g = known_genre(name).unwrap();
            assert_eq!(g.display, want, "{name}");
            assert_eq!(g.key, genre_key(want), "{name}");
        }
    }

    #[test]
    fn the_known_list_is_exactly_the_id3v1_list_without_other() {
        assert_eq!(ID3V1_GENRES[0], "Blues");
        assert_eq!(ID3V1_GENRES[12], "Other");
        assert_eq!(ID3V1_GENRES[148], "Christmas");
        assert_eq!(ID3V1_GENRES[191], "Psybient");
        assert_eq!(known(&split_genres("(31)")[0]).as_deref(), Some("Trance"));
        assert_eq!(known(&split_genres("12")[0]), None, "12 is Other");
        let names = known_genre_names();
        assert_eq!(names.len(), 191);
        let mut expected: Vec<&str> = ID3V1_GENRES
            .iter()
            .copied()
            .filter(|n| *n != "Other")
            .collect();
        expected.sort_by_key(|s| s.to_lowercase());
        assert_eq!(names, expected, "verbatim names, sorted");
        // Names not in the list are not genres, however common.
        for n in ["K-Pop", "Hardstyle", "Reggaeton", "Trap"] {
            assert_eq!(known(n), None, "{n}");
        }
    }

    #[test]
    fn known_names_are_unique_by_key_and_canonical() {
        let names = known_genre_names();
        let keys: std::collections::HashSet<String> = names.iter().map(|n| genre_key(n)).collect();
        assert_eq!(keys.len(), names.len(), "one display per key");
        for n in &names {
            assert_eq!(known(n).as_deref(), Some(n.as_str()), "canonical {n}");
        }
    }
}
