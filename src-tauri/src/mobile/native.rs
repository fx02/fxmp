//! The app's Android side (`FxmpPlugin.kt`): passwords encrypted with an
//! Android Keystore key, and the playback service that keeps the music on with
//! the screen off.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager, Wry};

use crate::credentials::CredentialStore;

/// The Kotlin plugin, registered before the app's state is built.
pub struct Native(pub PluginHandle<Wry>);

pub fn plugin() -> TauriPlugin<Wry> {
    Builder::new("fxmp")
        .setup(|app, api| {
            let handle = api.register_android_plugin("hr.pvt.fxmp", "FxmpPlugin")?;
            app.manage(Native(handle));
            Ok(())
        })
        .build()
}

#[derive(Serialize)]
struct Plain<'a> {
    text: &'a str,
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    data: String,
}

/// Passwords in `path` (account → ciphertext), each sealed by a Keystore key
/// that never leaves the device's secure hardware.
pub struct KeystoreStore {
    plugin: PluginHandle<Wry>,
    path: PathBuf,
    /// One read-change-write of the file at a time.
    file: Mutex<()>,
}

impl KeystoreStore {
    pub fn new(app: &tauri::AppHandle, path: PathBuf) -> Self {
        Self {
            plugin: app.state::<Native>().0.clone(),
            path,
            file: Mutex::new(()),
        }
    }

    fn read(&self) -> BTreeMap<String, String> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write(&self, map: &BTreeMap<String, String>) -> Result<(), String> {
        let tmp = self.path.with_extension("tmp");
        let json = serde_json::to_vec(map).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json)
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| crate::i18n::tf("err.keychain", &[("error", &e.to_string())]))
    }
}

impl CredentialStore for KeystoreStore {
    fn get(&self, account: &str) -> Option<String> {
        let data = {
            let _file = self.file.lock().unwrap_or_else(|e| e.into_inner());
            self.read().remove(account)?
        };
        match self
            .plugin
            .run_mobile_plugin::<Decrypted>("decrypt", Sealed { data })
        {
            Ok(p) => Some(p.text),
            Err(e) => {
                log::warn!("cannot read a stored password: {e}");
                None
            }
        }
    }

    fn set(&self, account: &str, password: &str) -> Result<(), String> {
        let sealed: Sealed = self
            .plugin
            .run_mobile_plugin("encrypt", Plain { text: password })
            .map_err(|e| crate::i18n::tf("err.keychain", &[("error", &e.to_string())]))?;
        let _file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = self.read();
        map.insert(account.to_string(), sealed.data);
        self.write(&map)
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        let _file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = self.read();
        if map.remove(account).is_some() {
            self.write(&map)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Decrypted {
    text: String,
}
