//! User settings, stored as JSON in the app config directory. Passwords are not part of
//! the settings; they live in the OS keychain (see `credentials`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use smp_core::{LibraryId, DEFAULT_LIBRARY};

use crate::i18n;

pub const FILE_NAME: &str = "settings.json";
/// Quick successive changes (a volume slider) are written at most this often.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// One SMB server. `root` is the music folder inside the share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub share: String,
    pub root: String,
    pub username: String,
    pub domain: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "10.111.100.2".into(),
            share: "F".into(),
            root: "MP3".into(),
            username: String::new(),
            domain: None,
        }
    }
}

/// Where a library's music is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// A settings value, built and cloned rarely: boxing the servers buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum LibrarySource {
    /// An SMB server, with an optional backup server it falls back to.
    Smb {
        primary: ServerConfig,
        secondary: Option<ServerConfig>,
    },
    /// A folder on this computer (an external disk, a mounted network volume…).
    Local { path: String },
}

/// A music library: one source whose tracks the catalog keeps under its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Library {
    pub id: LibraryId,
    /// Blank: the UI shows a default name.
    #[serde(default)]
    pub name: String,
    /// Off: its tracks stay in the catalog but are neither listed nor played.
    #[serde(default = "on")]
    pub enabled: bool,
    pub source: LibrarySource,
}

fn on() -> bool {
    true
}

/// Reads the saved further libraries; an entry that cannot be read (a hand-edited
/// file), or whose id is taken or not above the first library's, is dropped
/// rather than making the whole settings file unreadable.
fn lenient_libraries<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Library>, D::Error> {
    let values = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(v) => v,
        _ => return Ok(Vec::new()),
    };
    let mut libraries: Vec<Library> = Vec::new();
    for value in values {
        match serde_json::from_value::<Library>(value) {
            Ok(l) if l.id > DEFAULT_LIBRARY && libraries.iter().all(|o| o.id != l.id) => {
                libraries.push(l)
            }
            Ok(l) => log::warn!("settings: library id {} dropped (taken)", l.id),
            Err(e) => log::warn!("settings: a library entry dropped: {e}"),
        }
    }
    Ok(libraries)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The first library's (an SMB server's) servers: set up on first run.
    pub primary: ServerConfig,
    pub secondary: Option<ServerConfig>,
    /// The first library's name (blank: a default name) and whether it is on.
    pub default_library_name: String,
    pub default_library_enabled: bool,
    /// Further libraries (ids above [`DEFAULT_LIBRARY`]), in the order shown.
    #[serde(deserialize_with = "lenient_libraries")]
    pub libraries: Vec<Library>,
    /// The highest library id ever given out (see [`Settings::next_library_id`]).
    pub last_library_id: LibraryId,
    /// Linear, 0..1.
    pub volume: f32,
    /// Output device name; `None` = system default.
    pub device: Option<String>,
    pub crossfade_enabled: bool,
    /// 1..=10.
    pub crossfade_secs: u8,
    pub normalize: bool,
    pub notify: bool,
    pub autostart: bool,
    /// Genre keys of the last genre selection (empty = all).
    pub last_selection: Vec<String>,
    /// UI language code (a file in `ui/locales`).
    pub language: String,
    /// A restored playlist plays on start (else it waits, paused, where it stopped).
    pub resume_autoplay: bool,
    /// Where "Look up" goes by default (a click on the now-playing title, the tray).
    #[serde(deserialize_with = "crate::lookup::lenient_service")]
    pub lookup_service: crate::lookup::Service,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            primary: ServerConfig::default(),
            secondary: None,
            default_library_name: String::new(),
            default_library_enabled: true,
            libraries: Vec::new(),
            last_library_id: DEFAULT_LIBRARY,
            volume: 0.7,
            device: None,
            crossfade_enabled: true,
            crossfade_secs: 5,
            normalize: true,
            notify: false,
            autostart: false,
            last_selection: Vec::new(),
            language: i18n::DEFAULT_LANGUAGE.into(),
            resume_autoplay: false,
            lookup_service: crate::lookup::Service::default(),
        }
    }
}

impl Settings {
    /// Brings out-of-range values (e.g. from a hand-edited file) back into range.
    pub fn clamped(mut self) -> Self {
        self.crossfade_secs = self.crossfade_secs.clamp(1, 10);
        if !i18n::is_supported(&self.language) {
            self.language = i18n::DEFAULT_LANGUAGE.into();
        }
        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            Settings::default().volume
        };
        self
    }

    /// Every library, the first (SMB) one first.
    pub fn all_libraries(&self) -> Vec<Library> {
        let first = Library {
            id: DEFAULT_LIBRARY,
            name: self.default_library_name.clone(),
            enabled: self.default_library_enabled,
            source: LibrarySource::Smb {
                primary: self.primary.clone(),
                secondary: self.secondary.clone(),
            },
        };
        std::iter::once(first)
            .chain(self.libraries.iter().cloned())
            .collect()
    }

    pub fn library(&self, id: LibraryId) -> Option<Library> {
        self.all_libraries().into_iter().find(|l| l.id == id)
    }

    /// The libraries switched off.
    pub fn disabled_libraries(&self) -> Vec<LibraryId> {
        self.all_libraries()
            .into_iter()
            .filter(|l| !l.enabled)
            .map(|l| l.id)
            .collect()
    }

    /// The id for a new library: never one given out before (a removed
    /// library's leftovers, in the catalog or the keychain, are never adopted).
    pub fn next_library_id(&self) -> LibraryId {
        self.libraries
            .iter()
            .map(|l| l.id)
            .chain([self.last_library_id, DEFAULT_LIBRARY])
            .max()
            .unwrap_or(DEFAULT_LIBRARY)
            + 1
    }

    /// Records `id` as given out (or found in the catalog): later ids are above it.
    pub fn take_library_id(&mut self, id: LibraryId) {
        self.last_library_id = self.last_library_id.max(id);
    }

    /// "Reset settings to defaults": every setting back to its default except the
    /// language, and the servers and libraries unless `forget_servers`.
    pub fn reset(&self, forget_servers: bool) -> Self {
        let defaults = Settings {
            language: self.language.clone(),
            // Ids are never given out twice, whatever is reset.
            last_library_id: self.next_library_id() - 1,
            ..Settings::default()
        };
        if forget_servers {
            defaults
        } else {
            Settings {
                primary: self.primary.clone(),
                secondary: self.secondary.clone(),
                default_library_name: self.default_library_name.clone(),
                default_library_enabled: self.default_library_enabled,
                libraries: self.libraries.clone(),
                ..defaults
            }
        }
    }

    /// Reads settings from `path`; `None` if the file does not exist or cannot be
    /// parsed (then it is renamed to `settings.json.bad-<unix time>`).
    pub fn load(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("cannot read {}: {e}", path.display());
                return None;
            }
        };
        match serde_json::from_str::<Settings>(&text) {
            Ok(s) => Some(s.clamped()),
            Err(e) => {
                // Kept aside (the libraries in it would otherwise be lost for
                // good at the next save).
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let mut aside = path.as_os_str().to_owned();
                aside.push(format!(".bad-{secs}"));
                let aside = PathBuf::from(aside);
                match std::fs::rename(path, &aside) {
                    Ok(()) => log::warn!(
                        "invalid {} ({e}): kept as {}, starting from defaults",
                        path.display(),
                        aside.display()
                    ),
                    Err(re) => log::warn!(
                        "invalid {} ({e}), and it cannot be kept aside: {re}",
                        path.display()
                    ),
                }
                None
            }
        }
    }

    /// Writes atomically (temp file + rename), creating the directory if needed.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

/// The live settings plus their file. Small changes are saved in the background,
/// debounced; [`SettingsStore::replace_and_save`] and [`SettingsStore::flush`] write
/// at once. Every write happens under the settings lock, so the file never goes back
/// to an older state.
pub struct SettingsStore {
    shared: Arc<Shared>,
    dirty: Mutex<Sender<()>>,
}

struct Shared {
    path: PathBuf,
    current: Mutex<Settings>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Settings> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save_locked(&self, s: &Settings) -> io::Result<()> {
        s.save(&self.path)
    }
}

impl SettingsStore {
    pub fn new(path: PathBuf, initial: Settings) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            path,
            current: Mutex::new(initial),
        });
        let (tx, rx) = mpsc::channel::<()>();
        let saver = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name("settings-saver".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    std::thread::sleep(SAVE_DEBOUNCE);
                    while rx.try_recv().is_ok() {}
                    let Some(shared) = saver.upgrade() else {
                        return;
                    };
                    let s = shared.lock();
                    if let Err(e) = shared.save_locked(&s) {
                        log::warn!("cannot save settings: {e}");
                    }
                }
            })?;
        Ok(Self {
            shared,
            dirty: Mutex::new(tx),
        })
    }

    pub fn get(&self) -> Settings {
        self.shared.lock().clone()
    }

    /// Changes the settings, runs `apply` with the result while still holding the
    /// lock (so whatever it applies happens in the same order as the changes), and
    /// schedules a save.
    pub fn update(
        &self,
        change: impl FnOnce(&mut Settings),
        apply: impl FnOnce(&Settings),
    ) -> Settings {
        let mut s = self.shared.lock();
        change(&mut s);
        *s = s.clone().clamped();
        apply(&s);
        let _ = self
            .dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(());
        s.clone()
    }

    /// Replaces all settings and writes them now. `apply` runs under the lock with
    /// the old and the new settings.
    pub fn replace_and_save(
        &self,
        new: Settings,
        apply: impl FnOnce(&Settings, &Settings),
    ) -> io::Result<Settings> {
        let mut s = self.shared.lock();
        let new = new.clamped();
        self.shared.save_locked(&new)?;
        apply(&s, &new);
        *s = new;
        Ok(s.clone())
    }

    /// Makes new settings from the current ones (`change`) and writes them now,
    /// all under the lock, so nothing saved meanwhile is lost. `apply` runs under
    /// the lock with the old and the new settings.
    pub fn change_and_save(
        &self,
        change: impl FnOnce(&Settings) -> Settings,
        apply: impl FnOnce(&Settings, &Settings),
    ) -> io::Result<Settings> {
        let mut s = self.shared.lock();
        let new = change(&s).clamped();
        self.shared.save_locked(&new)?;
        apply(&s, &new);
        *s = new;
        Ok(s.clone())
    }

    /// Writes the current settings now (e.g. on exit, ahead of a pending save).
    pub fn flush(&self) -> io::Result<()> {
        let s = self.shared.lock();
        self.shared.save_locked(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_match_spec() {
        let s = Settings::default();
        assert_eq!(s.primary.host, "10.111.100.2");
        assert_eq!(s.primary.share, "F");
        assert_eq!(s.primary.root, "MP3");
        assert_eq!(s.secondary, None);
        assert!(s.crossfade_enabled);
        assert_eq!(s.crossfade_secs, 5);
        assert!(s.normalize);
        assert!(!s.notify);
        assert!(!s.autostart);
        assert_eq!(s.volume, 0.7);
        assert_eq!(s.device, None);
        assert!(s.last_selection.is_empty());
        assert_eq!(s.language, "en");
        assert!(!s.resume_autoplay);
        let old: Settings = serde_json::from_str(r#"{"volume": 0.5}"#).unwrap();
        assert!(!old.resume_autoplay, "settings from before the option");
    }

    #[test]
    fn language_defaults_to_english_and_must_be_shipped() {
        let old: Settings = serde_json::from_str(r#"{"volume": 0.5}"#).unwrap();
        assert_eq!(
            old.language, "en",
            "settings from before the language option"
        );
        let hr: Settings = serde_json::from_str(r#"{"language": "hr"}"#).unwrap();
        assert_eq!(hr.clamped().language, "hr");
        let xx: Settings = serde_json::from_str(r#"{"language": "xx"}"#).unwrap();
        assert_eq!(xx.clamped().language, "en");
    }

    fn customised() -> Settings {
        Settings {
            primary: ServerConfig {
                host: "10.0.0.9".into(),
                username: "music".into(),
                ..ServerConfig::default()
            },
            secondary: Some(ServerConfig {
                host: "10.111.0.2".into(),
                username: "music".into(),
                ..ServerConfig::default()
            }),
            default_library_name: String::new(),
            default_library_enabled: true,
            libraries: Vec::new(),
            last_library_id: DEFAULT_LIBRARY,
            volume: 0.2,
            device: Some("USB DAC".into()),
            crossfade_enabled: false,
            crossfade_secs: 9,
            normalize: false,
            notify: true,
            autostart: true,
            last_selection: vec!["rock".into()],
            language: "hr".into(),
            resume_autoplay: true,
            lookup_service: crate::lookup::Service::Genius,
        }
    }

    #[test]
    fn lookup_defaults_to_last_fm_and_a_bad_value_keeps_the_file() {
        use crate::lookup::Service;
        assert_eq!(Settings::default().lookup_service, Service::Lastfm);
        let old: Settings = serde_json::from_str(r#"{"volume": 0.5}"#).unwrap();
        assert_eq!(
            old.lookup_service,
            Service::Lastfm,
            "from before the option"
        );
        let bad: Settings =
            serde_json::from_str(r#"{"volume": 0.5, "lookup_service": "nope"}"#).unwrap();
        assert_eq!((bad.volume, bad.lookup_service), (0.5, Service::Lastfm));
        let set: Settings = serde_json::from_str(r#"{"lookup_service": "discogs"}"#).unwrap();
        assert_eq!(set.lookup_service, Service::Discogs);
    }

    #[test]
    fn reset_restores_defaults_keeping_language_and_servers() {
        let old = customised();
        let new = old.reset(false);
        assert_eq!(
            new,
            Settings {
                primary: old.primary.clone(),
                secondary: old.secondary.clone(),
                language: "hr".into(),
                ..Settings::default()
            }
        );
        let forgot = old.reset(true);
        assert_eq!(
            forgot,
            Settings {
                language: "hr".into(),
                ..Settings::default()
            }
        );
    }

    #[test]
    fn settings_clamp_crossfade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for (stored, expected) in [(0u8, 1u8), (15, 10), (7, 7)] {
            let s = Settings {
                crossfade_secs: stored,
                ..Settings::default()
            };
            s.save(&path).unwrap();
            assert_eq!(Settings::load(&path).unwrap().crossfade_secs, expected);
        }
    }

    #[test]
    fn settings_never_serialize_password() {
        let s = Settings {
            secondary: Some(ServerConfig {
                host: "10.111.0.2".into(),
                username: "music".into(),
                domain: Some("WORKGROUP".into()),
                ..ServerConfig::default()
            }),
            ..Settings::default()
        };
        let json = serde_json::to_string(&s).unwrap().to_lowercase();
        assert!(!json.contains("password"), "{json}");
    }

    #[test]
    fn missing_or_corrupt_file_loads_none_and_partial_file_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(Settings::load(&path), None);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load(&path), None);
        // Kept aside for the user, not overwritten by the next save.
        assert!(!path.exists());
        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("settings.json.bad-"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), "{ not json");
        std::fs::write(&path, r#"{"volume": 0.25, "primary": {"username": "u"}}"#).unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.volume, 0.25);
        assert_eq!(s.primary.username, "u");
        assert_eq!(s.primary.host, "10.111.100.2");
        assert_eq!(s.crossfade_secs, 5);
    }

    #[test]
    fn store_saves_debounced_and_flushes_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let store = SettingsStore::new(path.clone(), Settings::default()).unwrap();
        let mut applied = Vec::new();
        for i in 1..=50 {
            store.update(|s| s.volume = i as f32 / 100.0, |s| applied.push(s.volume));
        }
        assert_eq!(applied.len(), 50);
        assert_eq!(store.get().volume, 0.5);
        assert!(!path.exists(), "slider ticks are not written one by one");
        std::thread::sleep(SAVE_DEBOUNCE * 2);
        assert_eq!(Settings::load(&path).unwrap().volume, 0.5);

        store.update(|s| s.crossfade_secs = 99, |_| {});
        assert_eq!(store.get().crossfade_secs, 10, "clamped");
        store.flush().unwrap();
        assert_eq!(Settings::load(&path).unwrap().crossfade_secs, 10);

        let mut seen = None;
        let new = Settings {
            notify: true,
            ..store.get()
        };
        store
            .replace_and_save(new, |old, new| seen = Some((old.notify, new.notify)))
            .unwrap();
        assert_eq!(seen, Some((false, true)));
        assert!(Settings::load(&path).unwrap().notify);
    }

    fn local(id: LibraryId, path: &str) -> Library {
        Library {
            id,
            name: format!("Disk {id}"),
            enabled: true,
            source: LibrarySource::Local { path: path.into() },
        }
    }

    #[test]
    fn a_fresh_install_has_exactly_the_smb_library() {
        let s = Settings::default();
        let libs = s.all_libraries();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].id, smp_core::DEFAULT_LIBRARY);
        assert!(libs[0].enabled);
        assert_eq!(
            libs[0].source,
            LibrarySource::Smb {
                primary: ServerConfig::default(),
                secondary: None
            }
        );
        assert_eq!(s.next_library_id(), 2);
        // Settings from before libraries read the same.
        let old: Settings =
            serde_json::from_str(r#"{"primary": {"username": "music"}, "volume": 0.5}"#).unwrap();
        assert_eq!(old.all_libraries().len(), 1);
        assert_eq!(old.primary.username, "music");
    }

    #[test]
    fn libraries_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let s = Settings {
            default_library_name: "NAS".into(),
            default_library_enabled: false,
            libraries: vec![
                local(2, "/Volumes/Glazba čćž 🎵"),
                Library {
                    id: 5,
                    name: "Office".into(),
                    enabled: false,
                    source: LibrarySource::Smb {
                        primary: ServerConfig {
                            host: "10.0.0.5".into(),
                            username: "u".into(),
                            ..ServerConfig::default()
                        },
                        secondary: Some(ServerConfig::default()),
                    },
                },
            ],
            ..customised()
        };
        s.save(&path).unwrap();
        let back = Settings::load(&path).unwrap();
        assert_eq!(back, s);
        let ids: Vec<LibraryId> = back.all_libraries().iter().map(|l| l.id).collect();
        assert_eq!(ids, [1, 2, 5]);
        assert_eq!(back.all_libraries()[0].name, "NAS");
        assert_eq!(back.disabled_libraries(), [1, 5]);
        assert_eq!(back.next_library_id(), 6);
        assert_eq!(back.library(2).unwrap(), local(2, "/Volumes/Glazba čćž 🎵"));
        assert!(back.library(3).is_none());
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains(r#""type": "local""#), "{json}");
        assert!(!json.to_lowercase().contains("password"));
    }

    #[test]
    fn a_bad_library_entry_is_dropped_not_the_whole_file() {
        let s: Settings = serde_json::from_str(
            r#"{"volume": 0.3, "libraries": [
                {"id": 2, "source": {"type": "local", "path": "/a"}},
                {"id": 3, "source": {"type": "ftp"}},
                {"id": 1, "source": {"type": "local", "path": "/taken"}},
                {"id": 2, "source": {"type": "local", "path": "/again"}},
                "nonsense"
            ]}"#,
        )
        .unwrap();
        assert_eq!(s.volume, 0.3);
        assert_eq!(s.libraries.len(), 1);
        assert_eq!(s.libraries[0].id, 2);
        assert!(s.libraries[0].enabled, "on unless saved off");
        assert_eq!(s.libraries[0].name, "");
    }

    #[test]
    fn reset_keeps_libraries_unless_servers_are_forgotten() {
        let s = Settings {
            libraries: vec![local(2, "/x")],
            default_library_name: "NAS".into(),
            ..customised()
        };
        let kept = s.reset(false);
        assert_eq!(kept.libraries, s.libraries);
        assert_eq!(kept.default_library_name, "NAS");
        let forgot = s.reset(true);
        assert!(forgot.libraries.is_empty());
        assert_eq!(forgot.all_libraries().len(), 1);
    }

    #[test]
    fn library_ids_are_never_used_twice() {
        let mut s = Settings {
            libraries: vec![local(2, "/a"), local(3, "/b")],
            ..Settings::default()
        };
        assert_eq!(s.next_library_id(), 4);
        s.take_library_id(4);
        s.libraries.retain(|l| l.id < 3);
        assert_eq!(s.next_library_id(), 5, "3 and 4 were given out");
        // Rows of a library the settings lost (in the catalog) are not adopted.
        s.take_library_id(9);
        assert_eq!(s.next_library_id(), 10);
        s.take_library_id(2);
        assert_eq!(s.next_library_id(), 10, "never goes back");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.next_library_id(), 10);
        assert_eq!(s.reset(true).next_library_id(), 10, "kept by a reset");
        let old: Settings = serde_json::from_str(r#"{"volume": 0.5}"#).unwrap();
        assert_eq!(old.next_library_id(), 2);
    }
}
