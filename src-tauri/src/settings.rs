//! User settings, stored as JSON in the app config directory. Passwords are not part of
//! the settings; they live in the OS keychain (see `credentials`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub primary: ServerConfig,
    pub secondary: Option<ServerConfig>,
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            primary: ServerConfig::default(),
            secondary: None,
            volume: 0.7,
            device: None,
            crossfade_enabled: true,
            crossfade_secs: 5,
            normalize: true,
            notify: false,
            autostart: false,
            last_selection: Vec::new(),
        }
    }
}

impl Settings {
    /// Brings out-of-range values (e.g. from a hand-edited file) back into range.
    pub fn clamped(mut self) -> Self {
        self.crossfade_secs = self.crossfade_secs.clamp(1, 10);
        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            Settings::default().volume
        };
        self
    }

    /// Reads settings from `path`; `None` if the file does not exist or cannot be parsed.
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
                log::warn!("ignoring invalid {}: {e}", path.display());
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
}
