// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The keys of unlocked folders, in memory only.

use std::sync::{Arc, PoisonError, RwLock};

use super::format::{FolderId, FolderKey};

struct Unlocked {
    folder: String,
    id: FolderId,
    key: FolderKey,
}

/// The keys of the folders unlocked in this process. Shared by every clone
/// of a [`crate::Vault`] (cheap to clone); keys are wiped when removed.
#[derive(Clone, Default)]
pub struct KeyRing(Arc<RwLock<Vec<Unlocked>>>);

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
        v.push(Unlocked { folder: folder.to_string(), id, key });
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
