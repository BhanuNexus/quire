// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The locked folders this device knows of, remembered apart from their key
//! files (docs/formats/locked-folders.md#remembered-locked-folders).
//!
//! Whether a folder is locked can't hang on its key file alone: anyone who
//! can write the folder (a cloud provider, a sync peer, a script) could
//! delete `quire-lock.json`, or rename it to `quire-lock.unlocking.json`,
//! and the next save would then write the note in plain text. So once a
//! device has seen a folder locked, it remembers that, by folder id, in two
//! places:
//!
//! * `.quire/shared/locked-folders.json`, which travels with the vault, so
//!   another device learns of a locked folder even before its key file;
//! * a file of this device's own (`<config>/vaults/<key>/locked-folders.json`
//!   for the app and the CLI), which nobody who only reaches the vault can
//!   change.
//!
//! A remembered folder whose key file is gone stays locked: plain writes
//! there are refused (by the vault, and by the storage itself), and the
//! person is told to restore the key file or unlock with the recovery key.
//! A key file renamed to the `unlocking` name counts as unlocking only while
//! this process runs that conversion.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use super::{LockedFolder, folders_in, is_encrypted_name};
use crate::{NotePath, Storage};

/// The vault's record of its locked folders (travels with the vault).
pub const SHARED_RECORD: &str = ".quire/shared/locked-folders.json";
/// The name of this device's record, in the folder the app or CLI keeps
/// for the vault.
pub const LOCAL_RECORD: &str = "locked-folders.json";

const FORMAT: u32 = 1;

/// One remembered locked folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownFolder {
    /// Vault-relative, no trailing slash.
    pub folder: String,
    /// The folder id (hex), as in its key file and every file's header.
    pub id: String,
    /// The key file's check value (hex): tells whether a recovery key is
    /// this folder's even when the key file is lost.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub check: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    format: u32,
    folders: Vec<KnownFolder>,
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

impl Record {
    fn parse(text: &str) -> Vec<KnownFolder> {
        serde_json::from_str::<Self>(text)
            .ok()
            .filter(|r| r.format <= FORMAT)
            .map(|r| r.folders.into_iter().filter(|f| valid_folder(&f.folder)).collect())
            .unwrap_or_default()
    }

    fn text(folders: &[KnownFolder]) -> String {
        let mut s = serde_json::to_string_pretty(&Self { format: FORMAT, folders: folders.to_vec(), other: serde_json::Map::new() })
            .unwrap_or_else(|_| unreachable!("plain data serializes"));
        s.push('\n');
        s
    }
}

/// A folder path a record may name: relative, no hidden or empty segments.
fn valid_folder(f: &str) -> bool {
    !f.is_empty() && !f.contains('\\') && f.split('/').all(|s| !s.is_empty() && !s.starts_with('.'))
}

#[derive(Default)]
struct State {
    folders: Vec<KnownFolder>,
    /// Folders this process is turning back into plain ones right now.
    converting: Vec<String>,
    /// This device's record, once [`crate::Vault::remember_locked_folders`] named it.
    local: Option<PathBuf>,
}

/// The locked folders this process knows of. Shared by every clone of a
/// [`crate::Vault`], [`crate::Vault::without_keys`] included.
#[derive(Clone, Default)]
pub struct Known(Arc<RwLock<State>>);

impl std::fmt::Debug for Known {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.folders().iter().map(|k| &k.folder)).finish()
    }
}

impl Known {
    pub fn folders(&self) -> Vec<KnownFolder> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).folders.clone()
    }

    /// The remembered folder `path` is in.
    pub fn folder_of(&self, path: &str) -> Option<String> {
        let st = self.0.read().unwrap_or_else(PoisonError::into_inner);
        st.folders.iter().map(|k| LockedFolder { path: k.folder.clone(), unlocking: false }).find(|f| f.contains(path)).map(|f| f.path)
    }

    pub fn contains(&self, folder: &str) -> bool {
        self.0.read().unwrap_or_else(PoisonError::into_inner).folders.iter().any(|k| k.folder == folder)
    }

    pub fn get(&self, folder: &str) -> Option<KnownFolder> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).folders.iter().find(|k| k.folder == folder).cloned()
    }

    /// Whether this process is turning `folder` back into a plain one.
    pub fn is_converting(&self, folder: &str) -> bool {
        self.0.read().unwrap_or_else(PoisonError::into_inner).converting.iter().any(|c| c == folder)
    }

    /// What the storage must keep refusing plain writes into: every
    /// remembered folder but those being unlocked for good here.
    fn hint(&self) -> Vec<String> {
        let st = self.0.read().unwrap_or_else(PoisonError::into_inner);
        st.folders.iter().filter(|k| !st.converting.contains(&k.folder)).map(|k| k.folder.clone()).collect()
    }

    fn local(&self) -> Option<PathBuf> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).local.clone()
    }
}

/// While alive, `folder` is being unlocked for good by this process: its
/// `unlocking` key file means what it says, and plain writes are let
/// through.
pub(crate) struct Converting<'a> {
    known: &'a Known,
    storage: &'a dyn Storage,
    folder: String,
}

impl Drop for Converting<'_> {
    fn drop(&mut self) {
        self.known.0.write().unwrap_or_else(PoisonError::into_inner).converting.retain(|c| *c != self.folder);
        self.storage.set_locked_folders(&self.known.hint());
    }
}

impl crate::Vault {
    /// The locked folders this process knows of.
    pub fn known_locked(&self) -> &Known {
        &self.known
    }

    /// Learn this vault's locked folders: from their key files, the vault's
    /// record ([`SHARED_RECORD`]) and this device's (`local`, a file this
    /// device keeps outside the vault, such as
    /// `<config>/vaults/<key>/locked-folders.json`). Both records are
    /// brought up to date. Call it when the vault is opened.
    ///
    /// A folder is forgotten only when nothing of it is left: no key file
    /// (either name), no longer in the vault's record (the device that
    /// unlocked it for good removed it there), and no encrypted file in it.
    ///
    /// Returns the remembered folders whose key file is missing (or renamed
    /// to the `unlocking` name, which this process didn't do).
    pub fn remember_locked_folders(&self, local: Option<&Path>) -> Vec<String> {
        let shared = self.storage.read(&NotePath::new(SHARED_RECORD)).map(|t| Record::parse(&t)).unwrap_or_default();
        let mine = local.and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Record::parse(&t)).unwrap_or_default();
        let listing = self.storage.list().unwrap_or_default();
        let on_disk = folders_in(&listing);
        let mut all: Vec<KnownFolder> = Vec::new();
        // Key files on disk (not ones being unlocked: only a record can say those were locked).
        for f in on_disk.iter().filter(|f| !f.unlocking) {
            if let Ok((_, kf)) = self.key_file(&f.path) {
                all.push(KnownFolder { folder: f.path.clone(), id: kf.folder_id.clone(), check: kf.check.clone() });
            }
        }
        let held = |folder: &str| on_disk.iter().any(|f| f.path == folder);
        for k in mine.iter().chain(&shared).chain(&self.known.folders()) {
            if all.iter().any(|a| a.folder == k.folder) {
                continue;
            }
            let encrypted = listing
                .iter()
                .any(|p| is_encrypted_name(p.as_str()) && LockedFolder { path: k.folder.clone(), unlocking: false }.contains(p.as_str()));
            let in_shared = shared.iter().any(|s| s.folder == k.folder);
            if held(&k.folder) || in_shared || encrypted {
                all.push(k.clone());
            }
        }
        // Locked folders don't nest: an outer one wins.
        let outer = all.clone();
        all.retain(|k| !outer.iter().any(|o| LockedFolder { path: o.folder.clone(), unlocking: false }.contains(&k.folder)));
        all.sort_by(|a, b| a.folder.cmp(&b.folder));
        {
            let mut st = self.known.0.write().unwrap_or_else(PoisonError::into_inner);
            st.folders.clone_from(&all);
            if local.is_some() {
                st.local = local.map(Path::to_path_buf);
            }
        }
        self.save_known(&shared, &mine);
        all.iter()
            .filter(|k| !on_disk.iter().any(|f| f.path == k.folder && (!f.unlocking || self.known.is_converting(&f.path))))
            .map(|k| k.folder.clone())
            .collect()
    }

    /// Remember `folder` as locked (its key file just written or arrived).
    pub fn note_locked_folder(&self, folder: &str) -> crate::Result<()> {
        let (f, kf) = self.key_file(folder)?;
        if f.unlocking {
            return Ok(());
        }
        let k = KnownFolder { folder: f.path, id: kf.folder_id, check: kf.check };
        let before = self.known.folders();
        {
            let mut st = self.known.0.write().unwrap_or_else(PoisonError::into_inner);
            st.folders.retain(|o| o.folder != k.folder);
            st.folders.push(k);
            st.folders.sort_by(|a, b| a.folder.cmp(&b.folder));
        }
        self.save_known_over(&before);
        Ok(())
    }

    /// Forget that `folder` was locked: it was unlocked for good.
    pub(crate) fn forget_locked_folder(&self, folder: &str) {
        let before = self.known.folders();
        self.known.0.write().unwrap_or_else(PoisonError::into_inner).folders.retain(|k| k.folder != folder);
        self.save_known_over(&before);
    }

    /// Mark `folder` as being unlocked for good by this process.
    pub(crate) fn converting(&self, folder: &str) -> Converting<'_> {
        self.known.0.write().unwrap_or_else(PoisonError::into_inner).converting.push(folder.to_string());
        self.storage.set_locked_folders(&self.known.hint());
        Converting { known: &self.known, storage: self.storage.as_ref(), folder: folder.to_string() }
    }

    fn save_known_over(&self, before: &[KnownFolder]) {
        let shared = self.storage.read(&NotePath::new(SHARED_RECORD)).map(|t| Record::parse(&t)).unwrap_or_default();
        // Other devices' folders in the vault's record that this one doesn't know yet are kept.
        let now = self.known.folders();
        let mut merged: Vec<KnownFolder> = shared.iter().filter(|s| !before.iter().any(|b| b.folder == s.folder)).cloned().collect();
        merged.retain(|m| !now.iter().any(|n| n.folder == m.folder));
        merged.extend(now);
        merged.sort_by(|a, b| a.folder.cmp(&b.folder));
        let mine = self.known.local().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Record::parse(&t)).unwrap_or_default();
        self.write_known(&shared, &merged, &mine);
    }

    fn save_known(&self, shared: &[KnownFolder], mine: &[KnownFolder]) {
        let now = self.known.folders();
        self.write_known(shared, &now, mine);
    }

    fn write_known(&self, shared_before: &[KnownFolder], shared_now: &[KnownFolder], mine_before: &[KnownFolder]) {
        if shared_before != shared_now {
            let p = NotePath::new(SHARED_RECORD);
            let r = if shared_now.is_empty() { self.storage.remove(&p) } else { self.storage.write_atomic(&p, &Record::text(shared_now)) };
            // Best effort: this process remembers the folder either way, and
            // this device's own record is the one nobody else can change.
            let _ = r;
        }
        let now = self.known.folders();
        if let Some(local) = self.known.local()
            && mine_before != now.as_slice()
        {
            if let Some(dir) = local.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = local.with_extension("json.tmp");
            let _ = std::fs::write(&tmp, Record::text(&now)).and_then(|()| std::fs::rename(&tmp, &local));
        }
        self.storage.set_locked_folders(&self.known.hint());
    }
}
