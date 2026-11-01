// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! An open document: buffer + history + selection + memoized derived data.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use quire_markdown::{Extracted, Heading, StyleRun};
use quire_text::{Buffer, EditKind, History, LineDelta, Selection, Transaction, TransactionError};
use quire_vault::NotePath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub u64);

/// Derived data, valid for exactly one revision.
#[derive(Default)]
struct Derived {
    rev: u64,
    text: Option<Rc<str>>,
    runs: Option<Rc<Vec<StyleRun>>>,
    extracted: Option<Rc<Extracted>>,
}

pub struct Document {
    pub id: DocId,
    /// `None` for an unsaved scratch document.
    pub path: Option<NotePath>,
    buffer: Buffer,
    history: History,
    selection: Selection,
    revision: u64,
    saved_revision: u64,
    last_edit: Option<Instant>,
    /// Content as last read from / written to disk, to recognise our own echoes.
    disk_text: Option<Rc<str>>,
    /// Plain mode: no highlighting or derived data, for very large files.
    pub large: bool,
    /// An external change arrived while we had unsaved edits.
    pub conflict: bool,
    /// Created as "Untitled": the file takes its name from the first line
    /// until the user renames it themselves.
    pub auto_name: bool,
    derived: RefCell<Derived>,
    /// (revision after the edit, line deltas) for views that keep per-line state.
    line_log: Vec<(u64, Vec<LineDelta>)>,
}

const LINE_LOG_CAP: usize = 256;

impl Document {
    pub fn new(id: DocId, path: Option<NotePath>, text: &str, large_threshold_bytes: usize) -> Self {
        Self {
            id,
            path,
            buffer: Buffer::from(text),
            history: History::default(),
            selection: Selection::cursor(0),
            revision: 0,
            saved_revision: 0,
            last_edit: None,
            disk_text: Some(Rc::from(text)),
            large: text.len() > large_threshold_bytes,
            conflict: false,
            auto_name: false,
            derived: RefCell::default(),
            line_log: Vec::new(),
        }
    }

    /// Line-structure changes after revision `since`, oldest first. `None`
    /// when the log no longer reaches back that far (rebuild from scratch).
    pub fn line_deltas_since(&self, since: u64) -> Option<Vec<LineDelta>> {
        if since == self.revision {
            return Some(Vec::new());
        }
        let first = self.line_log.iter().position(|(rev, _)| *rev > since)?;
        if self.line_log[first].0 != since + 1 {
            return None;
        }
        Some(self.line_log[first..].iter().flat_map(|(_, d)| d.iter().copied()).collect())
    }

    fn log_lines(&mut self, deltas: Vec<LineDelta>) {
        self.line_log.push((self.revision, deltas));
        if self.line_log.len() > LINE_LOG_CAP {
            self.line_log.drain(..self.line_log.len() - LINE_LOG_CAP);
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn set_selection(&mut self, sel: Selection) {
        if sel != self.selection {
            // Moving the caret ends a typing burst.
            self.history.seal();
        }
        self.selection = sel.clamp(self.buffer.len_chars());
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn last_edit(&self) -> Option<Instant> {
        self.last_edit
    }

    pub fn title(&self) -> String {
        match &self.path {
            Some(p) => p.stem().to_string(),
            None => "Scratchpad".into(),
        }
    }

    // ------------------------------------------------------------- editing

    pub fn apply(&mut self, tx: Transaction, kind: EditKind) -> Result<(), TransactionError> {
        if tx.is_empty() {
            if let Some(sel) = tx.selection() {
                self.set_selection(sel);
            }
            return Ok(());
        }
        tx.check(&self.buffer)?;
        let inverse = tx.invert(&self.buffer);
        let deltas = tx.line_deltas(&self.buffer);
        let before = self.selection;
        tx.apply(&mut self.buffer)?;
        let after = tx.selection().unwrap_or_else(|| tx.map_selection(before)).clamp(self.buffer.len_chars());
        let now = Instant::now();
        self.history.commit(tx, inverse, before, after, kind, now);
        self.selection = after;
        self.bump(now);
        self.log_lines(deltas);
        Ok(())
    }

    fn bump(&mut self, now: Instant) {
        self.revision += 1;
        self.last_edit = Some(now);
    }

    pub fn undo(&mut self) -> bool {
        self.replay(|h| h.undo())
    }

    pub fn redo(&mut self) -> bool {
        self.replay(|h| h.redo())
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    pub fn undo_branches(&self) -> usize {
        self.history.branches()
    }

    pub fn switch_undo_branch(&mut self) {
        self.history.switch_branch();
    }

    fn replay(&mut self, f: impl FnOnce(&mut History) -> Option<quire_text::UndoStep>) -> bool {
        let Some(step) = f(&mut self.history) else {
            return false;
        };
        let mut deltas = Vec::new();
        for tx in &step.transactions {
            deltas.extend(tx.line_deltas(&self.buffer));
            // History only holds transactions it produced against this buffer.
            let _ = tx.apply(&mut self.buffer);
        }
        self.selection = step.selection.clamp(self.buffer.len_chars());
        self.bump(Instant::now());
        self.log_lines(deltas);
        true
    }

    /// Adapter for widgets that edit a `String` directly (egui's TextEdit):
    /// diff against the buffer and apply the minimal change.
    pub fn apply_widget_text(&mut self, new_text: &str, new_sel: Selection) {
        let old = self.text();
        if *old == *new_text {
            self.set_selection(new_sel);
            return;
        }
        let (start, old_end, new_end) = minimal_diff(&old, new_text);
        let inserted: String = new_text.chars().skip(start).take(new_end - start).collect();
        // Only edits at the caret coalesce into a typing burst.
        let caret = self.selection.head;
        let kind = match (old_end - start, new_end - start) {
            (0, n) if n <= 2 && start == caret && !inserted.contains('\n') => EditKind::Insert,
            (d, 0) if d <= 2 && (old_end == caret || start == caret) => EditKind::Delete,
            _ => EditKind::Other,
        };
        let tx = Transaction::replace(start, old_end, inserted).with_selection(new_sel);
        let _ = self.apply(tx, kind);
    }

    /// Replace everything (external reload) as one undoable step.
    pub fn reload(&mut self, text: &str) {
        let len = self.buffer.len_chars();
        let tx = Transaction::replace(0, len, text).with_selection(self.selection.clamp(text.chars().count()));
        let _ = self.apply(tx, EditKind::Other);
        self.mark_saved(text);
    }

    pub fn mark_saved(&mut self, disk_text: &str) {
        self.saved_revision = self.revision;
        self.disk_text = Some(Rc::from(disk_text));
        self.conflict = false;
    }

    /// Content as last read from or written to disk.
    pub fn disk_text(&self) -> Option<Rc<str>> {
        self.disk_text.clone()
    }

    /// Whether `content` is what we last read or wrote (an echo of our own save).
    pub fn matches_disk(&self, content: &str) -> bool {
        self.disk_text.as_deref() == Some(content)
    }

    // ------------------------------------------------------------- derived data

    fn derived(&self) -> std::cell::RefMut<'_, Derived> {
        let mut d = self.derived.borrow_mut();
        if d.rev != self.revision || d.text.is_none() {
            *d = Derived { rev: self.revision, text: Some(Rc::from(self.buffer.to_string())), ..Default::default() };
        }
        d
    }

    /// Full text, materialised once per revision.
    pub fn text(&self) -> Rc<str> {
        self.derived().text.clone().expect("text is always set")
    }

    /// Style runs for highlighting (empty in plain mode).
    pub fn style_runs(&self) -> Rc<Vec<StyleRun>> {
        let mut d = self.derived();
        if d.runs.is_none() {
            let runs = if self.large { Vec::new() } else { quire_markdown::highlight(d.text.as_deref().unwrap_or("")) };
            d.runs = Some(Rc::new(runs));
        }
        d.runs.clone().expect("just set")
    }

    pub fn extracted(&self) -> Rc<Extracted> {
        let mut d = self.derived();
        if d.extracted.is_none() {
            let ex = if self.large { Extracted::default() } else { quire_markdown::extract(d.text.as_deref().unwrap_or("")) };
            d.extracted = Some(Rc::new(ex));
        }
        d.extracted.clone().expect("just set")
    }

    pub fn outline(&self) -> Vec<Heading> {
        self.extracted().headings.clone()
    }

    /// (line, column) of the caret, 1-based for display.
    pub fn caret_line_col(&self) -> (usize, usize) {
        let (l, c) = self.buffer.line_col(self.selection.head);
        (l + 1, c + 1)
    }

    /// Byte offset → char offset helper for jumping to headings/search hits.
    pub fn byte_to_char(&self, byte: usize) -> usize {
        self.buffer.byte_to_char(byte)
    }
}

/// Common prefix/suffix diff in chars → (start, old_end, new_end).
fn minimal_diff(old: &str, new: &str) -> (usize, usize, usize) {
    let prefix_bytes = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    // Back up to a char boundary in both strings.
    let mut p = prefix_bytes;
    while !old.is_char_boundary(p) || !new.is_char_boundary(p) {
        p -= 1;
    }
    let max_suffix = old.len().min(new.len()) - p;
    let mut s = old.bytes().rev().zip(new.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - s) || !new.is_char_boundary(new.len() - s) {
        s -= 1;
    }
    let start = old[..p].chars().count();
    let old_end = start + old[p..old.len() - s].chars().count();
    let new_end = start + new[p..new.len() - s].chars().count();
    (start, old_end, new_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_edits_become_minimal_transactions() {
        let mut d = Document::new(DocId(1), None, "hello wörld", 1 << 20);
        d.apply_widget_text("hello, wörld", Selection::cursor(6));
        assert_eq!(&*d.text(), "hello, wörld");
        d.apply_widget_text("hello, wörld!", Selection::cursor(13));
        assert!(d.is_dirty());
        assert!(d.undo());
        assert_eq!(&*d.text(), "hello, wörld");
        assert!(d.undo());
        assert_eq!(&*d.text(), "hello wörld");
        assert!(d.redo());
        assert_eq!(&*d.text(), "hello, wörld");
    }

    #[test]
    fn diff_handles_repeats_and_unicode() {
        assert_eq!(minimal_diff("aaa", "aaaa"), (3, 3, 4));
        assert_eq!(minimal_diff("नमस्ते", "नमते"), (2, 4, 2));
        assert_eq!(minimal_diff("abc", "abc"), (3, 3, 3));
    }

    #[test]
    fn caches_by_revision() {
        let mut d = Document::new(DocId(1), None, "# Title\n", 1 << 20);
        let a = d.style_runs();
        let b = d.style_runs();
        assert!(Rc::ptr_eq(&a, &b));
        d.apply(Transaction::insert(8, "more"), EditKind::Insert).unwrap();
        assert!(!Rc::ptr_eq(&a, &d.style_runs()));
        assert_eq!(d.outline()[0].text, "Title");
    }

    #[test]
    fn line_log_covers_edits_and_undo() {
        let mut d = Document::new(DocId(1), None, "a\nb", 1 << 20);
        let r0 = d.revision();
        d.apply(Transaction::insert(1, "\nx"), EditKind::Other).unwrap();
        d.undo();
        let deltas = d.line_deltas_since(r0).unwrap();
        assert_eq!(deltas.len(), 2);
        assert_eq!((deltas[0].old_lines, deltas[0].new_lines), (1, 2));
        assert_eq!((deltas[1].old_lines, deltas[1].new_lines), (2, 1));
        assert_eq!(d.line_deltas_since(d.revision()), Some(vec![]));
    }

    #[test]
    fn large_files_skip_derived_work() {
        let d = Document::new(DocId(1), None, "# big", 2);
        assert!(d.large && d.style_runs().is_empty());
    }
}
