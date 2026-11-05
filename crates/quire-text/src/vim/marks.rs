// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Marks of one note: `m{a-z}`, the position before the latest jump (`''`)
//! and the last change (`` `. ``).
//!
//! Like Vim's, a mark is a line and a column. When lines are added or
//! removed above it, it moves with its line; when its line is deleted, the
//! mark goes. Edits inside its line leave the column alone.

use std::collections::BTreeMap;

use crate::Buffer;

/// A mark: (line, column in chars), both zero based.
pub(super) type Pos = (usize, usize);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    named: BTreeMap<char, Pos>,
    /// Where the cursor was before the latest jump (`''`).
    prev: Option<Pos>,
    /// Where the last change was made (`'.`).
    change: Option<Pos>,
}

impl Marks {
    pub fn get(&self, c: char) -> Option<Pos> {
        match c {
            'a'..='z' => self.named.get(&c).copied(),
            '\'' | '`' => self.prev,
            '.' => self.change,
            _ => None,
        }
    }

    pub fn set(&mut self, c: char, at: Pos) {
        match c {
            'a'..='z' => {
                self.named.insert(c, at);
            }
            '\'' | '`' => self.prev = Some(at),
            '.' => self.change = Some(at),
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.prev.is_none() && self.change.is_none()
    }

    /// The marks to keep between sessions: `a`–`z` and `.`, as
    /// (name, line, column).
    pub fn list(&self) -> Vec<(char, usize, usize)> {
        self.named.iter().map(|(c, (l, k))| (*c, *l, *k)).chain(self.change.map(|(l, k)| ('.', l, k))).collect()
    }

    pub fn from_list(list: &[(char, usize, usize)]) -> Self {
        let mut m = Self::default();
        for (c, l, k) in list {
            m.set(*c, (*l, *k));
        }
        m
    }

    /// Follow an edit that turned `before` into `after`.
    pub fn edited(&mut self, before: &Buffer, after: &Buffer) {
        if self.is_empty() || before.rope() == after.rope() {
            return;
        }
        let span = aligned_span(before, after);
        self.edited_at(before, after, span);
    }

    /// Follow an edit that replaced `s..oe` of `before` with `s..ne` of `after`.
    pub fn edited_at(&mut self, before: &Buffer, after: &Buffer, (s, oe, ne): (usize, usize, usize)) {
        if self.is_empty() {
            return;
        }
        let l0 = before.char_to_line(s);
        let map = |at: Pos| map_pos(before, after, (s, oe, ne), at);
        self.named = std::mem::take(&mut self.named).into_iter().filter_map(|(c, at)| Some((c, map(at)?))).collect();
        self.prev = self.prev.and_then(map);
        // The last change stays near the edit even when its line went.
        self.change = self.change.map(|at| map(at).unwrap_or((l0, 0)));
    }
}

/// Where `before` and `after` differ, like [`super::diff_span`]; a pure
/// insertion or deletion is moved back to start a line when it can be
/// (deleting `two\n` from `one\ntwo\nthree` is not `wo\nt`).
fn aligned_span(before: &Buffer, after: &Buffer) -> (usize, usize, usize) {
    let (s, oe, ne) = super::diff_span(before, after);
    let (long, n) = match (oe > s, ne > s) {
        (true, false) => (before, oe - s),
        (false, true) => (after, ne - s),
        _ => return (s, oe, ne),
    };
    let at_line_start = |p: usize| p == 0 || long.char_at(p - 1) == Some('\n');
    let mut k = 0;
    while !at_line_start(s - k) && long.char_at(s - k - 1) == long.char_at(s - k - 1 + n) {
        k += 1;
    }
    if at_line_start(s - k) { (s - k, oe - k, ne - k) } else { (s, oe, ne) }
}

/// Where a mark goes after the region `s..oe` of `before` became `s..ne`
/// of `after`; `None` when its line was deleted.
fn map_pos(before: &Buffer, after: &Buffer, (s, oe, ne): (usize, usize, usize), (l, c): Pos) -> Option<Pos> {
    let l0 = before.char_to_line(s);
    let l1 = before.char_to_line(oe);
    let removed = l1 - l0;
    let added = after.char_to_line(ne) - l0;
    if l < l0 {
        return Some((l, c));
    }
    if l > l1 {
        return Some(((l + added).saturating_sub(removed), c));
    }
    if removed == added || l >= before.len_lines() {
        return Some((l, c));
    }
    let r = before.line_range(l);
    let has_break = r.end < before.len_chars();
    // The whole line went: with its line break, or (the last line) with the one before it.
    let gone = (s <= r.start && has_break && r.end < oe) || (!has_break && s < r.start && oe >= r.end);
    if gone {
        return None;
    }
    let p = if r.start < s {
        r.start
    } else if r.start >= oe {
        r.start - oe + ne
    } else {
        s
    };
    Some((after.char_to_line(p.min(after.len_chars())), c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(text: &str, edit: (usize, usize, &str), at: Pos) -> Option<Pos> {
        let before = Buffer::from(text);
        let mut after = before.clone();
        crate::Transaction::replace(edit.0, edit.1, edit.2).apply(&mut after).unwrap();
        let mut m = Marks::default();
        m.set('a', at);
        m.edited(&before, &after);
        m.get('a')
    }

    #[test]
    fn marks_follow_their_lines() {
        let t = "one\ntwo\nthree";
        assert_eq!(moved(t, (0, 0, "new\n"), (1, 2)), Some((2, 2)), "a line added above");
        assert_eq!(moved(t, (0, 4, ""), (2, 1)), Some((1, 1)), "a line removed above");
        assert_eq!(moved(t, (4, 8, ""), (1, 0)), None, "its line deleted");
        assert_eq!(moved(t, (4, 8, ""), (2, 3)), Some((1, 3)));
        assert_eq!(moved(t, (7, 13, ""), (2, 0)), None, "the last line deleted");
        assert_eq!(moved(t, (4, 4, "--"), (1, 1)), Some((1, 1)), "edits in the line keep the column");
        assert_eq!(moved(t, (5, 5, "\n"), (2, 0)), Some((3, 0)), "a line split above");
        let m = Marks::from_list(&[('a', 1, 2), ('.', 0, 0)]);
        assert_eq!(m.list(), vec![('a', 1, 2), ('.', 0, 0)]);
    }
}
