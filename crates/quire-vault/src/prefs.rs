// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Vault preferences: `.quire/shared/vault.toml`.
//!
//! This file travels with the vault (folder sync, git) and people may edit it
//! by hand, so it is rewritten with `toml_edit`: comments, key order and
//! sections Quire doesn't know survive every save. Each feature owns one
//! section (`[periodic]`, `[git]`, `[lint]`…) and reads and writes it through
//! [`VaultPrefs::section`] and [`VaultPrefs::set_section`] with its own
//! serde type, so features never share a struct. See docs/formats/vault-prefs.md.

use serde::Serialize;
use serde::de::DeserializeOwned;
use toml_edit::{DocumentMut, Item, Table};

use crate::{NotePath, Vault, VaultError};

/// Where the preferences live, relative to the vault root.
pub const VAULT_PREFS_PATH: &str = ".quire/shared/vault.toml";
/// The format this version writes and fully understands.
pub const VAULT_PREFS_FORMAT: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum PrefsError {
    /// The file says `format` is newer than this Quire knows; it is read but never rewritten.
    #[error("vault preferences were made by a newer Quire (format {0}); not changing them")]
    NewerFormat(i64),
    /// The file couldn't be parsed; it is left as it is.
    #[error("vault preferences can't be read ({0}); not changing them")]
    Unreadable(String),
    #[error("can't store this setting: {0}")]
    Encode(String),
    #[error(transparent)]
    Vault(#[from] VaultError),
}

/// The parsed preferences file, with its formatting.
#[derive(Debug, Clone)]
pub struct VaultPrefs {
    doc: DocumentMut,
    /// Why the file on disk couldn't be parsed (then this holds defaults).
    unreadable: Option<String>,
}

impl Default for VaultPrefs {
    fn default() -> Self {
        let mut doc = DocumentMut::new();
        doc.insert("format", toml_edit::value(VAULT_PREFS_FORMAT));
        Self { doc, unreadable: None }
    }
}

impl VaultPrefs {
    /// Parse preferences text.
    pub fn parse(text: &str) -> Result<Self, PrefsError> {
        let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| PrefsError::Unreadable(e.to_string()))?;
        Ok(Self { doc, unreadable: None })
    }

    /// The vault's preferences; defaults when the file is missing. An
    /// unreadable file gives defaults too, and [`VaultPrefs::save`] then
    /// refuses to overwrite it.
    pub fn load(vault: &Vault) -> Self {
        match vault.storage().read(&NotePath::new(VAULT_PREFS_PATH)) {
            Ok(text) => Self::parse(&text).unwrap_or_else(|e| Self { unreadable: Some(e.to_string()), ..Self::default() }),
            Err(_) => Self::default(),
        }
    }

    /// `format` as written in the file (1 when absent).
    pub fn format(&self) -> i64 {
        self.doc.get("format").and_then(Item::as_integer).unwrap_or(1)
    }

    /// The file was written by a newer Quire; show "made by a newer Quire" and don't save.
    pub fn is_newer(&self) -> bool {
        self.format() > VAULT_PREFS_FORMAT
    }

    /// A section as the feature's own type; its `Default` when the section is
    /// missing or doesn't match the type (a hand-edit typo never stops the app).
    pub fn section<T: DeserializeOwned + Default>(&self, name: &str) -> T {
        self.try_section(name).ok().flatten().unwrap_or_default()
    }

    /// A section, or why it can't be read; `Ok(None)` when absent.
    pub fn try_section<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, String> {
        if self.doc.get(name).is_none() {
            return Ok(None);
        }
        let all: toml::Table = toml::from_str(&self.doc.to_string()).map_err(|e| e.to_string())?;
        all.get(name).cloned().map(toml::Value::try_into::<T>).transpose().map_err(|e| e.to_string())
    }

    /// Replace a section with `value`, keeping comments and key order for
    /// keys that stay. Keys the type doesn't serialize are removed, so types
    /// keep unknown keys with `#[serde(flatten)] other: toml::Table`.
    pub fn set_section<T: Serialize>(&mut self, name: &str, value: &T) -> Result<(), PrefsError> {
        if self.is_newer() {
            return Err(PrefsError::NewerFormat(self.format()));
        }
        let mut wrapper = toml::Table::new();
        wrapper.insert(name.to_string(), toml::Value::try_from(value).map_err(|e| PrefsError::Encode(e.to_string()))?);
        let text = toml::to_string(&wrapper).map_err(|e| PrefsError::Encode(e.to_string()))?;
        let mut fresh: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| PrefsError::Encode(e.to_string()))?;
        let Some(new_item) = fresh.remove(name) else { return Ok(()) };
        match (self.doc.get_mut(name).and_then(Item::as_table_mut), new_item) {
            (Some(old), Item::Table(new)) => merge_table(old, new),
            (_, new) => {
                self.doc.insert(name, new);
            }
        }
        Ok(())
    }

    /// The file's text as it would be saved.
    pub fn to_text(&self) -> String {
        self.doc.to_string()
    }

    /// Write the file atomically. Refuses (and changes nothing) when the
    /// file on disk is newer or couldn't be read.
    pub fn save(&self, vault: &Vault) -> Result<(), PrefsError> {
        if let Some(why) = &self.unreadable {
            return Err(PrefsError::Unreadable(why.clone()));
        }
        if self.is_newer() {
            return Err(PrefsError::NewerFormat(self.format()));
        }
        vault.storage().write_atomic(&NotePath::new(VAULT_PREFS_PATH), &self.doc.to_string())?;
        Ok(())
    }
}

/// Put `new`'s keys into `old` in place: existing keys keep their position
/// and comments, new keys are appended, and keys `new` lacks are removed.
fn merge_table(old: &mut Table, new: Table) {
    let gone: Vec<String> = old.iter().map(|(k, _)| k.to_string()).filter(|k| !new.contains_key(k)).collect();
    for k in gone {
        old.remove(&k);
    }
    for (k, item) in new {
        match (old.get_mut(&k), item) {
            (Some(Item::Table(o)), Item::Table(n)) => merge_table(o, n),
            (Some(Item::Value(o)), Item::Value(mut n)) => {
                // Keep a trailing comment (`x = 1 # why`) on the value.
                *n.decor_mut() = o.decor().clone();
                *o = n;
            }
            (_, item) => {
                old.insert(&k, item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Periodic {
        folder: String,
        format: String,
        #[serde(flatten)]
        other: toml::Table,
    }

    const HAND_WRITTEN: &str = "# Shared settings for this vault
format = 1

[periodic]
# where daily notes go
folder = \"Daily\"   # keep it short
format = \"%Y-%m-%d\"
weekday_start = \"monday\"

[future_feature]
level = 3
";

    #[test]
    fn edits_keep_comments_order_and_unknown_sections() {
        let mut p = VaultPrefs::parse(HAND_WRITTEN).unwrap();
        let mut per: Periodic = p.section("periodic");
        assert_eq!(per.folder, "Daily");
        assert_eq!(per.other.get("weekday_start").and_then(toml::Value::as_str), Some("monday"));
        per.folder = "Journal".into();
        p.set_section("periodic", &per).unwrap();
        let out = p.to_text();
        assert!(out.contains("# where daily notes go\nfolder = \"Journal\"   # keep it short\n"), "{out}");
        assert!(out.contains("weekday_start = \"monday\""), "{out}");
        assert!(out.contains("[future_feature]\nlevel = 3"), "{out}");
        assert!(out.starts_with("# Shared settings for this vault\nformat = 1\n"), "{out}");
        // A new section is appended.
        let mut tags = BTreeMap::new();
        tags.insert("colors".to_string(), BTreeMap::from([("project".to_string(), "#4C8BF5".to_string())]));
        p.set_section("tags", &tags).unwrap();
        let back: BTreeMap<String, BTreeMap<String, String>> = p.section("tags");
        assert_eq!(back, tags);
    }

    #[test]
    fn missing_bad_and_newer_files() {
        let v = Vault::in_memory(&[]);
        let p = VaultPrefs::load(&v);
        assert_eq!(p.format(), 1);
        assert_eq!(p.section::<Periodic>("periodic"), Periodic::default());
        p.save(&v).unwrap();
        assert!(v.storage().read(&NotePath::new(VAULT_PREFS_PATH)).unwrap().contains("format = 1"));

        // A typo in one section gives that section's defaults.
        let p = VaultPrefs::parse("[periodic]\nfolder = 3\n").unwrap();
        assert_eq!(p.section::<Periodic>("periodic"), Periodic::default());
        assert!(p.try_section::<Periodic>("periodic").is_err());

        // Unparsable: defaults, and the file is never overwritten.
        v.storage().write_atomic(&NotePath::new(VAULT_PREFS_PATH), "[periodic\n").unwrap();
        let p = VaultPrefs::load(&v);
        assert!(matches!(p.save(&v), Err(PrefsError::Unreadable(_))));
        assert_eq!(v.storage().read(&NotePath::new(VAULT_PREFS_PATH)).unwrap(), "[periodic\n");

        // Newer: readable, never rewritten.
        let mut p = VaultPrefs::parse("format = 2\n[periodic]\nfolder = \"D\"\n").unwrap();
        assert!(p.is_newer());
        assert_eq!(p.section::<Periodic>("periodic").folder, "D");
        assert!(matches!(p.set_section("periodic", &Periodic::default()), Err(PrefsError::NewerFormat(2))));
        assert!(matches!(p.save(&v), Err(PrefsError::NewerFormat(2))));
    }
}
