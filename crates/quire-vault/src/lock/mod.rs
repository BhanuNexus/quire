// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Locked folders: folders whose notes and attachments are stored
//! encrypted (docs/design/0.11-secure.md, docs/formats/locked-folders.md).
//!
//! A folder is locked when it holds [`KEY_FILE`]. Its files are stored as
//! `name.qenc`; the vault reads and writes them by their plain names
//! (`Journal/Day.md`) through a [`KeyRing`] that holds the keys of unlocked
//! folders, in memory only.
//!
//! The rules that keep plain text off the disk:
//! * [`crate::Vault::notes`] never lists a note in a locked folder, so the
//!   index and everything built on it never sees one;
//! * [`crate::Storage`] adapters refuse to write a plain file into a locked
//!   folder ([`crate::VaultError::Locked`]);
//! * history snapshots of a locked note are encrypted too.

mod convert;
pub mod format;
mod known;
mod ring;

pub use convert::{ConvertReport, PlainLeft, begin_lock, keep_both, lock_folder, plain_left, refuse_to_lock, unlock_folder_permanently};
pub use format::{FolderId, FolderKey, Header, Kdf, KeyFile, Kind, LockError, MIN_PASSPHRASE};
pub use known::{Known, KnownFolder, LOCAL_RECORD, SHARED_RECORD};
pub use ring::KeyRing;
/// For callers that hold passphrases and recovery keys (`Zeroizing<String>`).
pub use zeroize;

use crate::{NotePath, Storage};

/// The key file at the top of a locked folder.
pub const KEY_FILE: &str = "quire-lock.json";
/// The key file while the folder is being unlocked for good: the folder is
/// plain for writes, and the next run finishes the conversion.
pub const UNLOCKING_FILE: &str = "quire-lock.unlocking.json";
/// The extension of an encrypted file (`Day.md.qenc`).
pub const EXT: &str = ".qenc";

/// Whether `path` is an encrypted file (`….qenc`).
pub fn is_encrypted_name(path: &str) -> bool {
    path.len() > EXT.len() && path.get(path.len() - EXT.len()..).is_some_and(|e| e.eq_ignore_ascii_case(EXT))
}

/// Whether `path` is a locked folder's key file (either name).
pub fn is_key_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == KEY_FILE || name == UNLOCKING_FILE
}

/// The stored name of a file in a locked folder: `Journal/Day.md` →
/// `Journal/Day.md.qenc`.
pub fn stored(path: &NotePath) -> NotePath {
    NotePath::new(format!("{}{EXT}", path.as_str()))
}

/// The plain name of a stored file: `Journal/Day.md.qenc` → `Journal/Day.md`.
pub fn plain_name(path: &NotePath) -> Option<NotePath> {
    is_encrypted_name(path.as_str()).then(|| NotePath::new(&path.as_str()[..path.as_str().len() - EXT.len()]))
}

/// A locked folder, as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedFolder {
    /// Vault-relative, no trailing slash (`Journal`, `Private/Health`).
    pub path: String,
    /// Being unlocked for good ([`UNLOCKING_FILE`]); finished by running
    /// the conversion again.
    pub unlocking: bool,
}

impl LockedFolder {
    pub fn key_file(&self) -> NotePath {
        NotePath::join(&self.path, if self.unlocking { UNLOCKING_FILE } else { KEY_FILE })
    }

    /// Whether `path` is inside this folder.
    pub fn contains(&self, path: &str) -> bool {
        path.strip_prefix(self.path.as_str()).is_some_and(|rest| rest.starts_with('/'))
    }
}

/// The locked folder `path` is in, found by looking for a key file in each
/// folder above it (outermost first). A file at the vault root is never in
/// one.
pub fn folder_of(storage: &(impl Storage + ?Sized), path: &str) -> Option<LockedFolder> {
    folder_of_with(path, |p| storage.exists(&NotePath::new(p)))
}

/// [`folder_of`] with an existence test of its own (a file system path,
/// a listing).
pub fn folder_of_with(path: &str, exists: impl Fn(&str) -> bool) -> Option<LockedFolder> {
    let folder = path.rsplit_once('/').map(|(f, _)| f)?;
    let mut end = 0;
    for seg in folder.split('/') {
        end += seg.len();
        let prefix = &folder[..end];
        end += 1;
        if seg.is_empty() || seg.starts_with('.') {
            return None;
        }
        if exists(&format!("{prefix}/{KEY_FILE}")) {
            return Some(LockedFolder { path: prefix.to_string(), unlocking: false });
        }
        if exists(&format!("{prefix}/{UNLOCKING_FILE}")) {
            return Some(LockedFolder { path: prefix.to_string(), unlocking: true });
        }
    }
    None
}

/// Every locked folder in a listing of the vault (outermost only).
pub fn folders_in(listing: &[NotePath]) -> Vec<LockedFolder> {
    let mut out: Vec<LockedFolder> = listing
        .iter()
        .filter_map(|p| {
            let (folder, name) = p.as_str().rsplit_once('/')?;
            match name {
                KEY_FILE => Some(LockedFolder { path: folder.to_string(), unlocking: false }),
                UNLOCKING_FILE => Some(LockedFolder { path: folder.to_string(), unlocking: true }),
                _ => None,
            }
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.unlocking.cmp(&b.unlocking)));
    out.dedup_by(|a, b| a.path == b.path);
    let all = out.clone();
    out.retain(|f| !all.iter().any(|o| o.contains(&f.path)));
    out
}

/// Whether a path inside `folders` (a [`folders_in`] result).
pub fn in_any(folders: &[LockedFolder], path: &str) -> bool {
    folders.iter().any(|f| f.contains(path))
}

/// Whether `path` is under one of the vault's own hidden folders at the
/// top (`.quire/`, `.trash/`, `.git/`…), which never belong to a locked
/// folder. A hidden folder *inside* a locked folder (`Journal/.drafts/`)
/// is part of it.
fn app_data(path: &str) -> bool {
    path.split('/').next().is_some_and(|s| s.starts_with('.'))
}

/// The locked folder a plain write to `path` would land in, if any.
///
/// `known` are the locked folders the vault remembers apart from their key
/// files ([`Known`]): a plain write there is refused even while the key
/// file is missing or renamed to the `unlocking` name. And a plain `X` is
/// refused wherever `X.qenc` exists, unless the folder is being unlocked
/// for good.
///
/// Encrypted files and the folder's own key file are fine, and so is the
/// vault's app data at the top (`.quire/`, `.trash/`).
pub fn refuses_plain(path: &str, exists: impl Fn(&str) -> bool, known: &[String]) -> Option<LockedFolder> {
    if is_encrypted_name(path) || app_data(path) {
        return None;
    }
    let remembered = known.iter().find(|k| LockedFolder { path: (*k).clone(), unlocking: false }.contains(path));
    let f = match (folder_of_with(path, &exists), remembered) {
        (Some(f), _) if !f.unlocking => f,
        (_, Some(k)) => LockedFolder { path: k.clone(), unlocking: false },
        (Some(_), None) => return None,
        (None, None) => {
            // No key file: the note's encrypted copy alone says this was locked.
            if !exists(&format!("{path}{EXT}")) {
                return None;
            }
            LockedFolder { path: path.rsplit_once('/').map_or_else(String::new, |(d, _)| d.to_string()), unlocking: false }
        }
    };
    if is_key_file(path) && path.rsplit_once('/').is_some_and(|(dir, _)| dir == f.path) {
        return None;
    }
    Some(f)
}

#[cfg(test)]
mod tests;
