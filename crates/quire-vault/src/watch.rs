// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, unbounded};
use notify::{EventKind, RecursiveMode, Watcher};

use crate::storage::is_hidden;
use crate::{NotePath, Result, VaultError};

/// Changes made to the vault by something other than Quire (sync, git, another editor).
/// Quire's own writes also arrive here; consumers compare content to ignore echoes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultEvent {
    /// A Markdown note was created or changed.
    Changed(NotePath),
    /// A Markdown note was removed (or renamed away).
    Removed(NotePath),
    /// Any other visible file (PDF, image, canvas…) was created or changed.
    AttachmentChanged(NotePath),
    /// Any other visible file was removed.
    AttachmentRemoved(NotePath),
}

impl VaultEvent {
    /// Classify a filesystem event for a vault-relative path; `None` for
    /// hidden paths (`.quire/`, `.git/`, temp files) and other event kinds.
    pub(crate) fn from_fs(rel: NotePath, kind: EventKind, exists: bool) -> Option<Self> {
        if is_hidden(rel.as_str()) {
            return None;
        }
        let removed = match kind {
            EventKind::Remove(_) => true,
            EventKind::Create(_) | EventKind::Modify(_) => !exists,
            _ => return None,
        };
        Some(match (rel.is_markdown(), removed) {
            (true, false) => Self::Changed(rel),
            (true, true) => Self::Removed(rel),
            (false, false) => Self::AttachmentChanged(rel),
            (false, true) => Self::AttachmentRemoved(rel),
        })
    }

    /// The path the event is about.
    pub fn path(&self) -> &NotePath {
        match self {
            Self::Changed(p) | Self::Removed(p) | Self::AttachmentChanged(p) | Self::AttachmentRemoved(p) => p,
        }
    }
}

/// Observer over the vault folder. Drop it to stop watching.
pub struct VaultWatcher {
    _inner: notify::RecommendedWatcher,
    pub events: Receiver<VaultEvent>,
}

impl VaultWatcher {
    pub fn start(root: &Path) -> Result<Self> {
        let (tx, rx) = unbounded();
        let root: PathBuf = root.to_path_buf();
        let root_for_cb = root.clone();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            for p in &ev.paths {
                let Ok(rel) = p.strip_prefix(&root_for_cb) else {
                    continue;
                };
                let rel = NotePath::new(rel.to_string_lossy());
                // Folders change too (a file added inside); only files are events.
                if p.is_dir() {
                    continue;
                }
                if let Some(msg) = VaultEvent::from_fs(rel, ev.kind, p.exists()) {
                    let _ = tx.send(msg);
                }
            }
        })
        .map_err(|e| VaultError::Watch(e.to_string()))?;
        w.watch(&root, RecursiveMode::Recursive).map_err(|e| VaultError::Watch(e.to_string()))?;
        Ok(Self { _inner: w, events: rx })
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    use super::*;

    #[test]
    fn notes_and_attachments_are_told_apart() {
        let ev = |p: &str, k: EventKind, exists: bool| VaultEvent::from_fs(NotePath::new(p), k, exists);
        let create = EventKind::Create(CreateKind::File);
        assert_eq!(ev("A.md", create, true), Some(VaultEvent::Changed("A.md".into())));
        assert_eq!(ev("A.md", EventKind::Remove(RemoveKind::File), false), Some(VaultEvent::Removed("A.md".into())));
        assert_eq!(ev("papers/x.pdf", create, true), Some(VaultEvent::AttachmentChanged("papers/x.pdf".into())));
        assert_eq!(
            ev("b.canvas", EventKind::Modify(ModifyKind::Any), false),
            Some(VaultEvent::AttachmentRemoved("b.canvas".into())),
            "a rename away looks like a change to a missing file"
        );
        assert_eq!(ev(".quire/history/x/1.md.gz", create, true), None);
        assert_eq!(ev("notes/.tmpA1b2", create, true), None);
        assert_eq!(ev("A.md", EventKind::Access(notify::event::AccessKind::Any), true), None);
    }
}
