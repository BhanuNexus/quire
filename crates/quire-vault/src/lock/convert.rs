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
    /// Locking: plain files left as they were because an encrypted copy
    /// with other content is next to them (see [`keep_both`]). Unlocking:
    /// the copies made of encrypted files whose plain file differed.
    pub conflicts: Vec<NotePath>,
}

/// Plain files left in a locked folder: an interrupted conversion, or a
/// file another program (or a device that doesn't know about locking)
/// put there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainLeft {
    pub folder: String,
    /// Plain files with no encrypted copy: "Lock a folder" encrypts them.
    pub files: Vec<NotePath>,
    /// Plain files next to an encrypted copy (`X` and `X.qenc`). The
    /// encrypted one is the note; the plain one is shown to the person as
    /// a conflict, never read in its place or sealed over it.
    pub conflicts: Vec<NotePath>,
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

/// Where a lock conversion notes what it encrypted (device-local, under
/// `.quire/`): each plain file's keyed fingerprint ([`FolderKey::tag`]), so
/// a resumed run removes a plain file only if it is the very file this
/// device encrypted, never one that showed up meanwhile.
fn journal_path(id: FolderId) -> NotePath {
    NotePath::new(format!(".quire/lock/{}.json", id.to_hex()))
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Journal {
    format: u32,
    folder: String,
    /// Plain path → fingerprint of the bytes encrypted.
    files: std::collections::BTreeMap<String, String>,
}

impl Journal {
    fn load(vault: &Vault, id: FolderId, folder: &str) -> Self {
        vault
            .storage()
            .read(&journal_path(id))
            .ok()
            .and_then(|t| serde_json::from_str::<Self>(&t).ok())
            .filter(|j| j.folder == folder)
            .unwrap_or_else(|| Self { format: 1, folder: folder.to_string(), files: std::collections::BTreeMap::new() })
    }

    fn save(&self, vault: &Vault, id: FolderId) -> Result<()> {
        let text = serde_json::to_string(self).unwrap_or_else(|_| unreachable!("plain data serializes"));
        vault.storage().write_atomic(&journal_path(id), &text)
    }
}

/// Start locking `folder`: its key file first, and the recovery key back.
///
/// Writes the key file (if it has none yet), reads it back and unwraps it
/// again with `passphrase`, and keeps the key in the vault's key ring.
/// Returns the new folder's recovery key, before any file is converted, so
/// it can be shown even if the conversion then stops. `None` for a folder
/// that already has its key file.
pub fn begin_lock(vault: &Vault, folder: &str, passphrase: Option<&str>, kdf: Kdf, now_ms: i64) -> Result<Option<String>> {
    let folder = folder.trim().trim_matches('/').to_string();
    if let Some(why) = refuse_to_lock(vault, &folder) {
        return Err(VaultError::InvalidName(why));
    }
    if vault.storage().exists(&NotePath::join(&folder, UNLOCKING_FILE)) {
        return Err(VaultError::InvalidName(format!("{folder}/ is being unlocked for good; finish that first.")));
    }
    if vault.known_locked().contains(&folder) && vault.key_file(&folder).is_err() {
        return Err(VaultError::Locked(format!(
            "{folder} (its key file {KEY_FILE} is missing: restore it, or unlock with the recovery key to make a new one)"
        )));
    }
    let key_path = NotePath::join(&folder, KEY_FILE);
    if vault.storage().exists(&key_path) {
        let _ = vault.note_locked_folder(&folder);
        return Ok(None);
    }
    let pass = passphrase.ok_or_else(|| VaultError::Locked(folder.clone()))?;
    let id = FolderId::generate().map_err(|e| damaged(&key_path, &e))?;
    let key = FolderKey::generate().map_err(|e| damaged(&key_path, &e))?;
    let kf = KeyFile::new(id, &key, pass, kdf, now_ms).map_err(|e| damaged(&key_path, &e))?;
    // From here on the folder is locked: agents can't see it and nothing
    // plain can be written into it.
    vault.storage().write_atomic(&key_path, &kf.to_json())?;
    // The key file as stored must give the key back, before any file is converted.
    let (_, stored) = vault.key_file(&folder)?;
    let again = stored.unwrap_key(pass).map_err(|e| damaged(&key_path, &e))?;
    stored.check_key(&again).map_err(|e| damaged(&key_path, &e))?;
    vault.keys().insert(&folder, id, key.clone());
    vault.note_locked_folder(&folder)?;
    Ok(Some(key.recovery_key()))
}

/// Lock `folder`, or finish locking it.
///
/// * First run: a new key, wrapped by `passphrase` into `quire-lock.json`
///   ([`begin_lock`]); the report carries the recovery key.
/// * Again (resuming, or plain files that arrived later): `passphrase`
///   unlocks the existing key, unless the folder is unlocked already.
///
/// Files are converted one at a time (only one file's bytes are held at
/// once). Each plain file is encrypted to `name.qenc`, read back, decrypted
/// and compared; its fingerprint goes into a journal first. Only when every
/// file passed are the plain ones removed, each only if it still has the
/// fingerprint recorded. A plain file next to an encrypted copy with other
/// content is the other way round: it is removed only if it is the original
/// this device's journal recorded (the encrypted copy is then newer, from
/// another device); otherwise both are kept and it is reported in
/// [`ConvertReport::conflicts`]. Then the history of the folder's notes is
/// encrypted. The folder is left unlocked in this vault's key ring.
pub fn lock_folder(vault: &Vault, folder: &str, passphrase: Option<&str>, kdf: Kdf, now_ms: i64) -> Result<ConvertReport> {
    let folder = folder.trim().trim_matches('/').to_string();
    let mut report = ConvertReport { recovery_key: begin_lock(vault, &folder, passphrase, kdf, now_ms)?, ..ConvertReport::default() };
    let key_path = NotePath::join(&folder, KEY_FILE);
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
    let mut journal = Journal::load(vault, id, &folder);

    // Phase 1: an encrypted, verified copy of every plain file, one at a time.
    let plain: Vec<NotePath> =
        stored_files(vault, &folder)?.into_iter().filter(|p| !is_encrypted_name(p.as_str()) && !is_key_file(p.as_str())).collect();
    let mut verified: Vec<(NotePath, String)> = Vec::with_capacity(plain.len());
    for p in &plain {
        let bytes = vault.storage().read_bytes(p)?;
        let tag = key.tag(&bytes);
        let enc = stored(p);
        let existing = match vault.storage().read_bytes(&enc) {
            Ok(b) => Some(b),
            Err(VaultError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let recorded = journal.files.get(p.as_str()) == Some(&tag);
        match existing.map(|b| key.open(Kind::File, id, &b)) {
            // Already encrypted, same bytes.
            Some(Ok(b)) if b == bytes => {}
            // The original this device encrypted; the encrypted copy changed since (another device's edit): it wins.
            Some(Ok(_)) if recorded => {}
            // A plain file next to an encrypted one it doesn't match: both stay, for the person to decide.
            Some(_) if !recorded => {
                report.conflicts.push(p.clone());
                continue;
            }
            // No encrypted copy yet (or this device's own copy can't be read): encrypt it.
            _ => {
                journal.files.insert(p.as_str().to_string(), tag.clone());
                journal.save(vault, id)?;
                let sealed = key.seal(Kind::File, id, &bytes).map_err(|e| damaged(&enc, &e))?;
                vault.storage().write_bytes_atomic(&enc, &sealed)?;
                let back = vault.storage().read_bytes(&enc)?;
                if key.open(Kind::File, id, &back).map_err(|e| damaged(&enc, &e))? != bytes {
                    return Err(VaultError::Damaged {
                        path: enc.to_string(),
                        reason: "the encrypted copy didn't read back the same".into(),
                    });
                }
            }
        }
        verified.push((p.clone(), tag));
        report.files += 1;
    }
    // Phase 2: remove the plain files, each only if it is still what was encrypted.
    for (p, tag) in &verified {
        match vault.storage().read_bytes(p) {
            Ok(now) if key.tag(&now) == *tag => vault.storage().remove(p)?,
            // Changed by another program meanwhile: kept next to the encrypted copy.
            Ok(_) => report.conflicts.push(p.clone()),
            Err(VaultError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    if vault.storage().exists(&journal_path(id)) {
        vault.storage().remove(&journal_path(id))?;
    }
    // Phase 3: the history of the folder's notes, and of notes that were in it before.
    for note in vault.locked_notes(&folder).into_iter().chain(vault.history_paths_under(&folder)) {
        report.history += vault.rehome_history(&note, &note)?;
    }
    report.conflicts.sort();
    report.conflicts.dedup();
    Ok(report)
}

/// Keep both sides of each conflict ([`PlainLeft::conflicts`]) in `folder`.
///
/// Each plain file next to an encrypted copy with other content is
/// encrypted under a new name (`Day 1 (plain copy).md`) and the plain file
/// removed. Needs the folder unlocked. Returns the new names.
pub fn keep_both(vault: &Vault, folder: &str) -> Result<Vec<NotePath>> {
    let folder = folder.trim().trim_matches('/').to_string();
    let f = vault.locked_folder(&NotePath::join(&folder, "x")).filter(|f| f.path == folder && !f.unlocking);
    let f = f.ok_or_else(|| VaultError::InvalidName(format!("{folder}/ isn't a locked folder")))?;
    if !vault.keys().is_unlocked(&folder) {
        return Err(VaultError::Locked(folder));
    }
    let mut out = Vec::new();
    let conflicts = plain_left(vault).into_iter().filter(|l| l.folder == folder).flat_map(|l| l.conflicts).collect::<Vec<_>>();
    for p in conflicts {
        let bytes = vault.storage().read_bytes(&p)?;
        let to = copy_name(vault, &p, "plain copy");
        vault.write_locked(&f, &to, &bytes)?;
        if vault.read_file(&to)? != bytes {
            return Err(VaultError::Damaged { path: to.to_string(), reason: "the encrypted copy didn't read back the same".into() });
        }
        if vault.storage().read_bytes(&p).is_ok_and(|now| now == bytes) {
            vault.storage().remove(&p)?;
        }
        out.push(to);
    }
    Ok(out)
}

/// Make `folder` an ordinary folder again, or finish doing so. Needs the
/// folder unlocked (or `passphrase`).
///
/// The key file is renamed to `quire-lock.unlocking.json` first (the folder
/// takes plain files from then on, for this process); each `.qenc` file is
/// decrypted to its plain name and read back; then the encrypted files and
/// the key file go to `.trash/` (they hold only ciphertext), and history is
/// decrypted. A plain file that is already there is kept (it's what the
/// last run wrote, or a newer edit); if it differs from its encrypted copy,
/// that copy is decrypted next to it under a new name rather than lost.
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
    // From here on this process writes the folder plain.
    let _converting = vault.converting(&folder);
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
        let bytes = key.open(Kind::File, id, &vault.storage().read_bytes(enc)?).map_err(|e| damaged(enc, &e))?;
        let target = match vault.storage().read_bytes(&plain) {
            Ok(now) if now == bytes => continue,
            // A plain file with other content: keep it, and this one beside it.
            Ok(_) => {
                let c = copy_name(vault, &plain, "encrypted copy");
                report.conflicts.push(c.clone());
                c
            }
            Err(VaultError::NotFound(_)) => plain,
            Err(e) => return Err(e),
        };
        vault.storage().write_bytes_atomic(&target, &bytes)?;
        if vault.storage().read_bytes(&target)? != bytes {
            return Err(VaultError::Damaged { path: target.to_string(), reason: "the plain copy didn't read back the same".into() });
        }
        report.files += 1;
    }
    for enc in &encrypted {
        if plain_name(enc).is_some_and(|p| vault.storage().exists(&p)) {
            vault.storage().trash(enc)?;
        }
    }
    vault.storage().trash(&unlocking)?;
    vault.forget_locked_folder(&folder);
    vault.keys().remove(&folder);
    Ok(report)
}

/// A free name beside `p`: `Day 1 (what).md`, `Day 1 (what 2).md`…
fn copy_name(vault: &Vault, p: &NotePath, what: &str) -> NotePath {
    let (stem, ext) = match p.file_name().rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (p.file_name().to_string(), String::new()),
    };
    let mut n = 1;
    loop {
        let name = if n == 1 { format!("{stem} ({what}){ext}") } else { format!("{stem} ({what} {n}){ext}") };
        let c = NotePath::join(p.folder(), &name);
        if !vault.storage().exists(&c) && !vault.storage().exists(&stored(&c)) {
            return c;
        }
        n += 1;
    }
}

/// Plain files in locked folders (not counting folders being unlocked for
/// good): what an interrupted lock or another program left.
pub fn plain_left(vault: &Vault) -> Vec<PlainLeft> {
    let all = vault.storage().list().unwrap_or_default();
    vault
        .locked_folders()
        .into_iter()
        .map(|s| s.folder)
        .filter(|f| !f.unlocking)
        .filter_map(|f| {
            let (mut files, mut conflicts) = (Vec::new(), Vec::new());
            for p in all.iter().filter(|p| f.contains(p.as_str()) && !is_encrypted_name(p.as_str()) && !is_key_file(p.as_str())) {
                if all.binary_search(&stored(p)).is_ok() || vault.storage().exists(&stored(p)) {
                    conflicts.push(p.clone());
                } else {
                    files.push(p.clone());
                }
            }
            (!files.is_empty() || !conflicts.is_empty()).then_some(PlainLeft { folder: f.path, files, conflicts })
        })
        .collect()
}
