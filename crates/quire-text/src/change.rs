// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use crate::{Buffer, Selection};

/// Replace the char range `start..end` (in the *original* document) with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

impl Change {
    pub fn new(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self { start, end, text: text.into() }
    }

    fn inserted_len(&self) -> usize {
        self.text.chars().count()
    }

    fn delta(&self) -> isize {
        self.inserted_len() as isize - (self.end - self.start) as isize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionError {
    Overlapping,
    OutOfBounds { end: usize, len: usize },
    Inverted,
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overlapping => write!(f, "changes overlap"),
            Self::OutOfBounds { end, len } => {
                write!(f, "change ends at {end} but document has {len} chars")
            }
            Self::Inverted => write!(f, "change start is after its end"),
        }
    }
}

impl std::error::Error for TransactionError {}

/// Lines `start_line .. start_line + old_lines` were replaced by `new_lines` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineDelta {
    pub start_line: usize,
    pub old_lines: usize,
    pub new_lines: usize,
}

/// Which side a mapped position sticks to when text is inserted exactly at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assoc {
    Before,
    After,
}

/// An atomic, invertible edit: a set of non-overlapping changes expressed in
/// the coordinates of the document *before* the edit, plus an optional
/// selection to set afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Transaction {
    changes: Vec<Change>,
    selection: Option<Selection>,
}

impl Transaction {
    /// Build from arbitrary changes. They are sorted and validated.
    pub fn new(mut changes: Vec<Change>) -> Result<Self, TransactionError> {
        changes.retain(|c| !(c.start == c.end && c.text.is_empty()));
        changes.sort_by_key(|c| (c.start, c.end));
        for c in &changes {
            if c.start > c.end {
                return Err(TransactionError::Inverted);
            }
        }
        for w in changes.windows(2) {
            if w[0].end > w[1].start {
                return Err(TransactionError::Overlapping);
            }
        }
        Ok(Self { changes, selection: None })
    }

    pub fn replace(start: usize, end: usize, text: impl Into<String>) -> Self {
        let (start, end) = (start.min(end), start.max(end));
        Self::new(vec![Change::new(start, end, text)]).expect("single change is always valid")
    }

    pub fn insert(pos: usize, text: impl Into<String>) -> Self {
        Self::replace(pos, pos, text)
    }

    pub fn delete(start: usize, end: usize) -> Self {
        Self::replace(start, end, "")
    }

    pub fn with_selection(mut self, sel: Selection) -> Self {
        self.selection = Some(sel);
        self
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Validate against a buffer without applying.
    pub fn check(&self, buf: &Buffer) -> Result<(), TransactionError> {
        let len = buf.len_chars();
        match self.changes.last() {
            Some(c) if c.end > len => Err(TransactionError::OutOfBounds { end: c.end, len }),
            _ => Ok(()),
        }
    }

    /// Apply to `buf`. Changes are applied back to front so earlier offsets stay valid.
    pub fn apply(&self, buf: &mut Buffer) -> Result<(), TransactionError> {
        self.check(buf)?;
        for c in self.changes.iter().rev() {
            buf.apply_change(c);
        }
        Ok(())
    }

    /// The transaction that undoes `self`, computed against the buffer state
    /// *before* `self` is applied.
    pub fn invert(&self, before: &Buffer) -> Transaction {
        let mut shift: isize = 0;
        let mut inverse = Vec::with_capacity(self.changes.len());
        for c in &self.changes {
            let new_start = (c.start as isize + shift) as usize;
            let new_end = new_start + c.inserted_len();
            inverse.push(Change::new(new_start, new_end, before.slice(c.start..c.end)));
            shift += c.delta();
        }
        Transaction { changes: inverse, selection: None }
    }

    /// Map a position in the old document to the new one.
    pub fn map_pos(&self, pos: usize, assoc: Assoc) -> usize {
        let mut shift: isize = 0;
        for c in &self.changes {
            if pos < c.start || (pos == c.start && c.start == c.end && assoc == Assoc::Before) {
                break;
            }
            if pos < c.end {
                // Inside a replaced range: snap to the edge of the replacement.
                let base = (c.start as isize + shift) as usize;
                return match assoc {
                    Assoc::Before => base,
                    Assoc::After => base + c.inserted_len(),
                };
            }
            shift += c.delta();
        }
        (pos as isize + shift).max(0) as usize
    }

    pub fn map_selection(&self, sel: Selection) -> Selection {
        Selection::new(self.map_pos(sel.anchor, Assoc::After), self.map_pos(sel.head, Assoc::After))
    }

    /// How this transaction changes the line structure, computed against the
    /// buffer *before* it is applied. Entries are ordered back to front so a
    /// per-line table can be spliced in sequence without index shifting.
    pub fn line_deltas(&self, before: &Buffer) -> Vec<LineDelta> {
        let mut merged: Vec<LineDelta> = Vec::new();
        for c in &self.changes {
            let l0 = before.char_to_line(c.start);
            let l1 = before.char_to_line(c.end);
            let removed_breaks = before.slice(c.start..c.end).matches('\n').count() as isize;
            let added_breaks = c.text.matches('\n').count() as isize;
            let d = added_breaks - removed_breaks;
            match merged.last_mut() {
                Some(m) if l0 < m.start_line + m.old_lines => {
                    let end = (m.start_line + m.old_lines - 1).max(l1);
                    let old = end - m.start_line + 1;
                    let delta = m.new_lines as isize - m.old_lines as isize + d;
                    m.old_lines = old;
                    m.new_lines = (old as isize + delta).max(1) as usize;
                }
                _ => merged.push(LineDelta {
                    start_line: l0,
                    old_lines: l1 - l0 + 1,
                    new_lines: ((l1 - l0 + 1) as isize + d).max(1) as usize,
                }),
            }
        }
        merged.reverse();
        merged
    }

    /// Net change in document length.
    pub fn len_delta(&self) -> isize {
        self.changes.iter().map(Change::delta).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_invert_round_trip() {
        let original = "The quick brown fox";
        let mut buf = Buffer::from(original);
        let tx = Transaction::new(vec![
            Change::new(4, 9, "slow"),     // quick -> slow
            Change::new(16, 19, "turtle"), // fox -> turtle
            Change::new(0, 0, ">> "),
        ])
        .unwrap();
        let inv = tx.invert(&buf);
        tx.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), ">> The slow brown turtle");
        inv.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), original);
    }

    #[test]
    fn rejects_overlap_and_out_of_bounds() {
        assert_eq!(Transaction::new(vec![Change::new(0, 5, "a"), Change::new(3, 6, "b")]), Err(TransactionError::Overlapping));
        let mut buf = Buffer::from("abc");
        assert!(Transaction::delete(1, 10).apply(&mut buf).is_err());
    }

    #[test]
    fn maps_positions() {
        let tx = Transaction::new(vec![Change::new(2, 4, "XYZ")]).unwrap();
        assert_eq!(tx.map_pos(0, Assoc::After), 0);
        assert_eq!(tx.map_pos(3, Assoc::After), 5);
        assert_eq!(tx.map_pos(3, Assoc::Before), 2);
        assert_eq!(tx.map_pos(6, Assoc::After), 7);
        let ins = Transaction::insert(2, "ab");
        assert_eq!(ins.map_pos(2, Assoc::Before), 2);
        assert_eq!(ins.map_pos(2, Assoc::After), 4);
    }

    #[test]
    fn line_deltas_describe_structure_changes() {
        let b = Buffer::from("a\nb\nc\nd");
        let tx = Transaction::new(vec![Change::new(0, 0, "x\ny\n"), Change::new(4, 7, "")]).unwrap();
        // "b\nc" (lines 2..3 → 1 line) and an insert of two breaks on line 0.
        assert_eq!(
            tx.line_deltas(&b),
            vec![LineDelta { start_line: 2, old_lines: 2, new_lines: 1 }, LineDelta { start_line: 0, old_lines: 1, new_lines: 3 }]
        );
        let same_line = Transaction::new(vec![Change::new(0, 0, "\n"), Change::new(1, 1, "\n")]).unwrap();
        assert_eq!(same_line.line_deltas(&Buffer::from("ab")), vec![LineDelta { start_line: 0, old_lines: 1, new_lines: 3 }]);
    }

    #[test]
    fn unicode_inverse() {
        let mut buf = Buffer::from("नमस्ते 🌍 world");
        let tx = Transaction::replace(7, 8, "🌏🌏");
        let inv = tx.invert(&buf);
        tx.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "नमस्ते 🌏🌏 world");
        inv.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "नमस्ते 🌍 world");
    }
}
