// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender, unbounded};
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
        // Whether the file is there now decides, whatever the event says:
        // events can be merged or arrive late (macOS FSEvents reports a file
        // deleted and made again, as by git or a safe-save, as created then
        // removed), and a rename away looks like a change.
        let removed = match kind {
            EventKind::Remove(_) | EventKind::Create(_) | EventKind::Modify(_) => !exists,
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

/// What the watcher last knew of a file: its modification time and size.
type Seen = HashMap<NotePath, (Option<SystemTime>, u64)>;

/// Every visible file under `root`, with what [`Seen`] keeps. A folder
/// that goes away meanwhile is skipped.
fn scan(root: &Path) -> Seen {
    let mut out = Seen::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(rel) = p.strip_prefix(root) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if is_hidden(&rel) {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(p),
                #[expect(clippy::filetype_is_file, reason = "a symlinked note is skipped, as the vault listing skips it")]
                Ok(t) if t.is_file() => {
                    let m = e.metadata().ok();
                    out.insert(NotePath::new(rel), (m.as_ref().and_then(|m| m.modified().ok()), m.map_or(0, |m| m.len())));
                }
                _ => {}
            }
        }
    }
    out
}

/// The events that turn `before` into `now`: files new or changed (by time
/// or size), and files gone.
fn differences(before: &Seen, now: &Seen) -> Vec<VaultEvent> {
    let mut out: Vec<VaultEvent> = now
        .iter()
        .filter(|(p, m)| before.get(*p) != Some(m))
        .filter_map(|(p, _)| VaultEvent::from_fs(p.clone(), EventKind::Modify(notify::event::ModifyKind::Any), true))
        .chain(
            before
                .keys()
                .filter(|p| !now.contains_key(*p))
                .filter_map(|p| VaultEvent::from_fs(p.clone(), EventKind::Remove(notify::event::RemoveKind::Any), false)),
        )
        .collect();
    out.sort_by(|a, b| a.path().cmp(b.path()));
    out
}

/// Looks over the whole vault again when the watcher may have missed
/// events (a full event queue, a watcher error), and reports what changed
/// since the watcher last knew: on its own thread, at most one at a time,
/// and one for a burst of reasons.
struct Rescanner {
    root: PathBuf,
    tx: Sender<VaultEvent>,
    /// Every file as last seen (by the first scan, events, and rescans).
    seen: Mutex<Option<Seen>>,
    /// A rescan is asked for and hasn't started yet.
    pending: AtomicBool,
    /// Rescans run one at a time.
    running: Mutex<()>,
}

/// How long a rescan waits for more reasons to come in before it starts.
const RESCAN_SETTLE: Duration = Duration::from_millis(300);

impl Rescanner {
    fn new(root: PathBuf, tx: Sender<VaultEvent>) -> Arc<Self> {
        Arc::new(Self { root, tx, seen: Mutex::new(None), pending: AtomicBool::new(false), running: Mutex::new(()) })
    }

    /// Learn the vault as it is now, in the background (what a rescan compares with).
    fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("quire-watch-scan".into()).spawn(move || {
            let _one = me.running.lock().unwrap_or_else(PoisonError::into_inner);
            let now = scan(&me.root);
            let mut seen = me.seen.lock().unwrap_or_else(PoisonError::into_inner);
            // Events that came in meanwhile are newer than the scan.
            let mut merged = now;
            if let Some(s) = seen.take() {
                merged.extend(s);
            }
            *seen = Some(merged);
        });
    }

    /// Note what an event said about `rel`.
    fn saw(&self, rel: &NotePath, path: &Path) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let seen = seen.get_or_insert_with(Seen::new);
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() => {
                seen.insert(rel.clone(), (m.modified().ok(), m.len()));
            }
            _ => {
                seen.remove(rel);
            }
        }
    }

    /// Ask for a rescan; several asks before it starts make one.
    fn ask(self: &Arc<Self>) {
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new().name("quire-watch-rescan".into()).spawn(move || {
            std::thread::sleep(RESCAN_SETTLE);
            me.rescan_now();
        });
    }

    fn rescan_now(&self) {
        let _one = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        // Asks from here on need another rescan: this one may be too early for them.
        self.pending.store(false, Ordering::SeqCst);
        let now = scan(&self.root);
        let before = self.seen.lock().unwrap_or_else(PoisonError::into_inner).replace(now.clone()).unwrap_or_default();
        for ev in differences(&before, &now) {
            let _ = self.tx.send(ev);
        }
    }
}

/// Observer over the vault folder. Drop it to stop watching.
///
/// When the system says events were lost (an overflowing event queue, the
/// `Rescan` flag, or an error), the whole vault is looked over again in the
/// background and what changed is reported as ordinary events.
pub struct VaultWatcher {
    _inner: notify::RecommendedWatcher,
    pub events: Receiver<VaultEvent>,
}

impl VaultWatcher {
    pub fn start(root: &Path) -> Result<Self> {
        let (tx, rx) = unbounded();
        let root: PathBuf = root.to_path_buf();
        let rescan = Rescanner::new(root.clone(), tx.clone());
        let root_for_cb = root.clone();
        let on_event = Arc::clone(&rescan);
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return on_event.ask() };
            if ev.need_rescan() {
                on_event.ask();
            }
            for p in &ev.paths {
                let Ok(rel) = p.strip_prefix(&root_for_cb) else {
                    continue;
                };
                let rel = NotePath::new(rel.to_string_lossy());
                // Folders change too (a file added inside); only files are events.
                if p.is_dir() {
                    continue;
                }
                if let Some(msg) = VaultEvent::from_fs(rel.clone(), ev.kind, p.exists()) {
                    on_event.saw(&rel, p);
                    let _ = tx.send(msg);
                }
            }
        })
        .map_err(|e| VaultError::Watch(e.to_string()))?;
        w.watch(&root, RecursiveMode::Recursive).map_err(|e| VaultError::Watch(e.to_string()))?;
        rescan.start();
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
        // Reliability scan item 9: a removal of a file that is there again
        // (deleted and made again, events merged) is a change.
        assert_eq!(ev("A.md", EventKind::Remove(RemoveKind::File), true), Some(VaultEvent::Changed("A.md".into())));
    }

    /// Reliability scan item 9: after lost events, a rescan reports what
    /// changed since the watcher last knew, as ordinary events.
    #[test]
    fn a_rescan_reports_what_the_lost_events_would_have() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |p: &str, t: &str| {
            let abs = root.join(p);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, t).unwrap();
        };
        write("Kept.md", "same");
        write("Edited.md", "one");
        write("Gone.md", "x");
        write("pic.png", "p");
        write(".quire/cache/x", "hidden");
        let (tx, rx) = unbounded();
        let r = Rescanner::new(root.clone(), tx);
        r.rescan_now();
        // The first look reports every file (nothing was known before).
        assert_eq!(rx.try_iter().count(), 4);
        write("Edited.md", "one, then more");
        std::fs::remove_file(root.join("Gone.md")).unwrap();
        write("New/Note.md", "n");
        std::fs::remove_file(root.join("pic.png")).unwrap();
        r.rescan_now();
        let got: Vec<VaultEvent> = rx.try_iter().collect();
        assert_eq!(
            got,
            [
                VaultEvent::Changed("Edited.md".into()),
                VaultEvent::Removed("Gone.md".into()),
                VaultEvent::Changed("New/Note.md".into()),
                VaultEvent::AttachmentRemoved("pic.png".into()),
            ]
        );
        // An event the watcher did see is not reported again.
        write("Kept.md", "changed, and seen");
        r.saw(&NotePath::new("Kept.md"), &root.join("Kept.md"));
        r.rescan_now();
        assert_eq!(rx.try_iter().count(), 0);
        // Many asks at once make one rescan.
        write("Kept.md", "changed again, not seen");
        for _ in 0..20 {
            r.ask();
        }
        let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(first, VaultEvent::Changed("Kept.md".into()));
        std::thread::sleep(RESCAN_SETTLE * 2);
        assert_eq!(rx.try_iter().count(), 0);
    }
}
