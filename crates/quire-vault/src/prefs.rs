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
use toml_edit::{DocumentMut, Item};

use crate::{NotePath, Vault, VaultError};

/// Where the preferences live, relative to the vault root.
pub const VAULT_PREFS_PATH: &str = ".quire/shared/vault.toml";
/// What [`VaultPrefs::copy_from`] copies when no section is named.
///
/// Settings that make sense in any vault. Not `sort`, `order`, `icons` and `web`,
/// which are about one vault's own folders and notes (name them to copy them).
pub const COPY_SECTIONS: [&str; 8] = ["periodic", "tags", "git", "lint", "staleness", "citations", "links", "tasks"];

/// Keys that are never copied between vaults, by section.
///
/// They act on a remote or on this device (committing, pulling and pushing by
/// themselves), and copying them could start sending notes somewhere nobody
/// chose. The destination keeps its own values for them.
pub const NOT_COPIED: [(&str, &[&str]); 1] = [("git", &["auto_commit", "pull_on_start", "push_after_commit"])];

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
    /// The file used `\r\n` line ends throughout; they are written back.
    crlf: bool,
    /// Why the file on disk couldn't be parsed (then this holds defaults).
    unreadable: Option<String>,
}

impl Default for VaultPrefs {
    fn default() -> Self {
        let mut doc = DocumentMut::new();
        doc.insert("format", toml_edit::value(VAULT_PREFS_FORMAT));
        Self { doc, crlf: false, unreadable: None }
    }
}

impl VaultPrefs {
    /// Parse preferences text.
    pub fn parse(text: &str) -> Result<Self, PrefsError> {
        let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| PrefsError::Unreadable(e.to_string()))?;
        let crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
        Ok(Self { doc, crlf, unreadable: None })
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
        match (self.doc.get_mut(name).and_then(Item::as_table_like_mut), new_item) {
            (Some(old), Item::Table(new)) => merge_table(old, &new),
            (_, new) => {
                self.doc.insert(name, new);
            }
        }
        Ok(())
    }

    /// Take sections from another vault's preferences, replacing the same
    /// sections here (0.12). With no `names`, [`COPY_SECTIONS`]. Returns the
    /// sections copied (those `from` has); nothing is written until
    /// [`VaultPrefs::save`].
    pub fn copy_from(&mut self, from: &Self, names: &[&str]) -> Result<Vec<String>, PrefsError> {
        if let Some(why) = &from.unreadable {
            return Err(PrefsError::Unreadable(why.clone()));
        }
        if self.is_newer() {
            return Err(PrefsError::NewerFormat(self.format()));
        }
        let wanted: Vec<&str> = if names.is_empty() { COPY_SECTIONS.to_vec() } else { names.to_vec() };
        let mut copied = Vec::new();
        for name in wanted {
            if name == "format" {
                continue;
            }
            if let Some(item) = from.doc.get(name) {
                let item = self.without_not_copied(name, item);
                self.doc.insert(name, item);
                copied.push(name.to_string());
            }
        }
        Ok(copied)
    }

    /// `item` (section `name` of another vault) without the [`NOT_COPIED`]
    /// keys, which keep their value here (or are absent, as here).
    fn without_not_copied(&self, name: &str, item: &Item) -> Item {
        let Some((_, keys)) = NOT_COPIED.iter().find(|(n, _)| *n == name) else { return item.clone() };
        let Ok(mut table) = item.clone().into_table() else { return item.clone() };
        for key in *keys {
            table.remove(key);
            if let Some(mine) = self.doc.get(name).and_then(Item::as_table_like).and_then(|t| t.get(key)) {
                table.insert(key, mine.clone());
            }
        }
        Item::Table(table)
    }

    /// The file's text as it would be saved.
    pub fn to_text(&self) -> String {
        let text = self.doc.to_string();
        if self.crlf { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text }
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
        vault.storage().write_atomic(&NotePath::new(VAULT_PREFS_PATH), &self.to_text())?;
        Ok(())
    }
}

/// A value as plain data, for comparing what a key says apart from how it
/// is written (`['a' , "b"]` and `["a", "b"]` are the same value).
fn plain(v: &toml_edit::Value) -> Option<toml::Value> {
    let mut t: toml::Table = toml::from_str(&format!("v = {v}")).ok()?;
    t.remove("v")
}

/// Put `new`'s keys into `old` in place: existing keys keep their position
/// and comments, new keys are appended, and keys `new` lacks are removed.
///
/// A key whose value says the same thing is not touched at all, so its
/// quotes, spacing and comments (also inside arrays) stay as written. Inline
/// tables (`types = { a = 1 }`) are merged key by key like standard ones.
fn merge_table(old: &mut dyn toml_edit::TableLike, new: &dyn toml_edit::TableLike) {
    let gone: Vec<String> = old.iter().map(|(k, _)| k.to_string()).filter(|k| !new.contains_key(k)).collect();
    for k in gone {
        old.remove(&k);
    }
    for (k, item) in new.iter() {
        if let Some(o) = old.get_mut(k) {
            if let (Some(old_t), Some(new_t)) = (o.as_table_like_mut(), item.as_table_like()) {
                merge_table(old_t, new_t);
                continue;
            }
            if let (Item::Value(ov), Item::Value(nv)) = (&mut *o, item) {
                if plain(ov).is_some_and(|a| plain(nv).is_some_and(|b| a == b)) {
                    continue;
                }
                // Keep a trailing comment (`x = 1 # why`) on the value.
                let mut nv = nv.clone();
                *nv.decor_mut() = ov.decor().clone();
                *ov = nv;
                continue;
            }
            *o = item.clone();
        } else {
            old.insert(k, item.clone());
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

    #[test]
    fn git_settings_that_push_or_commit_by_themselves_are_not_copied() {
        let from = VaultPrefs::parse(
            "format = 1\n[git]\nauto_commit = true\npush_after_commit = true\npull_on_start = true\nmessage = \"M {{count}}\"\ncommit_after_idle_s = 30\n",
        )
        .unwrap();
        // A destination with no git section gets only the harmless keys.
        let mut to = VaultPrefs::parse("format = 1\n").unwrap();
        assert_eq!(to.copy_from(&from, &[]).unwrap(), ["git"]);
        let out = to.to_text();
        assert!(out.contains("message") && out.contains("commit_after_idle_s = 30"), "{out}");
        assert!(!out.contains("auto_commit") && !out.contains("push_after_commit") && !out.contains("pull_on_start"), "{out}");
        // Naming the section doesn't change that, and the destination keeps its own values.
        let mut to = VaultPrefs::parse("format = 1\n[git]\nauto_commit = false\npush_after_commit = true\nmessage = \"mine\"\n").unwrap();
        assert_eq!(to.copy_from(&from, &["git"]).unwrap(), ["git"]);
        let out = to.to_text();
        assert!(out.contains("auto_commit = false") && out.contains("push_after_commit = true"), "{out}");
        assert!(out.contains("M {{count}}") && !out.contains("pull_on_start"), "{out}");
    }

    #[test]
    fn sections_copy_from_another_vault() {
        let from = VaultPrefs::parse(
            "format = 1\n[periodic.daily]\nfolder = \"Journal\"\n[tags.colors]\nwork = \"#ff0000\"\n[order]\n\"P/\" = [\"a.md\"]\n",
        )
        .unwrap();
        let mut to = VaultPrefs::parse("# mine\nformat = 1\n[periodic.daily]\nfolder = \"Daily\"\n[lint]\nx = 1\n").unwrap();
        // Default: the portable sections only.
        assert_eq!(to.copy_from(&from, &[]).unwrap(), ["periodic", "tags"]);
        let out = to.to_text();
        assert!(
            out.starts_with("# mine\nformat = 1\n") && out.contains("folder = \"Journal\"") && out.contains("work = \"#ff0000\""),
            "{out}"
        );
        assert!(out.contains("[lint]") && !out.contains("[order]"), "{out}");
        // Named sections, even per-vault ones.
        assert_eq!(to.copy_from(&from, &["order", "missing", "format"]).unwrap(), ["order"]);
        assert!(to.to_text().contains("[order]"));
        // An unreadable source is never copied from; a newer destination is never changed.
        let bad = VaultPrefs { unreadable: Some("x".into()), ..VaultPrefs::default() };
        assert!(matches!(to.copy_from(&bad, &[]), Err(PrefsError::Unreadable(_))));
        let mut newer = VaultPrefs::parse("format = 9\n").unwrap();
        assert!(matches!(newer.copy_from(&from, &[]), Err(PrefsError::NewerFormat(9))));
    }

    /// Every section a settings page saves with `set_section` keeps the file's
    /// text: saving what was read changes nothing, a change touches its own
    /// value only, and `\r\n` files stay `\r\n`.
    #[test]
    fn saving_keeps_every_other_byte_for_every_settings_section() {
        const SECTIONS: [&str; 12] =
            ["periodic", "tags", "git", "lint", "staleness", "citations", "links", "tasks", "home", "sort", "order", "icons"];
        for eol in ["\n", "\r\n"] {
            for name in SECTIONS {
                let text = format!(
                    "# top{eol}format = 1{eol}{eol}[{name}]{eol}# why{eol}list = [{eol}  'a',   # first{eol}  \"b\",{eol}]{eol}odd   =   'x'  # keep{eol}\
                     inline = {{ k = 1, j = \"v\" }} # inl{eol}{eol}[{name}.sub]{eol}# blue{eol}color = '#112233'{eol}{eol}[[other]]{eol}n = 1{eol}[[other]]{eol}n = 2{eol}{eol}[tail]{eol}x = 1{eol}"
                );
                let mut p = VaultPrefs::parse(&text).unwrap();
                let t: toml::Table = p.section(name);
                assert!(!t.is_empty(), "{name} reads");
                p.set_section(name, &t).unwrap();
                assert_eq!(p.to_text(), text, "{name}, unchanged, eol {eol:?}");
                let mut changed = t.clone();
                changed.insert("added".into(), toml::Value::Integer(7));
                changed.insert("odd".into(), toml::Value::String("y".into()));
                p.set_section(name, &changed).unwrap();
                let out = p.to_text();
                assert!(
                    out.contains(&format!("odd   =   \"y\"  # keep{eol}")) && out.contains(&format!("added = 7{eol}")),
                    "{name}: {out:?}"
                );
                let rest = |s: &str| s.replace("odd   =   \"y\"", "odd   =   'x'").replace(&format!("added = 7{eol}"), "");
                assert_eq!(rest(&out), text, "{name}: only the two keys changed, eol {eol:?}");
                // Removing a key removes only its line.
                let mut fewer = t.clone();
                fewer.remove("odd");
                p.set_section(name, &fewer).unwrap();
                assert!(!p.to_text().contains("odd"), "{name}");
                assert!(p.to_text().contains(&format!("[tail]{eol}x = 1{eol}")), "{name}");
            }
        }
    }
}
