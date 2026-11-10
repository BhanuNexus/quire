// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Noticing other processes: an MCP server or the CLI writing proposals,
//! inbox findings or the shared policy while the app is open (design §A.4
//! "Multi-process").
//!
//! The vault watcher ignores `.quire/`, so this is a second, small watcher
//! on just those folders (not recursive). Events are only a signal; the
//! app reloads what it shows after a short quiet period.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{RecursiveMode, Watcher};

/// The folders watched, relative to the vault root.
pub const WATCHED: [&str; 3] = [".quire/agent/proposals", ".quire/agent/inbox", ".quire/shared"];

/// Set when something changed in a watched folder.
pub struct AgentWatcher {
    dirty: Arc<AtomicBool>,
    _inner: notify::RecommendedWatcher,
}

impl AgentWatcher {
    /// Watch the agent folders of the vault at `root` (creating them).
    pub fn start(root: &Path) -> Result<Self, String> {
        let dirty = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dirty);
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok_and(|e| !e.kind.is_access()) {
                flag.store(true, Ordering::Release);
            }
        })
        .map_err(|e| e.to_string())?;
        for d in WATCHED {
            let dir = root.join(d);
            let _ = std::fs::create_dir_all(&dir);
            w.watch(&dir, RecursiveMode::NonRecursive).map_err(|e| e.to_string())?;
        }
        Ok(Self { dirty, _inner: w })
    }

    /// Whether anything changed since the last call.
    pub fn take(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_a_new_proposal_file() {
        let dir = tempfile::tempdir().unwrap();
        let w = AgentWatcher::start(dir.path()).unwrap();
        // macOS (FSEvents) can report the folders `start` made a moment
        // later; let those arrive and drop them before the real test.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = w.take();
        std::fs::write(dir.path().join(".quire/agent/proposals/p_X.json"), "{}").unwrap();
        let t = std::time::Instant::now();
        while !w.take() {
            assert!(t.elapsed().as_secs() < 10, "no event");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
