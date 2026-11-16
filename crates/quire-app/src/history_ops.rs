// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Version history operations for the active note: list, diff, restore.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crossbeam_channel::{Sender, bounded, unbounded};
use quire_text::{EditKind, Selection, Transaction};
use quire_vault::{NotePath, Snapshot, Vault};
use similar::{ChangeTag, TextDiff};

use crate::workspace::{NoticeLevel, Workspace};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

enum Job {
    /// Keep `text` as a version.
    Keep {
        vault: Vault,
        path: NotePath,
        text: String,
        at: i64,
        force: bool,
        /// Provenance JSON for a version that isn't plain typing.
        prov: Option<serde_json::Value>,
    },
    /// Keep `text` only if the note has no versions yet (its original).
    KeepFirst {
        vault: Vault,
        path: NotePath,
        text: String,
        at: i64,
    },
    Flush(Sender<()>),
}

/// Writes version history on its own thread, in order, so saving a big note
/// never waits on compressing it. Readers call [`HistoryWriter::flush`] first.
///
/// A version that can't be written (a full disk, a folder that can't be
/// written, even a panic) is told once ([`tick`]), not on every save; once
/// a version is written again, the next failure is told again.
#[derive(Debug)]
pub(crate) struct HistoryWriter {
    tx: Sender<Job>,
    failure: Arc<Mutex<Failure>>,
}

/// What went wrong writing history, for the person.
#[derive(Debug, Default)]
struct Failure {
    /// Failing since the last version written: later failures stay quiet.
    failing: bool,
    /// The first failure, not yet told.
    untold: Option<String>,
}

impl Failure {
    fn note(&mut self, path: &NotePath, r: std::thread::Result<Result<(), String>>) {
        let error = match r {
            Ok(Ok(())) => {
                self.failing = false;
                return;
            }
            Ok(Err(e)) => e,
            Err(_) => "it stopped unexpectedly".to_string(),
        };
        if !self.failing {
            self.failing = true;
            self.untold = Some(format!(
                "Quire couldn't keep a version of “{}” in its history: {error}. Notes are still saved; version history may miss \
                 changes until this is fixed (check that the vault folder can be written and the disk isn't full).",
                path.stem()
            ));
        }
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("history job")
    }
}

impl HistoryWriter {
    pub(crate) fn spawn() -> Self {
        let (tx, rx) = unbounded::<Job>();
        let failure = Arc::new(Mutex::new(Failure::default()));
        let failed = Arc::clone(&failure);
        std::thread::Builder::new()
            .name("quire-history".into())
            .spawn(move || {
                for job in rx {
                    // A panic in one job is a failure to tell, not the end of history for the session.
                    let run = |f: &dyn Fn() -> quire_vault::Result<bool>| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f().map(|_| ()).map_err(|e| e.to_string())))
                    };
                    match job {
                        Job::Keep { vault, path, text, at, force, prov } => {
                            let r = run(&|| vault.snapshot_with(&path, &text, at, force, prov.as_ref()));
                            failed.lock().unwrap_or_else(PoisonError::into_inner).note(&path, r);
                        }
                        Job::KeepFirst { vault, path, text, at } => {
                            let r =
                                run(&|| if vault.history(&path).is_empty() { vault.snapshot(&path, &text, at, true) } else { Ok(false) });
                            failed.lock().unwrap_or_else(PoisonError::into_inner).note(&path, r);
                        }
                        Job::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("spawn history writer");
        Self { tx, failure }
    }

    /// The first history failure since the last one told, once.
    pub(crate) fn take_failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(PoisonError::into_inner).untold.take()
    }

    pub(crate) fn keep(&self, vault: &Vault, path: &NotePath, text: &str, at: i64, force: bool) {
        self.keep_with(vault, path, text, at, force, None);
    }

    /// [`HistoryWriter::keep`] with provenance written next to the snapshot.
    pub(crate) fn keep_with(&self, vault: &Vault, path: &NotePath, text: &str, at: i64, force: bool, prov: Option<serde_json::Value>) {
        let _ = self.tx.send(Job::Keep { vault: vault.clone(), path: path.clone(), text: text.to_string(), at, force, prov });
    }

    pub(crate) fn keep_first(&self, vault: &Vault, path: &NotePath, text: &str, at: i64) {
        let _ = self.tx.send(Job::KeepFirst { vault: vault.clone(), path: path.clone(), text: text.to_string(), at });
    }

    /// Wait until every version asked for so far is written.
    pub(crate) fn flush(&self) {
        let (done, wait) = bounded(1);
        if self.tx.send(Job::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(10));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Added,
    Removed,
    /// Collapsed run of unchanged lines.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Tell the person, once, that version history couldn't be written.
pub(crate) fn tick(ws: &mut Workspace, _now: std::time::Instant) -> bool {
    match ws.history.take_failure() {
        Some(text) => {
            ws.notify(NoticeLevel::Warn, text);
            true
        }
        None => false,
    }
}

/// Line diff from `old` to `new` with `context` unchanged lines around changes.
pub fn diff(old: &str, new: &str, context: usize) -> Vec<DiffLine> {
    let d = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for group in d.grouped_ops(context) {
        if !out.is_empty() {
            out.push(DiffLine { kind: DiffKind::Gap, text: "⋯".into() });
        }
        for op in group {
            for change in d.iter_changes(&op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => DiffKind::Same,
                    ChangeTag::Insert => DiffKind::Added,
                    ChangeTag::Delete => DiffKind::Removed,
                };
                out.push(DiffLine { kind, text: change.value().trim_end_matches('\n').to_string() });
            }
        }
    }
    out
}

/// (lines added, lines removed)
pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    let d = TextDiff::from_lines(old, new);
    d.iter_all_changes().fold((0, 0), |(a, r), c| match c.tag() {
        ChangeTag::Insert => (a + 1, r),
        ChangeTag::Delete => (a, r + 1),
        ChangeTag::Equal => (a, r),
    })
}

impl Workspace {
    /// Saved versions of the active note, newest first.
    pub fn note_history(&self) -> Vec<Snapshot> {
        self.history.flush();
        match (self.vault(), self.active_doc().and_then(|d| d.path.clone())) {
            (Some(v), Some(p)) => v.history(&p),
            _ => Vec::new(),
        }
    }

    /// Who made a version (its `.prov.json`), when known.
    /// Wait until every version saved so far is written to history (tests,
    /// and before reading history right after a save).
    pub fn flush_history(&self) {
        self.history.flush();
    }

    pub fn snapshot_provenance(&self, s: &Snapshot) -> Option<quire_agent::Provenance> {
        quire_agent::Provenance::from_json(&self.vault()?.read_provenance(s)?)
    }

    pub fn snapshot_text(&self, s: &Snapshot) -> Option<String> {
        self.history.flush();
        self.vault()?.read_snapshot(s).ok()
    }

    /// Replace the active note with an old version (undoable, and saved as a new version).
    pub fn restore_snapshot(&mut self, s: &Snapshot) -> bool {
        let Some(text) = self.snapshot_text(s) else { return false };
        let when = chrono::DateTime::from_timestamp_millis(s.at_ms)
            .map(|d| d.with_timezone(&chrono::Local).format("%b %-d, %H:%M").to_string())
            .unwrap_or_default();
        self.restore_text(&text, &format!("the version from {when}"))
    }

    /// Replace the active note with `text` as one undoable edit and save it.
    /// `what` names it in the notice ("the version from Nov 2, 14:03").
    pub fn restore_text(&mut self, text: &str, what: &str) -> bool {
        let Some(doc) = self.active_doc_mut() else { return false };
        let id = doc.id;
        let len = doc.buffer().len_chars();
        let caret = doc.selection().head.min(text.chars().count());
        if doc.apply(Transaction::replace(0, len, text).with_selection(Selection::cursor(caret)), EditKind::Other).is_err() {
            return false;
        }
        self.save(id);
        self.notify(NoticeLevel::Info, format!("Restored {what} (Undo brings yours back)"));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use quire_vault::NotePath;

    #[test]
    fn diffs_show_changes_with_context() {
        let d = diff("a\nb\nc\nd\ne\nf\ng\n", "a\nb\nC\nd\ne\nf\ng\nh\n", 1);
        let kinds: Vec<_> = d.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert!(kinds.contains(&(DiffKind::Removed, "c")) && kinds.contains(&(DiffKind::Added, "C")));
        assert!(kinds.contains(&(DiffKind::Gap, "⋯")));
        assert_eq!(diff_stats("a\nb\n", "a\nc\nd\n"), (2, 1));
    }

    #[test]
    fn saves_create_versions_and_restore_is_undoable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("N.md"), "original").unwrap();
        let mut w = Workspace::new(Settings::default());
        w.open_vault(dir.path()).unwrap();
        w.open_note(&NotePath::new("N.md")).unwrap();
        w.insert_text("edited ");
        let id = w.active_doc().unwrap().id;
        assert!(w.save(id));
        let h = w.note_history();
        assert_eq!(h.len(), 2, "original kept alongside the first save");
        assert_eq!(w.snapshot_text(&h[1]).unwrap(), "original");
        assert!(w.restore_snapshot(&h[1].clone()));
        assert_eq!(&*w.active_doc().unwrap().text(), "original");
        assert_eq!(std::fs::read_to_string(dir.path().join("N.md")).unwrap(), "original");
        w.run(crate::Command::Undo);
        assert_eq!(&*w.active_doc().unwrap().text(), "edited original");
    }

    #[test]
    fn an_accepted_suggestion_saves_with_provenance_and_typing_after_it_keeps_it() {
        use quire_agent::{Actor, Op, Provenance, Via};
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[("N.md", "one\n")]));
        let id = w.open_note(&NotePath::new("N.md")).unwrap();
        let device = w.device().id.clone();
        // A person accepts an agent's hunk into the open note.
        w.insert_text("from the agent\n");
        let mut prov = Provenance::new(Actor::human(&device), Op::Accept, "", 0);
        prov.accepted_from = Some(Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, &device));
        w.active_doc_mut().unwrap().next_save_provenance = Some(prov);
        assert!(w.save(id));
        assert!(w.active_doc().unwrap().next_save_provenance.is_none(), "used once");
        // …and keeps typing; the saves land inside the two-minute window.
        w.insert_text("typed ");
        assert!(w.save(id));
        w.insert_text("more ");
        assert!(w.save(id));
        let h = w.note_history();
        let vault = w.vault().unwrap().clone();
        let with_prov: Vec<Provenance> =
            h.iter().filter_map(|s| vault.read_provenance(s)).filter_map(|v| Provenance::from_json(&v)).collect();
        assert_eq!(with_prov.len(), 1, "{h:?}");
        assert_eq!(with_prov[0].accepted_from.as_ref().map(|a| a.id.as_str()), Some("mcp:claude-code"));
        assert_eq!(with_prov[0].hash, quire_vault::ContentHash::of("from the agent\none\n"));
        assert_eq!(vault.read_snapshot(&h[0]).unwrap(), "from the agent\ntyped more one\n");
    }

    /// Reliability scan item 10: a version that can't be written is told
    /// once, not on every save, and told again after history worked.
    #[test]
    fn a_history_failure_is_told_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("N.md"), "original").unwrap();
        let mut w = Workspace::new(Settings::default());
        w.open_vault(dir.path()).unwrap();
        w.open_note(&NotePath::new("N.md")).unwrap();
        let id = w.active_doc().unwrap().id;
        w.history.flush();
        // History's folder can't be made: a file stands where it goes.
        let history = dir.path().join(".quire/history");
        let _ = std::fs::remove_dir_all(&history);
        std::fs::create_dir_all(dir.path().join(".quire")).unwrap();
        std::fs::write(&history, "not a folder").unwrap();
        let told = |w: &Workspace| w.notices.iter().filter(|n| n.text.contains("couldn't keep a version")).count();
        for i in 0..3 {
            w.insert_text(&format!("edit {i} "));
            assert!(w.save(id), "the note itself is saved");
            w.history.flush();
            tick(&mut w, std::time::Instant::now());
        }
        assert_eq!(told(&w), 1);
        assert!(std::fs::read_to_string(dir.path().join("N.md")).unwrap().contains("edit 2"));
        // History works again, then fails again: told again.
        std::fs::remove_file(&history).unwrap();
        w.insert_text("ok ");
        assert!(w.save(id));
        w.history.flush();
        tick(&mut w, std::time::Instant::now());
        assert_eq!(told(&w), 1);
        std::fs::remove_dir_all(&history).unwrap();
        std::fs::write(&history, "not a folder").unwrap();
        w.insert_text("again ");
        assert!(w.save(id));
        w.history.flush();
        tick(&mut w, std::time::Instant::now());
        assert_eq!(told(&w), 2);
    }
}
