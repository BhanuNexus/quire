// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Turning a folder into a locked folder and back. Both are resumable: an
//! interruption at any step leaves every file readable in one form or the
//! other, and running the same conversion again finishes it.

use super::format::{FolderId, FolderKey, Kdf, KeyFile, Kind};
use super::{KEY_FILE, LockedFolder, UNLOCKING_FILE, is_encrypted_name, is_key_file, plain_name, stored};
use crate::locked::damaged;
use crate::{NotePath, Result, Vault, VaultError};

/// What a conversion did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvertReport {
    /// Files encrypted (locking) or decrypted (unlocking).
    pub files: usize,
    /// History snapshots encrypted or decrypted.
    pub history: usize,
    /// The recovery key, when this run created the folder's key (shown
    /// once; never stored by Quire).
    pub recovery_key: Option<String>,
}

/// Plain files left in a locked folder: an interrupted conversion, or a
/// file another program (or a device that doesn't know about locking)
/// put there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainLeft {
    pub folder: String,
    pub files: Vec<NotePath>,
}

/// Why `folder` can't become a locked folder, in words; `None` when it can.
pub fn refuse_to_lock(vault: &Vault, folder: &str) -> Option<String> {
    let folder = folder.trim().trim_matches('/');
    if folder.is_empty() {
        return Some("The whole vault can't be locked; choose a folder.".into());
    }
    if folder.split('/').any(|s| s.is_empty() || s == "." || s == ".." || s.starts_with('.')) || folder.contains('\\') {
        return Some(format!("{folder}/ is a hidden or app folder."));
    }
    let tpl = vault.config.templates_folder.trim_matches('/');
    if folder.eq_ignore_ascii_case(tpl) || folder.to_lowercase().starts_with(&format!("{}/", tpl.to_lowercase())) {
        return Some("The templates folder can't be locked (templates are read to make new notes).".into());
    }
    let all = vault.storage().list().unwrap_or_default();
    let inside = LockedFolder { path: folder.to_string(), unlocking: false };
    for f in super::folders_in(&all) {
        if f.path == folder {
            return None; // already locked: running again finishes a conversion
        }
        if f.contains(folder) {
            return Some(format!("{folder}/ is inside the locked folder {}/.", f.path));
        }
        if inside.contains(&f.path) {
            return Some(format!("{folder}/ contains the locked folder {}/.", f.path));
        }
    }
    if all.iter().any(|p| inside.contains(p.as_str()) && p.file_name().starts_with("quire-lock") && !is_key_file(p.as_str())) {
        return Some(format!("{folder}/ holds a copy of a locked folder's key file; resolve it first."));
    }
    None
}

/// The files directly under `folder` (any depth), as stored.
fn stored_files(vault: &Vault, folder: &str) -> Result<Vec<NotePath>> {
    let f = LockedFolder { path: folder.to_string(), unlocking: false };
    Ok(vault.storage().list()?.into_iter().filter(|p| f.contains(p.as_str())).collect())
}

/// Lock `folder`, or finish locking it.
///
/// * First run: a new key, wrapped by `passphrase` into `quire-lock.json`;
///   the report carries the recovery key.
/// * Again (resuming, or plain files that arrived later): `passphrase`
///   unlocks the existing key, unless the folder is unlocked already.
///
/// Every plain file is encrypted to `name.qenc`, read back and decrypted
/// with a key unwrapped again from the key file as stored, and compared;
/// only when every file passed are the plain ones removed. Then the
/// history of the folder's notes is encrypted. The folder is left
/// unlocked in this vault's key ring.
pub fn lock_folder(vault: &Vault, folder: &str, passphrase: Option<&str>, kdf: Kdf, now_ms: i64) -> Result<ConvertReport> {
    let folder = folder.trim().trim_matches('/').to_string();
    if let Some(why) = refuse_to_lock(vault, &folder) {
        return Err(VaultError::InvalidName(why));
    }
    let key_path = NotePath::join(&folder, KEY_FILE);
    if vault.storage().exists(&NotePath::join(&folder, UNLOCKING_FILE)) {
        return Err(VaultError::InvalidName(format!("{folder}/ is being unlocked for good; finish that first.")));
    }
    let mut report = ConvertReport::default();
    if !vault.storage().exists(&key_path) {
        let pass = passphrase.ok_or_else(|| VaultError::Locked(folder.clone()))?;
        let id = FolderId::generate().map_err(|e| damaged(&key_path, &e))?;
        let key = FolderKey::generate().map_err(|e| damaged(&key_path, &e))?;
        let kf = KeyFile::new(id, &key, pass, kdf, now_ms).map_err(|e| damaged(&key_path, &e))?;
        // From here on the folder is locked: agents can't see it and nothing
        // plain can be written into it.
        vault.storage().write_atomic(&key_path, &kf.to_json())?;
        report.recovery_key = Some(key.recovery_key());
        vault.keys().insert(&folder, id, key);
    }
    // The key as the key file on disk gives it (checks what was just written).
    let (_, kf) = vault.key_file(&folder)?;
    let id = kf.id().map_err(|e| damaged(&key_path, &e))?;
    let key = match (vault.keys().get(&folder), passphrase) {
        (Some((_, k)), _) => {
            kf.check_key(&k).map_err(|e| damaged(&key_path, &e))?;
            k
        }
        (None, Some(p)) => vault.unlock(&folder, p)?,
        (None, None) => return Err(VaultError::Locked(folder)),
    };
    let check = if report.recovery_key.is_some() && passphrase.is_some() {
        // Unwrap again from the stored file, so a key file that doesn't open is caught before any plain file goes.
        kf.unwrap_key(passphrase.unwrap_or_default()).map_err(|e| damaged(&key_path, &e))?
    } else {
        key.clone()
    };

    // Phase 1: an encrypted, verified copy of every plain file.
    let plain: Vec<NotePath> =
        stored_files(vault, &folder)?.into_iter().filter(|p| !is_encrypted_name(p.as_str()) && !is_key_file(p.as_str())).collect();
    let mut verified = Vec::with_capacity(plain.len());
    for p in &plain {
        let bytes = vault.storage().read_bytes(p)?;
        let enc = stored(p);
        let same = vault.storage().read_bytes(&enc).ok().and_then(|b| check.open(Kind::File, id, &b).ok()).is_some_and(|b| b == bytes);
        if !same {
            let sealed = key.seal(Kind::File, id, &bytes).map_err(|e| damaged(&enc, &e))?;
            vault.storage().write_bytes_atomic(&enc, &sealed)?;
            let back = vault.storage().read_bytes(&enc)?;
            if check.open(Kind::File, id, &back).map_err(|e| damaged(&enc, &e))? != bytes {
                return Err(VaultError::Damaged { path: enc.to_string(), reason: "the encrypted copy didn't read back the same".into() });
            }
        }
        verified.push((p.clone(), bytes));
        report.files += 1;
    }
    // Phase 2: remove the plain files, each only if it is still what was encrypted.
    for (p, bytes) in &verified {
        let still = vault.storage().read_bytes(p).ok();
        if still.as_deref() == Some(bytes.as_slice()) {
            vault.storage().remove(p)?;
        } else if let Some(now) = still {
            // Changed by another program meanwhile: encrypt the newer text, then remove it.
            vault.write_file(p, &now)?;
        }
    }
    // Phase 3: the history of the folder's notes.
    for note in vault.locked_notes(&folder) {
        report.history += vault.rehome_history(&note, &note)?;
    }
    Ok(report)
}

/// Make `folder` an ordinary folder again, or finish doing so. Needs the
/// folder unlocked (or `passphrase`).
///
/// The key file is renamed to `quire-lock.unlocking.json` first (the folder
/// takes plain files from then on); each `.qenc` file is decrypted to its
/// plain name and read back; then the encrypted files and the key file go
/// to `.trash/` (they hold only ciphertext), and history is decrypted. A
/// plain file that is already there wins: it's what the last run wrote, or
/// a newer edit.
pub fn unlock_folder_permanently(vault: &Vault, folder: &str, passphrase: Option<&str>) -> Result<ConvertReport> {
    let folder = folder.trim().trim_matches('/').to_string();
    let (f, kf) = vault.key_file(&folder)?;
    let id = kf.id().map_err(|e| damaged(&f.key_file(), &e))?;
    let key = match (vault.keys().get(&folder), passphrase) {
        (Some((_, k)), _) => k,
        (None, Some(p)) => vault.unlock(&folder, p)?,
        (None, None) => return Err(VaultError::Locked(folder)),
    };
    kf.check_key(&key).map_err(|e| damaged(&f.key_file(), &e))?;
    let unlocking = NotePath::join(&folder, UNLOCKING_FILE);
    if !f.unlocking {
        vault.storage().rename(&NotePath::join(&folder, KEY_FILE), &unlocking)?;
    }
    let mut report = ConvertReport::default();
    // History first, while the files still say which notes the folder has.
    // (The folder is "unlocking": snapshots are written plain.)
    let notes = vault.locked_notes(&folder);
    for note in &notes {
        report.history += vault.rehome_history(note, note)?;
    }
    let encrypted: Vec<NotePath> = stored_files(vault, &folder)?.into_iter().filter(|p| is_encrypted_name(p.as_str())).collect();
    for enc in &encrypted {
        let Some(plain) = plain_name(enc) else { continue };
        if vault.storage().exists(&plain) {
            continue;
        }
        let bytes = key.open(Kind::File, id, &vault.storage().read_bytes(enc)?).map_err(|e| damaged(enc, &e))?;
        vault.storage().write_bytes_atomic(&plain, &bytes)?;
        if vault.storage().read_bytes(&plain)? != bytes {
            return Err(VaultError::Damaged { path: plain.to_string(), reason: "the plain copy didn't read back the same".into() });
        }
        report.files += 1;
    }
    for enc in &encrypted {
        if plain_name(enc).is_some_and(|p| vault.storage().exists(&p)) {
            vault.storage().trash(enc)?;
        }
    }
    vault.storage().trash(&unlocking)?;
    vault.keys().remove(&folder);
    Ok(report)
}

/// Plain files in locked folders (not counting folders being unlocked for
/// good): what an interrupted lock or another program left.
pub fn plain_left(vault: &Vault) -> Vec<PlainLeft> {
    let all = vault.storage().list().unwrap_or_default();
    super::folders_in(&all)
        .into_iter()
        .filter(|f| !f.unlocking)
        .filter_map(|f| {
            let files: Vec<NotePath> = all
                .iter()
                .filter(|p| f.contains(p.as_str()) && !is_encrypted_name(p.as_str()) && !is_key_file(p.as_str()))
                .cloned()
                .collect();
            (!files.is_empty()).then_some(PlainLeft { folder: f.path, files })
        })
        .collect()
}
