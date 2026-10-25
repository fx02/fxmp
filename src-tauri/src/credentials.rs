//! SMB passwords in the OS keychain (service `smb-music-player`), one per server of
//! a library: account `library-{id}/{host}/{username}` (see [`account`]). Passwords
//! are never written to files or logs. A debug build with `SMP_DATA_DIR` set uses
//! another service (see `devdata`). Local-folder libraries have no password and
//! never touch the keychain.
//!
//! Before libraries, the account was `{host}/{username}`; [`migrate_legacy`] moves
//! those entries to the first library's accounts.

use smp_core::{LibraryId, DEFAULT_LIBRARY};

use crate::settings::{ServerConfig, Settings};

pub const SERVICE: &str = "smb-music-player";

/// The keychain account of server `host`/`username` of library `library`. The
/// host and user are part of it so a password is never sent to another server.
pub fn account(library: LibraryId, host: &str, username: &str) -> String {
    format!("library-{library}/{}/{}", host.trim(), username.trim())
}

/// The account a server's password had before libraries.
fn legacy_account(host: &str, username: &str) -> String {
    format!("{}/{}", host.trim(), username.trim())
}

/// Where passwords are kept, by account; the app uses [`Keychain`], tests an
/// in-memory store.
pub trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Option<String>;
    fn set(&self, account: &str, password: &str) -> Result<(), String>;
    /// Removes the stored password; no password stored is not an error.
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The stored password of `server` of library `library`. The first library falls
/// back to the entry from before libraries, should its move have failed.
pub fn server_password(
    creds: &dyn CredentialStore,
    library: LibraryId,
    server: &ServerConfig,
) -> Option<String> {
    creds
        .get(&account(library, &server.host, &server.username))
        .or_else(|| {
            (library == DEFAULT_LIBRARY)
                .then(|| creds.get(&legacy_account(&server.host, &server.username)))
                .flatten()
        })
}

/// Stores a typed password of `server` of library `library`.
pub fn set_server_password(
    creds: &dyn CredentialStore,
    library: LibraryId,
    server: &ServerConfig,
    password: &str,
) -> Result<(), String> {
    creds.set(&account(library, &server.host, &server.username), password)
}

/// Deletes the stored password of `server` of library `library` (for the first
/// library also the entry from before libraries). Nothing to do without a user.
pub fn delete_server_password(
    creds: &dyn CredentialStore,
    library: LibraryId,
    server: &ServerConfig,
) -> Result<(), String> {
    if server.username.trim().is_empty() {
        return Ok(());
    }
    creds.delete(&account(library, &server.host, &server.username))?;
    if library == DEFAULT_LIBRARY {
        creds.delete(&legacy_account(&server.host, &server.username))?;
    }
    Ok(())
}

/// Moves the first library's passwords from their accounts before libraries to
/// the library's: each is written to the new account, read back, and only when it
/// reads back the same is the old entry deleted. Returns how many moved. Safe to
/// run on every start: once moved, there is nothing to read.
pub fn migrate_legacy(creds: &dyn CredentialStore, settings: &Settings) -> usize {
    let mut moved = 0;
    for server in std::iter::once(&settings.primary).chain(settings.secondary.as_ref()) {
        if server.username.trim().is_empty() {
            continue;
        }
        let old = legacy_account(&server.host, &server.username);
        let Some(password) = creds.get(&old) else {
            continue;
        };
        let new = account(DEFAULT_LIBRARY, &server.host, &server.username);
        if creds.get(&new).is_none() {
            if let Err(e) = creds.set(&new, &password) {
                log::warn!(
                    "keychain: password of {} not moved: {e}",
                    server.host.trim()
                );
                continue;
            }
        }
        if creds.get(&new).as_deref() != Some(password.as_str()) {
            log::warn!(
                "keychain: the moved password of {} does not read back the same; the old \
                 entry is kept",
                server.host.trim()
            );
            continue;
        }
        match creds.delete(&old) {
            Ok(()) => {
                moved += 1;
                log::info!(
                    "keychain: password of {} moved to its library",
                    server.host.trim()
                );
            }
            Err(e) => log::warn!("keychain: old entry of {} kept: {e}", server.host.trim()),
        }
    }
    moved
}

/// The OS keychain via `keyring`, under keychain service `service`.
pub struct Keychain {
    pub service: &'static str,
}

impl CredentialStore for Keychain {
    /// The stored password, or `None` if there is none (or the keychain cannot be read).
    fn get(&self, account: &str) -> Option<String> {
        let entry = match keyring::Entry::new(self.service, account) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("keychain entry: {e}");
                return None;
            }
        };
        match entry.get_password() {
            Ok(pw) => Some(pw),
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                log::warn!("cannot read a keychain password: {e}");
                None
            }
        }
    }

    fn set(&self, account: &str, password: &str) -> Result<(), String> {
        keyring::Entry::new(self.service, account)
            .and_then(|e| e.set_password(password))
            .map_err(|e| crate::i18n::tf("err.keychain", &[("error", &e.to_string())]))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        let result = keyring::Entry::new(self.service, account).and_then(|e| e.delete_credential());
        match result {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(crate::i18n::tf(
                "err.keychain_delete",
                &[("error", &e.to_string())],
            )),
        }
    }
}

/// An in-memory store that counts how often it is used.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl MemoryStore {
    fn count(&self) {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// The accounts stored, sorted.
    pub fn accounts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.entries.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    /// Stores the password of `host`/`username` for library `library`.
    pub fn put(&self, library: LibraryId, host: &str, username: &str, password: &str) {
        self.set(&account(library, host, username), password)
            .unwrap();
    }
}

#[cfg(test)]
impl CredentialStore for MemoryStore {
    fn get(&self, account: &str) -> Option<String> {
        self.count();
        self.entries.lock().unwrap().get(account).cloned()
    }

    fn set(&self, account: &str, password: &str) -> Result<(), String> {
        self.count();
        self.entries
            .lock()
            .unwrap()
            .insert(account.to_string(), password.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        self.count();
        self.entries.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_deletes() {
        let m = MemoryStore::default();
        m.set("a", "pw").unwrap();
        m.delete("a").unwrap();
        assert_eq!(m.get("a"), None);
        m.delete("a").unwrap();
    }

    #[test]
    fn accounts_are_per_library_host_and_user() {
        assert_eq!(SERVICE, "smb-music-player");
        assert_eq!(
            account(1, " 10.111.100.2 ", "music "),
            "library-1/10.111.100.2/music"
        );
        assert_ne!(account(1, "h", "u"), account(2, "h", "u"));
    }

    fn two_servers() -> Settings {
        Settings {
            primary: ServerConfig {
                host: "10.111.100.2".into(),
                username: "music".into(),
                ..ServerConfig::default()
            },
            secondary: Some(ServerConfig {
                host: "10.111.0.2".into(),
                username: "backup".into(),
                ..ServerConfig::default()
            }),
            ..Settings::default()
        }
    }

    #[test]
    fn old_entries_move_to_the_first_library_and_go_once_verified() {
        let m = MemoryStore::default();
        m.set("10.111.100.2/music", "pw1").unwrap();
        m.set("10.111.0.2/backup", "pw2").unwrap();
        m.set("10.9.9.9/someone", "other").unwrap();
        let s = two_servers();
        assert_eq!(migrate_legacy(&m, &s), 2);
        assert_eq!(
            m.accounts(),
            [
                "10.9.9.9/someone",
                "library-1/10.111.0.2/backup",
                "library-1/10.111.100.2/music"
            ],
            "an entry of no configured server is left alone"
        );
        assert_eq!(server_password(&m, 1, &s.primary).as_deref(), Some("pw1"));
        assert_eq!(
            server_password(&m, 1, s.secondary.as_ref().unwrap()).as_deref(),
            Some("pw2")
        );
        assert_eq!(migrate_legacy(&m, &s), 0, "nothing left to move");
    }

    /// Writes go nowhere: the moved password never reads back.
    #[derive(Default)]
    struct ReadOnly(MemoryStore);
    impl CredentialStore for ReadOnly {
        fn get(&self, account: &str) -> Option<String> {
            self.0.get(account)
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<(), String> {
            self.0.delete(account)
        }
    }

    #[test]
    fn an_entry_that_does_not_read_back_is_not_deleted() {
        let m = ReadOnly::default();
        m.0.set("10.111.100.2/music", "pw1").unwrap();
        let s = two_servers();
        assert_eq!(migrate_legacy(&m, &s), 0);
        assert_eq!(m.0.accounts(), ["10.111.100.2/music"], "kept");
        assert_eq!(
            server_password(&m, 1, &s.primary).as_deref(),
            Some("pw1"),
            "still used"
        );
        assert_eq!(
            server_password(&m, 2, &s.primary),
            None,
            "only by library 1"
        );
    }

    #[test]
    fn a_partly_moved_entry_is_finished() {
        let m = MemoryStore::default();
        m.set("10.111.100.2/music", "pw1").unwrap();
        m.put(1, "10.111.100.2", "music", "pw1");
        assert_eq!(migrate_legacy(&m, &two_servers()), 1);
        assert_eq!(m.accounts(), ["library-1/10.111.100.2/music"]);
    }

    #[test]
    fn deleting_a_server_password_also_drops_its_old_entry() {
        let m = MemoryStore::default();
        let s = two_servers();
        m.set("10.111.100.2/music", "old").unwrap();
        set_server_password(&m, 1, &s.primary, "new").unwrap();
        m.put(2, "10.111.100.2", "music", "two");
        delete_server_password(&m, 1, &s.primary).unwrap();
        assert_eq!(m.accounts(), ["library-2/10.111.100.2/music"]);
    }
}
