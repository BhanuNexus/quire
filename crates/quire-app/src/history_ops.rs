// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Version history operations for the active note: list, diff, restore.

use quire_text::{EditKind, Selection, Transaction};
use quire_vault::Snapshot;
use similar::{ChangeTag, TextDiff};

use crate::workspace::{NoticeLevel, Workspace};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
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
        match (self.vault(), self.active_doc().and_then(|d| d.path.clone())) {
            (Some(v), Some(p)) => v.history(&p),
            _ => Vec::new(),
        }
    }

    pub fn snapshot_text(&self, s: &Snapshot) -> Option<String> {
        self.vault()?.read_snapshot(s).ok()
    }

    /// Replace the active note with an old version (undoable, and saved as a new version).
    pub fn restore_snapshot(&mut self, s: &Snapshot) -> bool {
        let Some(text) = self.snapshot_text(s) else { return false };
        let Some(doc) = self.active_doc_mut() else { return false };
        let id = doc.id;
        let len = doc.buffer().len_chars();
        let caret = doc.selection().head.min(text.chars().count());
        if doc.apply(Transaction::replace(0, len, text).with_selection(Selection::cursor(caret)), EditKind::Other).is_err() {
            return false;
        }
        self.save(id);
        let when = chrono::DateTime::from_timestamp_millis(s.at_ms)
            .map(|d| d.with_timezone(&chrono::Local).format("%b %-d, %H:%M").to_string())
            .unwrap_or_default();
        self.notify(NoticeLevel::Info, format!("Restored the version from {when} (Undo brings yours back)"));
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
}
