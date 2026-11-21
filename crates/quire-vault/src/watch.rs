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

    /// The one rule for a change read through the watcher: a `Changed` (or
    /// `AttachmentChanged`) path that is not a regular file below regular
    /// folders ([`crate::Vault::is_regular_file`]: a symlink, a file below a
    /// symlinked folder, a missing file) becomes a removal, so a consumer
    /// never reads through a link and a note swapped for one leaves the
    /// index. Every consumer that upserts from events calls this first (the
    /// MCP server, the agent panel's tool host, the workspace).
    #[must_use]
    pub fn regular(self, vault: &crate::Vault) -> Self {
        match self {
            Self::Changed(p) if !vault.is_regular_file(&p) => Self::Removed(p),
            Self::AttachmentChanged(p) if !vault.is_regular_file(&p) => Self::AttachmentRemoved(p),
            other => other,
        }
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
        // Without this first scan, the first rescan reports every file as changed: more work, nothing lost.
        #[expect(clippy::let_underscore_must_use, reason = "best effort, see above")]
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
        match std::fs::symlink_metadata(path) {
            #[expect(clippy::filetype_is_file, reason = "a symlinked note is skipped, as the vault listing skips it")]
            Ok(m) if m.file_type().is_file() => {
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
        let started = std::thread::Builder::new().name("quire-watch-rescan".into()).spawn(move || {
            std::thread::sleep(RESCAN_SETTLE);
            me.rescan_now();
        });
        if started.is_err() {
            // No thread now: a later ask tries again, instead of waiting forever for this one.
            self.pending.store(false, Ordering::SeqCst);
        }
    }

    fn rescan_now(&self) {
        let _one = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        // Asks from here on need another rescan: this one may be too early for them.
        self.pending.store(false, Ordering::SeqCst);
        let now = scan(&self.root);
        let before = self.seen.lock().unwrap_or_else(PoisonError::into_inner).replace(now.clone()).unwrap_or_default();
        for ev in differences(&before, &now) {
            if self.tx.send(ev).is_err() {
                break; // the watcher was dropped
            }
        }
    }
}

/// The event for one path a notify event names. Links are not followed (one
/// `symlink_metadata`, as [`scan`] does): a folder is no event, and a
/// symlinked file counts as gone, so it is never read and a note replaced by
/// a link is removed.
fn event_for(root: &Path, p: &Path, kind: EventKind) -> Option<VaultEvent> {
    let rel = NotePath::new(p.strip_prefix(root).ok()?.to_string_lossy());
    let meta = std::fs::symlink_metadata(p);
    if meta.as_ref().is_ok_and(std::fs::Metadata::is_dir) {
        return None;
    }
    #[expect(clippy::filetype_is_file, reason = "a symlinked note is not a note, as the vault listing skips it")]
    let regular = meta.is_ok_and(|m| m.file_type().is_file());
    VaultEvent::from_fs(rel, kind, regular)
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
                if let Some(msg) = event_for(&root_for_cb, p, ev.kind) {
                    on_event.saw(&rel, p);
                    #[expect(clippy::let_underscore_must_use, reason = "the receiver is gone only when the watcher was dropped")]
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

    /// A symlinked note is not a note: the notify callback reports it as gone (so a note swapped
    /// for a link leaves the index), whatever it points at; a regular one is a change.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_reported_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        std::fs::create_dir_all(root.join("Notes")).unwrap();
        std::fs::create_dir_all(root.join("Private")).unwrap();
        std::fs::write(root.join("Private/diary.md"), "secret").unwrap();
        std::fs::write(root.join("Notes/a.md"), "a").unwrap();
        std::os::unix::fs::symlink(root.join("Private/diary.md"), root.join("Notes/link.md")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere.md"), root.join("Notes/dangling.md")).unwrap();
        let create = EventKind::Create(CreateKind::File);
        let ev = |rel: &str| event_for(&root, &root.join(rel), create);
        assert_eq!(ev("Notes/a.md"), Some(VaultEvent::Changed("Notes/a.md".into())));
        assert_eq!(ev("Notes/link.md"), Some(VaultEvent::Removed("Notes/link.md".into())));
        assert_eq!(ev("Notes/dangling.md"), Some(VaultEvent::Removed("Notes/dangling.md".into())));
        assert_eq!(ev("Notes"), None, "a folder is not an event");
        // A regular note replaced by a link: gone.
        std::fs::remove_file(root.join("Notes/a.md")).unwrap();
        std::os::unix::fs::symlink(root.join("Private/diary.md"), root.join("Notes/a.md")).unwrap();
        assert_eq!(ev("Notes/a.md"), Some(VaultEvent::Removed("Notes/a.md".into())));
    }

    /// [`VaultEvent::regular`]: a change to a path that is not a regular file below regular
    /// folders becomes a removal, at every consumer.
    #[cfg(unix)]
    #[test]
    fn changes_to_links_become_removals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        std::fs::create_dir_all(root.join("Notes")).unwrap();
        std::fs::create_dir_all(root.join("Private")).unwrap();
        std::fs::write(root.join("Notes/a.md"), "a").unwrap();
        std::fs::write(root.join("Private/diary.md"), "secret").unwrap();
        std::fs::write(dir.path().join("outside.md"), "out").unwrap();
        std::os::unix::fs::symlink(root.join("Private/diary.md"), root.join("Notes/to-deny.md")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.md"), root.join("Notes/to-outside.md")).unwrap();
        std::os::unix::fs::symlink(root.join("Private"), root.join("Alias")).unwrap();
        let vault = crate::Vault::open_local(&root).unwrap();
        let changed = |p: &str| VaultEvent::Changed(p.into()).regular(&vault);
        assert_eq!(changed("Notes/a.md"), VaultEvent::Changed("Notes/a.md".into()));
        for p in ["Notes/to-deny.md", "Notes/to-outside.md", "Alias/diary.md", "Notes/missing.md"] {
            assert_eq!(changed(p), VaultEvent::Removed(p.into()), "{p}");
        }
        assert_eq!(VaultEvent::Removed("Notes/a.md".into()).regular(&vault), VaultEvent::Removed("Notes/a.md".into()));
        // Reading: only a regular file's text, never a link's target.
        assert_eq!(vault.read_regular(&"Notes/a.md".into()).unwrap().as_deref(), Some("a"));
        assert_eq!(vault.read_regular(&"Notes/to-deny.md".into()).unwrap(), None);
        assert_eq!(vault.read_regular(&"Alias/diary.md".into()).unwrap(), None);
        // A vault in memory has no links.
        let mem = crate::Vault::in_memory(&[("A.md", "a")]);
        assert_eq!(VaultEvent::Changed("A.md".into()).regular(&mem), VaultEvent::Changed("A.md".into()));
    }
}
