// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vault: a folder of Markdown files.
//!
//! [`Storage`] is the port; [`LocalFs`] and [`MemFs`] are adapters. [`Vault`]
//! is the repository that knows about notes, names, templates and refactors.

mod hash;
mod history;
pub mod lock;
mod locked;
mod note;
pub mod prefs;
mod refactor;
mod storage;
pub mod templates;
mod vault;
mod watch;

pub use hash::ContentHash;
pub use history::{Snapshot, retained};
pub use locked::LockedFolderState;
pub use note::{NotePath, sanitize_title};
pub use prefs::{PrefsError, VaultPrefs};
pub use refactor::{LinkRewrite, rewrite_links};
pub use storage::{LocalFs, MemFs, SingleFile, Storage};
pub use templates::{TemplateVars, expand_template};
pub use vault::{RenameReport, Vault, VaultConfig};
pub use watch::{VaultEvent, VaultWatcher};

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("io error on {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("note not found: {0}")]
    NotFound(String),
    #[error("a note named {0} already exists")]
    Exists(String),
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("watcher error: {0}")]
    Watch(String),
    /// A locked folder (0.11): its notes can't be read or written until it
    /// is unlocked, and nothing plain may be written into it.
    #[error("{0}/ is locked")]
    Locked(String),
    /// An encrypted file that doesn't decrypt (damaged, changed, moved from
    /// another locked folder).
    #[error("{path} can't be decrypted: {reason}")]
    Damaged { path: String, reason: String },
}

pub type Result<T> = std::result::Result<T, VaultError>;

pub(crate) fn io_err(path: impl Into<String>) -> impl FnOnce(std::io::Error) -> VaultError {
    let path = path.into();
    move |source| VaultError::Io { path, source }
}
