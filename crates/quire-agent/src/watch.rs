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

    use std::time::{Duration, Instant};

    /// Drops the events a watcher reports for what happened before the test
    /// began (macOS `FSEvents` can deliver the folders `start` made late) by
    /// waiting for a quiet stretch, not for a fixed time.
    fn settle(w: &AgentWatcher) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut quiet = 0;
        while quiet < 6 && Instant::now() < deadline {
            quiet = if w.take() { 0 } else { quiet + 1 };
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn notices_a_new_proposal_file() {
        let dir = tempfile::tempdir().unwrap();
        let w = AgentWatcher::start(dir.path()).unwrap();
        settle(&w);
        // Each round writes a new file and waits a while for it; an event
        // that was coalesced or late is covered by the next round's file,
        // and one that never comes fails at the deadline.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut n = 0;
        loop {
            assert!(Instant::now() < deadline, "no event");
            n += 1;
            std::fs::write(dir.path().join(format!(".quire/agent/proposals/p_{n}.json")), "{}").unwrap();
            let round = Instant::now();
            while round.elapsed() < Duration::from_secs(2) {
                if w.take() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
