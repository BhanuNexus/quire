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
    Changed(NotePath),
    Removed(NotePath),
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
                if is_hidden(rel.as_str()) || !rel.is_markdown() {
                    continue;
                }
                let msg = match ev.kind {
                    EventKind::Remove(_) => VaultEvent::Removed(rel),
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        if p.exists() {
                            VaultEvent::Changed(rel)
                        } else {
                            VaultEvent::Removed(rel)
                        }
                    }
                    _ => continue,
                };
                let _ = tx.send(msg);
            }
        })
        .map_err(|e| VaultError::Watch(e.to_string()))?;
        w.watch(&root, RecursiveMode::Recursive).map_err(|e| VaultError::Watch(e.to_string()))?;
        Ok(Self { _inner: w, events: rx })
    }
}
