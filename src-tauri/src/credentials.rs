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
        .map_err(|e| format!("Cannot save the password in the keychain: {e}"))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_is_host_slash_username() {
        assert_eq!(SERVICE, "smb-music-player");
        assert_eq!(account("10.111.100.2", "music"), "10.111.100.2/music");
    }
}
