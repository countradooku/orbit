//! Plugin secrets: the system keychain, or a private file where there is
//! none.
//!
//! Secrets go to the OS credential store through `keyring-core` (Windows
//! Credential Manager, the macOS Keychain, the Secret Service on Linux),
//! one entry per plugin and key, under a service name tied to this data
//! folder so two Orbit data folders never share secrets.
//!
//! A host without a usable keychain (a headless Linux server, a container,
//! WSL without a Secret Service) keeps them in
//! `<data>/plugins/data/<id>/secrets.json`, readable only by the user
//! (mode 0600 on Unix). That is no stronger than the folder's own
//! permissions, so [`Secrets::protection`] says which applies and the
//! Plugins page shows it.
//!
//! Every keychain call runs on a short-lived thread of its own: the Linux
//! backend drives its own async runtime, which must not start inside the
//! engine's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use keyring_core::{CredentialStore, Entry};

/// Where secrets are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    Keychain,
    File,
}

impl Protection {
    pub fn key(self) -> &'static str {
        match self {
            Protection::Keychain => "keychain",
            Protection::File => "file",
        }
    }
}

pub struct Secrets {
    /// `<data>/plugins/data`.
    dir: PathBuf,
    service: String,
    keychain: bool,
    /// The keychain, found on first use (asking it can take a moment on a
    /// host without one, so not at start).
    store: std::sync::OnceLock<Option<Arc<CredentialStore>>>,
}

impl Secrets {
    /// Opens the keychain when `keychain` is set and it answers within a
    /// few seconds; otherwise secrets go to files under `dir`.
    pub fn open(dir: &Path, keychain: bool) -> Self {
        let service = format!("Orbit plugins {}", &blake3::hash(dir.display().to_string().as_bytes()).to_hex()[..12]);
        Self { dir: dir.to_owned(), service, keychain, store: std::sync::OnceLock::new() }
    }

    fn store(&self) -> &Option<Arc<CredentialStore>> {
        self.store.get_or_init(|| if self.keychain { probe(&self.service) } else { None })
    }

    /// Where secrets go, once known (`None` until the keychain was first
    /// needed).
    pub fn protection(&self) -> Option<Protection> {
        if !self.keychain {
            return Some(Protection::File);
        }
        self.store.get().map(|s| if s.is_some() { Protection::Keychain } else { Protection::File })
    }

    fn file(&self, plugin: &str) -> PathBuf {
        self.dir.join(plugin).join("secrets.json")
    }

    /// Key names only (never values), so removing a plugin can delete its
    /// keychain entries.
    fn index(&self, plugin: &str) -> PathBuf {
        self.dir.join(plugin).join("secret-keys.json")
    }

    fn read_map(path: &Path) -> BTreeMap<String, String> {
        std::fs::read_to_string(path).ok().and_then(|t| sonic_rs::from_str(&t).ok()).unwrap_or_default()
    }

    fn write_map(path: &Path, map: &BTreeMap<String, String>) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = sonic_rs::to_string(map).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, text.as_bytes()).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    /// Whether a secret is stored, without reading it (from the key index,
    /// so the keychain isn't asked).
    pub fn has(&self, plugin: &str, key: &str) -> bool {
        Self::read_map(&self.index(plugin)).contains_key(key) || Self::read_map(&self.file(plugin)).contains_key(key)
    }

    pub fn get(&self, plugin: &str, key: &str) -> Result<Option<String>, String> {
        match self.store() {
            Some(store) => {
                let (store, service, user) = (store.clone(), self.service.clone(), format!("{plugin}/{key}"));
                off_thread(move || match Entry::new_in_store(&store, &service, &user).and_then(|e| e.get_password()) {
                    Ok(v) => Ok(Some(v)),
                    Err(keyring_core::Error::NoEntry) => Ok(None),
                    Err(e) => Err(format!("The keychain refused: {e}")),
                })
            }
            None => Ok(Self::read_map(&self.file(plugin)).get(key).cloned()),
        }
    }

    pub fn set(&self, plugin: &str, key: &str, value: &str) -> Result<(), String> {
        if value.len() > 16 * 1024 {
            return Err("A secret can be at most 16 KiB.".into());
        }
        match self.store() {
            Some(store) => {
                let (store, service, user, value) = (store.clone(), self.service.clone(), format!("{plugin}/{key}"), value.to_owned());
                off_thread(move || {
                    Entry::new_in_store(&store, &service, &user).and_then(|e| e.set_password(&value)).map_err(|e| format!("The keychain refused: {e}"))
                })?;
                let mut index = Self::read_map(&self.index(plugin));
                index.insert(key.to_owned(), String::new());
                Self::write_map(&self.index(plugin), &index)
            }
            None => {
                let mut map = Self::read_map(&self.file(plugin));
                map.insert(key.to_owned(), value.to_owned());
                Self::write_map(&self.file(plugin), &map)
            }
        }
    }

    pub fn delete(&self, plugin: &str, key: &str) -> Result<(), String> {
        match self.store() {
            Some(store) => {
                let (store, service, user) = (store.clone(), self.service.clone(), format!("{plugin}/{key}"));
                off_thread(move || match Entry::new_in_store(&store, &service, &user).and_then(|e| e.delete_credential()) {
                    Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
                    Err(e) => Err(format!("The keychain refused: {e}")),
                })?;
                let mut index = Self::read_map(&self.index(plugin));
                index.remove(key);
                Self::write_map(&self.index(plugin), &index)
            }
            None => {
                let mut map = Self::read_map(&self.file(plugin));
                map.remove(key);
                Self::write_map(&self.file(plugin), &map)
            }
        }
    }

    /// Deletes every secret of a removed plugin.
    pub fn forget(&self, plugin: &str) {
        let keys: Vec<String> = Self::read_map(&self.index(plugin)).into_keys().collect();
        for key in keys {
            if let Err(e) = self.delete(plugin, &key) {
                log::warn!("plugins: couldn’t delete {plugin}’s secret {key}: {e}");
            }
        }
        let _ = std::fs::remove_file(self.file(plugin));
        let _ = std::fs::remove_file(self.index(plugin));
    }
}

/// The platform's credential store, if it answers.
fn probe(service: &str) -> Option<Arc<CredentialStore>> {
    let service = service.to_owned();
    let found = off_thread_timeout(Duration::from_secs(3), move || {
        let store = platform_store()?;
        match Entry::new_in_store(&store, &service, "orbit-probe").and_then(|e| e.get_password()) {
            Ok(_) | Err(keyring_core::Error::NoEntry) => Some(store),
            Err(e) => {
                log::info!("plugins: the keychain isn’t usable ({e}); secrets go to private files");
                None
            }
        }
    });
    found.flatten()
}

#[cfg(windows)]
fn platform_store() -> Option<Arc<CredentialStore>> {
    windows_native_keyring_store::Store::new().ok().map(|s| s as Arc<CredentialStore>)
}

#[cfg(target_os = "macos")]
fn platform_store() -> Option<Arc<CredentialStore>> {
    apple_native_keyring_store::keychain::Store::new().ok().map(|s| s as Arc<CredentialStore>)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_store() -> Option<Arc<CredentialStore>> {
    zbus_secret_service_keyring_store::Store::new().ok().map(|s| s as Arc<CredentialStore>)
}

#[cfg(not(any(windows, unix)))]
fn platform_store() -> Option<Arc<CredentialStore>> {
    None
}

fn off_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    off_thread_timeout(Duration::from_secs(10), f).unwrap_or_else(|| Err("The keychain didn’t answer.".into()))
}

fn off_thread_timeout<T: Send + 'static>(timeout: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("orbit-keychain".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(timeout).ok()
}

/// Writes a file only its owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        // The data folder is in the user's profile, which only they (and
        // administrators) can read.
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_secrets_round_trip_and_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::open(dir.path(), false);
        assert_eq!(secrets.protection(), Some(Protection::File));
        assert_eq!(secrets.get("p", "token").unwrap(), None);
        secrets.set("p", "token", "s3cret").unwrap();
        assert_eq!(secrets.get("p", "token").unwrap().as_deref(), Some("s3cret"));
        assert_eq!(secrets.get("other", "token").unwrap(), None, "plugins don't share secrets");
        secrets.delete("p", "token").unwrap();
        assert_eq!(secrets.get("p", "token").unwrap(), None);
        secrets.set("p", "a", "1").unwrap();
        secrets.forget("p");
        assert_eq!(secrets.get("p", "a").unwrap(), None);
    }

    /// Writes to the real keychain under a throwaway service; run by hand.
    #[test]
    #[ignore = "uses the system keychain"]
    fn keychain_secrets_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::open(dir.path(), true);
        secrets.set("p", "probe", "x").unwrap();
        assert_eq!(secrets.protection(), Some(Protection::Keychain));
        secrets.set("p", "token", "s3cret").unwrap();
        assert_eq!(secrets.get("p", "token").unwrap().as_deref(), Some("s3cret"));
        secrets.forget("p");
        assert_eq!(secrets.get("p", "token").unwrap(), None);
    }
}
