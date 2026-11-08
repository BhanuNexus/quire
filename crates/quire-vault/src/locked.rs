// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! [`Vault`] and locked folders: reading and writing notes by their plain
//! names, unlocking and locking (see [`crate::lock`]).

use crate::lock::{self, FolderKey, KeyFile, KeyRing, Kind, LockError, LockedFolder};
use crate::{NotePath, RenameReport, Result, Vault, VaultError};

/// A locked folder and whether this process holds its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedFolderState {
    pub folder: LockedFolder,
    pub unlocked: bool,
    /// Remembered as locked, but its key file is gone (or renamed to the
    /// `unlocking` name, which this process didn't do): plain writes are
    /// refused until it is restored or rebuilt with the recovery key.
    pub key_missing: bool,
}

pub(crate) fn damaged(path: &NotePath, e: &LockError) -> VaultError {
    VaultError::Damaged { path: path.to_string(), reason: e.to_string() }
}

impl Vault {
    /// The keys of the folders this process unlocked.
    pub const fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// The same vault with no keys: what to hand to agents and anything that
    /// must never read a locked note, even an unlocked one.
    #[must_use]
    pub fn without_keys(&self) -> Self {
        Self { keys: KeyRing::new(), ..self.clone() }
    }

    /// The locked folder `p` is in, if any: one with a key file, or one
    /// this device remembers as locked ([`lock::Known`]) whose key file is
    /// missing. A key file under the `unlocking` name counts as unlocking
    /// only while this process runs that conversion, or for a folder this
    /// device never knew as locked.
    pub fn locked_folder(&self, p: &NotePath) -> Option<LockedFolder> {
        match lock::folder_of(self.storage.as_ref(), p.as_str()) {
            Some(f) if f.unlocking && self.known.contains(&f.path) && !self.known.is_converting(&f.path) => {
                Some(LockedFolder { path: f.path, unlocking: false })
            }
            Some(f) => Some(f),
            None => self.known.folder_of(p.as_str()).map(|path| LockedFolder { path, unlocking: false }),
        }
    }

    /// Every locked folder of the vault and whether it is unlocked here,
    /// remembered ones whose key file is missing included.
    pub fn locked_folders(&self) -> Vec<LockedFolderState> {
        let all = self.storage.list().unwrap_or_default();
        let on_disk = lock::folders_in(&all);
        let mut out: Vec<LockedFolderState> = on_disk
            .iter()
            .map(|f| {
                let refused = f.unlocking && self.known.contains(&f.path) && !self.known.is_converting(&f.path);
                let folder = LockedFolder { path: f.path.clone(), unlocking: f.unlocking && !refused };
                LockedFolderState { unlocked: self.keys.is_unlocked(&f.path), folder, key_missing: refused }
            })
            .collect();
        for k in self.known.folders() {
            if !on_disk.iter().any(|f| f.path == k.folder || f.contains(&k.folder)) {
                let folder = LockedFolder { path: k.folder.clone(), unlocking: false };
                out.push(LockedFolderState { unlocked: self.keys.is_unlocked(&k.folder), folder, key_missing: true });
            }
        }
        out.sort_by(|a, b| a.folder.path.cmp(&b.folder.path));
        out
    }

    /// The files of a locked folder by their plain names (`Journal/Day.md`
    /// for `Journal/Day.md.qenc`, and plain files a conversion hasn't
    /// reached yet), without the key file. Names aren't secret: this works
    /// while the folder is locked.
    pub fn locked_files(&self, folder: &str) -> Vec<NotePath> {
        let f = LockedFolder { path: folder.trim_matches('/').to_string(), unlocking: false };
        let mut v: Vec<NotePath> = self
            .storage
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| f.contains(p.as_str()) && !lock::is_key_file(p.as_str()))
            .map(|p| lock::plain_name(&p).unwrap_or(p))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The notes (`.md`) of a locked folder by their plain names.
    pub fn locked_notes(&self, folder: &str) -> Vec<NotePath> {
        self.locked_files(folder).into_iter().filter(NotePath::is_markdown).collect()
    }

    /// A file's bytes, decrypted when it is in a locked folder.
    pub fn read_file(&self, p: &NotePath) -> Result<Vec<u8>> {
        match self.locked_folder(p) {
            Some(f) => self.read_locked(&f, p),
            None => self.storage.read_bytes(p),
        }
    }

    /// Write a file's bytes, encrypted when it is in a locked folder.
    pub fn write_file(&self, p: &NotePath, bytes: &[u8]) -> Result<()> {
        match self.locked_folder(p) {
            Some(f) => self.write_locked(&f, p, bytes),
            None => self.storage.write_bytes_atomic(p, bytes),
        }
    }

    /// The key file of `folder` (either name).
    pub fn key_file(&self, folder: &str) -> Result<(LockedFolder, KeyFile)> {
        let folder = folder.trim_matches('/');
        for (name, unlocking) in [(lock::KEY_FILE, false), (lock::UNLOCKING_FILE, true)] {
            let p = NotePath::join(folder, name);
            match self.storage.read(&p) {
                Ok(text) => {
                    let kf = KeyFile::parse(&text).map_err(|e| damaged(&p, &e))?;
                    return Ok((LockedFolder { path: folder.to_string(), unlocking }, kf));
                }
                Err(VaultError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(VaultError::NotFound(format!("{folder}/{}", lock::KEY_FILE)))
    }

    /// Unlock `folder` with its passphrase. The key stays in this process's
    /// memory until [`Self::lock_folder_now`].
    pub fn unlock(&self, folder: &str, passphrase: &str) -> Result<FolderKey> {
        let (f, kf) = self.key_file(folder)?;
        let key = kf.unwrap_key(passphrase).map_err(|e| match e {
            LockError::WrongPassphrase => VaultError::Locked(format!("{} (wrong passphrase)", f.path)),
            e => damaged(&f.key_file(), &e),
        })?;
        let id = kf.id().map_err(|e| damaged(&f.key_file(), &e))?;
        self.keys.insert(&f.path, id, key.clone());
        Ok(key)
    }

    /// Unlock `folder` with its key (a recovery key, the OS keychain). When
    /// the key file is missing, the key must open the folder's encrypted
    /// files (or match the check value this device remembered).
    pub fn unlock_with_key(&self, folder: &str, key: FolderKey) -> Result<()> {
        let folder = folder.trim_matches('/');
        match self.key_file(folder) {
            Ok((f, kf)) => {
                kf.check_key(&key).map_err(|e| damaged(&f.key_file(), &e))?;
                let id = kf.id().map_err(|e| damaged(&f.key_file(), &e))?;
                self.keys.insert(&f.path, id, key);
            }
            Err(VaultError::NotFound(_)) if self.known.contains(folder) => {
                let id = self.check_key_without_key_file(folder, &key)?;
                self.keys.insert(folder, id, key);
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Forget `folder`'s key. Returns whether it was unlocked. (The caller
    /// closes open notes first.)
    pub fn lock_folder_now(&self, folder: &str) -> bool {
        self.keys.remove(folder.trim_matches('/'))
    }

    /// Give `folder` a new passphrase. Needs the folder unlocked (or `key`).
    /// Only the key file changes. A lost key file is rebuilt from the
    /// recovery key, which must first prove to be this folder's: it must
    /// open one of the folder's encrypted files, or, in a folder with none,
    /// match the check value this device remembered. Otherwise nothing is
    /// written.
    pub fn change_passphrase(&self, folder: &str, key: Option<FolderKey>, passphrase: &str, now_ms: i64) -> Result<()> {
        let folder = folder.trim_matches('/');
        let key = key.or_else(|| self.keys.get(folder).map(|(_, k)| k)).ok_or_else(|| VaultError::Locked(folder.to_string()))?;
        let (path, kf) = match self.key_file(folder) {
            Ok((f, mut kf)) => {
                kf.rewrap(&key, passphrase).map_err(|e| damaged(&f.key_file(), &e))?;
                (f.key_file(), kf)
            }
            Err(VaultError::NotFound(_)) => {
                let id = self.check_key_without_key_file(folder, &key)?;
                let p = NotePath::join(folder, lock::KEY_FILE);
                (p.clone(), KeyFile::new(id, &key, passphrase, lock::Kdf::DEFAULT, now_ms).map_err(|e| damaged(&p, &e))?)
            }
            Err(e) => return Err(e),
        };
        self.storage.write_atomic(&path, &kf.to_json())?;
        let _ = self.note_locked_folder(folder);
        Ok(())
    }

    /// The folder id `key` is good for, when `folder`'s key file is lost:
    /// the key must open one of its encrypted files, or, with none there,
    /// match the check value remembered for the folder.
    fn check_key_without_key_file(&self, folder: &str, key: &FolderKey) -> Result<lock::FolderId> {
        let inside = LockedFolder { path: folder.to_string(), unlocking: false };
        let encrypted: Vec<NotePath> = self
            .storage
            .list()?
            .into_iter()
            .filter(|p| inside.contains(p.as_str()) && lock::is_encrypted_name(p.as_str()))
            .take(8)
            .collect();
        let bad = || damaged(&NotePath::join(folder, lock::KEY_FILE), &LockError::BadRecoveryKey);
        let known = self.known.get(folder);
        if !encrypted.is_empty() {
            for p in &encrypted {
                let Ok(bytes) = self.storage.read_bytes(p) else { continue };
                let Ok(h) = lock::format::Header::parse(&bytes) else { continue };
                if known.as_ref().is_some_and(|k| k.id != h.folder.to_hex()) {
                    continue;
                }
                if key.open(Kind::File, h.folder, &bytes).is_ok() {
                    return Ok(h.folder);
                }
            }
            return Err(bad());
        }
        match known.filter(|k| !k.check.is_empty()) {
            Some(k) => {
                let id = lock::FolderId::from_hex(&k.id).ok_or_else(bad)?;
                if key.matches_check(&k.check) { Ok(id) } else { Err(bad()) }
            }
            None => Err(VaultError::NotFound(format!(
                "{folder}/{} is missing, and the folder has no encrypted file to check the recovery key against; restore the key file from a backup",
                lock::KEY_FILE
            ))),
        }
    }

    /// The bytes of a file in a locked folder.
    ///
    /// Locked: the encrypted copy (`X.qenc`) is the file. A plain `X` next to
    /// it is not read: anyone who can write the folder could have put it
    /// there ([`lock::plain_left`] reports it as a conflict). A plain `X`
    /// with no encrypted copy is a file a conversion hasn't reached yet.
    /// Being unlocked for good: the plain copy is the file.
    pub(crate) fn read_locked(&self, f: &LockedFolder, p: &NotePath) -> Result<Vec<u8>> {
        if !f.unlocking && !self.keys.is_unlocked(&f.path) {
            return Err(VaultError::Locked(f.path.clone()));
        }
        let stored = lock::stored(p);
        if f.unlocking {
            match self.storage.read_bytes(p) {
                Ok(b) => return Ok(b),
                Err(VaultError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let data = match self.storage.read_bytes(&stored) {
            Ok(d) => d,
            Err(VaultError::NotFound(_)) if f.unlocking => return Err(VaultError::NotFound(p.to_string())),
            Err(VaultError::NotFound(_)) => return self.storage.read_bytes(p),
            Err(e) => return Err(e),
        };
        let (id, key) = self.keys.get(&f.path).ok_or_else(|| VaultError::Locked(f.path.clone()))?;
        key.open(Kind::File, id, &data).map_err(|e| damaged(&stored, &e))
    }

    /// Store a file of a locked folder, encrypted (plain while the folder
    /// is being unlocked for good). A plain leftover with no encrypted copy
    /// is replaced; a plain file next to an encrypted copy is left alone (a
    /// conflict for the person to look at, never written over).
    pub(crate) fn write_locked(&self, f: &LockedFolder, p: &NotePath, bytes: &[u8]) -> Result<()> {
        if f.unlocking {
            return self.storage.write_bytes_atomic(p, bytes);
        }
        let (id, key) = self.keys.get(&f.path).ok_or_else(|| VaultError::Locked(f.path.clone()))?;
        let stored = lock::stored(p);
        let had_encrypted = self.storage.exists(&stored);
        let sealed = key.seal(Kind::File, id, bytes).map_err(|e| damaged(&stored, &e))?;
        self.storage.write_bytes_atomic(&stored, &sealed)?;
        if !had_encrypted && self.storage.exists(p) {
            // The plain copy an interrupted conversion left; the note is the encrypted file now.
            self.storage.remove(p)?;
        }
        Ok(())
    }

    /// [`Self::write_if`] in a locked folder: compared with the decrypted
    /// text, written as one step against the encrypted file as it was read.
    pub(crate) fn write_locked_if(&self, f: &LockedFolder, p: &NotePath, expected: Option<&str>, content: &str) -> Result<bool> {
        if f.unlocking {
            // Plain (being unlocked for good): compare and write.
            let now = match self.read_locked(f, p) {
                Ok(b) => Some(crate::storage::decode(&b).into_owned()),
                Err(VaultError::NotFound(_)) => None,
                Err(e) => return Err(e),
            };
            if now.as_deref() != expected {
                return Ok(false);
            }
            self.write_locked(f, p, content.as_bytes())?;
            return Ok(true);
        }
        let (id, key) = self.keys.get(&f.path).ok_or_else(|| VaultError::Locked(f.path.clone()))?;
        let stored = lock::stored(p);
        let raw = match self.storage.read_bytes(&stored) {
            Ok(b) => Some(b),
            Err(VaultError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        // No encrypted copy yet: the note is a plain leftover (or new).
        let plain = if raw.is_none() {
            match self.storage.read_bytes(p) {
                Ok(b) => Some(b),
                Err(VaultError::NotFound(_)) => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let now = match (&raw, &plain) {
            (Some(b), _) => Some(key.open(Kind::File, id, b).map_err(|e| damaged(&stored, &e))?),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };
        let now = now.as_deref().map(crate::storage::decode);
        if now.as_deref() != expected {
            return Ok(false);
        }
        let sealed = key.seal(Kind::File, id, content.as_bytes()).map_err(|e| damaged(&stored, &e))?;
        if !self.storage.write_bytes_if(&stored, &|cur| cur == raw.as_deref(), &sealed)? {
            return Ok(false);
        }
        if let Some(b) = plain
            && self.storage.read_bytes(p).is_ok_and(|now| now == b)
        {
            // The leftover is in the encrypted file now.
            self.storage.remove(p)?;
        }
        Ok(true)
    }

    /// Rename or move a note into, out of or within a locked folder. Both
    /// ends must be readable (unlocked) and writable. Links to it aren't
    /// rewritten in locked notes (they aren't in [`Self::notes`]); links in
    /// plain notes follow as with any rename.
    pub(crate) fn move_locked(&self, from: &NotePath, to: &NotePath) -> Result<RenameReport> {
        let (src, dst) = (self.locked_folder(from), self.locked_folder(to));
        if let (Some(a), Some(b)) = (&src, &dst)
            && a.path == b.path
            && !self.storage.exists(from)
        {
            // Within one folder: the encrypted file moves as it is (names aren't bound).
            self.storage.rename(&lock::stored(from), &lock::stored(to))?;
            let _ = self.move_history(from, to);
            return Ok(RenameReport { old_path: from.clone(), new_path: to.clone(), updated: vec![] });
        }
        let bytes = self.read_file(from)?;
        if self.exists(to) {
            return Err(VaultError::Exists(to.to_string()));
        }
        self.write_file(to, &bytes)?;
        if self.read_file(to)? != bytes {
            return Err(VaultError::Damaged { path: to.to_string(), reason: "the copy didn't read back the same".into() });
        }
        // History follows the note, encrypted or decrypted to match where it is now.
        self.rehome_history(from, to)?;
        match &src {
            // The encrypted file goes to the trash (ciphertext).
            Some(_) => self.trash(from)?,
            // A plain note moved into a locked folder: the plain original is
            // removed, not trashed (a plain copy is what locking removes).
            None => self.storage.remove(from)?,
        }
        Ok(RenameReport { old_path: from.clone(), new_path: to.clone(), updated: vec![] })
    }
}
