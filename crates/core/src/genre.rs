//! Genre splitting and normalization.

use lofty::id3::v1::GENRES;

/// Aliases applied to the normalized key (already lowercase alphanumeric).
pub const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("dnb", "drumandbass"),
    ("drumnbass", "drumandbass"),
    ("hiphop", "hiphop"),
    ("rnb", "randb"),
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

/// Most common original spelling; ties broken alphabetically (first wins).
pub fn pick_display(spellings: &[(String, usize)]) -> String {
    spellings
        .iter()
        .min_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)))
        .map(|(s, _)| s.clone())
        .unwrap_or_default()
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

    #[test]
    fn display_prefers_most_common_spelling() {
        assert_eq!(
            pick_display(&[("DnB".into(), 3), ("Drum & Bass".into(), 40)]),
            "Drum & Bass"
        );
        assert_eq!(pick_display(&[("b".into(), 2), ("a".into(), 2)]), "a");
    }
}
