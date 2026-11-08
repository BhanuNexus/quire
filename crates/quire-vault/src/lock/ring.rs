// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The keys of unlocked folders, in memory only.

use std::sync::{Arc, PoisonError, RwLock};

use super::format::{FolderId, FolderKey};

struct Unlocked {
    folder: String,
    id: FolderId,
    key: FolderKey,
    /// The key before "Change the folder's key", for files not encrypted
    /// again yet.
    previous: Option<FolderKey>,
}

/// The keys of the folders unlocked in this process.
///
/// Shared by every clone of a [`crate::Vault`] (cheap to clone); keys are
/// wiped when removed. Each entry is boxed, so a growing list moves
/// pointers, never leaving a copy of a key behind in freed memory.
#[derive(Clone, Default)]
#[expect(clippy::vec_box, reason = "boxed so that keys never move when the list grows (no copies left in freed memory)")]
pub struct KeyRing(Arc<RwLock<Vec<Box<Unlocked>>>>);

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.unlocked()).finish()
    }
}

impl KeyRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep `key` for `folder` (replacing an earlier one).
    pub fn insert(&self, folder: &str, id: FolderId, key: FolderKey) {
        let mut v = self.0.write().unwrap_or_else(PoisonError::into_inner);
        v.retain(|u| u.folder != folder);
        v.push(Box::new(Unlocked { folder: folder.to_string(), id, key, previous: None }));
    }

    /// Keep `previous` as `folder`'s key before a key change.
    pub fn set_previous(&self, folder: &str, previous: Option<FolderKey>) {
        let mut v = self.0.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(u) = v.iter_mut().find(|u| u.folder == folder) {
            u.previous = previous;
        }
    }

    /// `folder`'s key before a key change, if it had one.
    pub fn previous(&self, folder: &str) -> Option<FolderKey> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().find(|u| u.folder == folder).and_then(|u| u.previous.clone())
    }

    /// The previous key of the unlocked folder with this id.
    pub fn previous_by_id(&self, id: FolderId) -> Option<FolderKey> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().find(|u| u.id == id).and_then(|u| u.previous.clone())
    }

    /// Forget `folder`'s key. Returns whether it was unlocked.
    pub fn remove(&self, folder: &str) -> bool {
        let mut v = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let before = v.len();
        v.retain(|u| u.folder != folder);
        before != v.len()
    }

    /// Forget every key.
    pub fn clear(&self) {
        self.0.write().unwrap_or_else(PoisonError::into_inner).clear();
    }

    /// The unlocked folders.
    pub fn unlocked(&self) -> Vec<String> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().map(|u| u.folder.clone()).collect()
    }

    pub fn is_unlocked(&self, folder: &str) -> bool {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().any(|u| u.folder == folder)
    }

    /// The id and key of an unlocked folder.
    pub fn get(&self, folder: &str) -> Option<(FolderId, FolderKey)> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().find(|u| u.folder == folder).map(|u| (u.id, u.key.clone()))
    }

    /// The key of the unlocked folder with this id (for a history snapshot,
    /// whose header names its folder).
    pub fn by_id(&self, id: FolderId) -> Option<FolderKey> {
        self.0.read().unwrap_or_else(PoisonError::into_inner).iter().find(|u| u.id == id).map(|u| u.key.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.0.read().unwrap_or_else(PoisonError::into_inner).is_empty()
    }
}
