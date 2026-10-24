//! SMB passwords in the OS keychain (service `smb-music-player`, account
//! `{host}/{username}`). Passwords are never written to files or logs.

pub const SERVICE: &str = "smb-music-player";

pub fn account(host: &str, username: &str) -> String {
    format!("{host}/{username}")
}

/// Where passwords are kept; the app uses [`Keychain`], tests an in-memory store.
pub trait CredentialStore: Send + Sync {
    fn get(&self, host: &str, username: &str) -> Option<String>;
    fn set(&self, host: &str, username: &str, password: &str) -> Result<(), String>;
    /// Removes the stored password; no password stored is not an error.
    fn delete(&self, host: &str, username: &str) -> Result<(), String>;
}

/// The OS keychain via `keyring`.
pub struct Keychain;

impl CredentialStore for Keychain {
    fn get(&self, host: &str, username: &str) -> Option<String> {
        get(host, username)
    }

    fn set(&self, host: &str, username: &str, password: &str) -> Result<(), String> {
        set(host, username, password)
    }

    fn delete(&self, host: &str, username: &str) -> Result<(), String> {
        delete(host, username)
    }
}

/// The stored password, or `None` if there is none (or the keychain cannot be read).
pub fn get(host: &str, username: &str) -> Option<String> {
    let entry = match keyring::Entry::new(SERVICE, &account(host, username)) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("keychain entry for {host}: {e}");
            return None;
        }
    };
    match entry.get_password() {
        Ok(pw) => Some(pw),
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            log::warn!("cannot read keychain password for {host}: {e}");
            None
        }
    }
}

pub fn set(host: &str, username: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, &account(host, username))
        .and_then(|e| e.set_password(password))
        .map_err(|e| crate::i18n::tf("err.keychain", &[("error", &e.to_string())]))
}

pub fn delete(host: &str, username: &str) -> Result<(), String> {
    let result =
        keyring::Entry::new(SERVICE, &account(host, username)).and_then(|e| e.delete_credential());
    match result {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(crate::i18n::tf(
            "err.keychain_delete",
            &[("error", &e.to_string())],
        )),
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore(std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl CredentialStore for MemoryStore {
    fn get(&self, host: &str, username: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .get(&account(host, username))
            .cloned()
    }

    fn set(&self, host: &str, username: &str, password: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(account(host, username), password.to_string());
        Ok(())
    }

    fn delete(&self, host: &str, username: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(&account(host, username));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_deletes() {
        let m = MemoryStore::default();
        m.set("h", "u", "pw").unwrap();
        m.delete("h", "u").unwrap();
        assert_eq!(m.get("h", "u"), None);
        m.delete("h", "u").unwrap();
    }

    #[test]
    fn account_is_host_slash_username() {
        assert_eq!(SERVICE, "smb-music-player");
        assert_eq!(account("10.111.100.2", "music"), "10.111.100.2/music");
    }
}
