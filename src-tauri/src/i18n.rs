//! Translations. The texts live in `ui/locales/<lang>.json`, shared with the UI: the
//! backend compiles them in for the tray and its error texts, and hands them to the
//! UI through `get_locale`. English is the default and the fallback.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde::Serialize;
use serde_json::{Map, Value};

pub const DEFAULT_LANGUAGE: &str = "en";

/// Every language the app ships, by code. To add one, add its JSON file here.
const LOCALES: &[(&str, &str)] = &[
    ("en", include_str!("../../ui/locales/en.json")),
    ("hr", include_str!("../../ui/locales/hr.json")),
];

fn tables() -> &'static HashMap<&'static str, Map<String, Value>> {
    static TABLES: OnceLock<HashMap<&'static str, Map<String, Value>>> = OnceLock::new();
    TABLES.get_or_init(|| {
        LOCALES
            .iter()
            .map(|(code, json)| {
                let table = serde_json::from_str(json)
                    .unwrap_or_else(|e| panic!("ui/locales/{code}.json: {e}"));
                (*code, table)
            })
            .collect()
    })
}

static CURRENT: RwLock<String> = RwLock::new(String::new());

/// The app's language now (set from the settings at start and on change).
pub fn current() -> String {
    let lang = CURRENT.read().unwrap_or_else(|e| e.into_inner());
    if lang.is_empty() {
        DEFAULT_LANGUAGE.to_string()
    } else {
        lang.clone()
    }
}

pub fn set_current(lang: &str) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = lang.to_string();
}

/// [`tr`] in the current language.
pub fn t(key: &str) -> String {
    tr(&current(), key)
}

/// [`trf`] in the current language.
pub fn tf(key: &str, params: &[(&str, &str)]) -> String {
    trf(&current(), key, params)
}

pub fn is_supported(lang: &str) -> bool {
    tables().contains_key(lang)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Language {
    pub code: String,
    pub name: String,
}

/// The shipped languages with their own names ("English", "Hrvatski"), by code.
pub fn languages() -> Vec<Language> {
    LOCALES
        .iter()
        .map(|(code, _)| Language {
            code: code.to_string(),
            name: tr(code, "language.name"),
        })
        .collect()
}

/// The texts of `lang`, with English filling any gaps (English for an unknown
/// language). Built once per language.
pub fn locale(lang: &str) -> &'static Map<String, Value> {
    static MERGED: OnceLock<HashMap<&'static str, Map<String, Value>>> = OnceLock::new();
    let merged = MERGED.get_or_init(|| {
        tables()
            .iter()
            .map(|(code, own)| {
                let mut all = tables()[DEFAULT_LANGUAGE].clone();
                all.extend(own.clone());
                (*code, all)
            })
            .collect()
    });
    merged
        .get(lang)
        .unwrap_or_else(|| &merged[DEFAULT_LANGUAGE])
}

/// The text for `key` in `lang` (English if missing there, else the key itself).
pub fn tr(lang: &str, key: &str) -> String {
    [lang, DEFAULT_LANGUAGE]
        .iter()
        .filter_map(|l| tables().get(l)?.get(key)?.as_str())
        .next()
        .unwrap_or(key)
        .to_string()
}

/// [`tr`] with `{name}` placeholders replaced by `params`.
pub fn trf(lang: &str, key: &str, params: &[(&str, &str)]) -> String {
    params.iter().fold(tr(lang, key), |text, (name, value)| {
        text.replace(&format!("{{{name}}}"), value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_with_params_and_falls_back() {
        assert_eq!(tr("en", "tray.quit"), "Quit");
        assert_eq!(tr("hr", "tray.quit"), "Izlaz");
        assert_eq!(tr("xx", "tray.quit"), "Quit", "unknown language: English");
        assert_eq!(tr("hr", "no.such.key"), "no.such.key");
        assert_eq!(
            trf("en", "err.unknown_server", &[("name", "x")]),
            "Unknown server: x"
        );
        assert_eq!(
            trf("hr", "err.database", &[("error", "e")]),
            "Greška baze: e"
        );
    }

    #[test]
    fn lists_languages_and_merges_locales() {
        assert_eq!(
            languages(),
            vec![
                Language {
                    code: "en".into(),
                    name: "English".into()
                },
                Language {
                    code: "hr".into(),
                    name: "Hrvatski".into()
                },
            ]
        );
        assert!(is_supported("hr") && !is_supported("xx"));
        let hr = locale("hr");
        assert_eq!(hr["tabs.genres"], "Žanrovi");
        assert_eq!(hr.len(), locale("en").len());
        assert_eq!(locale("xx")["tabs.genres"], "Genres");
    }
}
