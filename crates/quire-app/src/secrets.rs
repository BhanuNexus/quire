// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! API keys: the OS keychain when available, otherwise a private file.
//! Keys never go in `settings.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub trait SecretStore {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<(), String>;
    /// Human description, shown in settings ("macOS Keychain", "private file").
    fn kind(&self) -> String;
}

#[cfg(feature = "keychain")]
struct Keychain;

#[cfg(feature = "keychain")]
impl SecretStore for Keychain {
    fn get(&self, key: &str) -> Option<String> {
        keyring::Entry::new("Quire", key).ok()?.get_password().ok()
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new("Quire", key).and_then(|e| e.set_password(value)).map_err(|e| e.to_string())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        keyring::Entry::new("Quire", key).and_then(|e| e.delete_credential()).map_err(|e| e.to_string())
    }
    fn kind(&self) -> String {
        "system keychain".into()
    }
}

/// Fallback: a TOML map in the config dir, readable only by the user.
pub struct FileSecrets {
    path: PathBuf,
}

impl FileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(&self.path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    fn store(&self, m: &BTreeMap<String, String>) -> Result<(), String> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let s = toml::to_string(m).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, s).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, key: &str) -> Option<String> {
        self.load().remove(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let mut m = self.load();
        m.insert(key.to_string(), value.to_string());
        self.store(&m)
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        let mut m = self.load();
        m.remove(key);
        self.store(&m)
    }
    fn kind(&self) -> String {
        format!("private file ({})", self.path.display())
    }
}

/// The best store on this machine.
pub fn store() -> Box<dyn SecretStore> {
    if let Some(p) = std::env::var_os("QUIRE_SECRETS_FILE") {
        return Box::new(FileSecrets::new(p));
    }
    #[cfg(feature = "keychain")]
    if keychain_usable() {
        return Box::new(Keychain);
    }
    let dir = crate::settings::config_dir().unwrap_or_else(std::env::temp_dir);
    Box::new(FileSecrets::new(dir.join("secrets.toml")))
}

/// Whether the OS keychain works here, decided once per process.
#[cfg(feature = "keychain")]
fn keychain_usable() -> bool {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USABLE.get_or_init(|| {
        if keyring::Entry::store_status().is_err() {
            return false;
        }
        // Some Linux sessions advertise a Secret Service that then fails, so
        // try a write there. macOS and Windows keychains are always present,
        // and a write can block on a locked keychain, so they skip the probe.
        if !cfg!(target_os = "linux") {
            return true;
        }
        keyring::Entry::new("Quire", "__probe__")
            .and_then(|e| {
                e.set_password("ok")?;
                let _ = e.delete_credential();
                Ok(())
            })
            .is_ok()
    })
}

fn env_name(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        _ => None,
    }
}

/// Key for a provider: stored secret first, then the conventional env var.
pub fn api_key(store: &dyn SecretStore, provider: &str) -> Option<String> {
    store
        .get(&format!("{provider}-api-key"))
        .filter(|k| !k.is_empty())
        .or_else(|| env_name(provider).and_then(|n| std::env::var(n).ok()).filter(|k| !k.is_empty()))
}

pub fn set_api_key(store: &dyn SecretStore, provider: &str, key: &str) -> Result<(), String> {
    let name = format!("{provider}-api-key");
    if key.is_empty() { store.delete(&name) } else { store.set(&name, key) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trips_privately() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileSecrets::new(dir.path().join("s.toml"));
        set_api_key(&s, "openai", "sk-1").unwrap();
        assert_eq!(api_key(&s, "openai").as_deref(), Some("sk-1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.path().join("s.toml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        set_api_key(&s, "openai", "").unwrap();
        assert!(s.get("openai-api-key").is_none());
    }
}
