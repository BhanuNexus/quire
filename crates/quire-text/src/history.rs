// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Branching undo history.
//!
//! Each node stores the forward transactions that lead to it from its parent
//! and their inverses. Undo walks to the parent; redo walks to the most
//! recently visited child, so undoing and then typing never destroys the
//! abandoned branch (it is still reachable with [`History::switch_branch`]).

use std::time::{Duration, Instant};

use crate::{Selection, Transaction};

/// What kind of edit produced a step. Consecutive steps of the same
/// coalescable kind typed in quick succession merge into one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    /// Paste, smart edits, refactors… never coalesced.
    Other,
}

#[derive(Debug, Clone)]
struct Node {
    parent: usize,
    children: Vec<usize>,
    /// Child to follow on redo.
    last_child: Option<usize>,
    forward: Vec<Transaction>,
    inverse: Vec<Transaction>,
    sel_before: Selection,
    sel_after: Selection,
    kind: EditKind,
    at: Instant,
}

/// Transactions to apply (in order) plus the selection to restore.
#[derive(Debug, Clone)]
pub struct UndoStep {
    pub transactions: Vec<Transaction>,
    pub selection: Selection,
}

#[derive(Debug, Clone)]
pub struct History {
    nodes: Vec<Node>,
    current: usize,
    coalesce_window: Duration,
}

impl Default for History {
    fn default() -> Self {
        Self::new(Duration::from_millis(500))
    }
}

impl History {
    pub fn new(coalesce_window: Duration) -> Self {
        let root = Node {
            parent: 0,
            children: Vec::new(),
            last_child: None,
            forward: Vec::new(),
            inverse: Vec::new(),
            sel_before: Selection::default(),
            sel_after: Selection::default(),
            kind: EditKind::Other,
            at: Instant::now(),
        };
        Self { nodes: vec![root], current: 0, coalesce_window }
    }

    /// Record an applied transaction and its inverse.
    pub fn commit(
        &mut self,
        forward: Transaction,
        inverse: Transaction,
        sel_before: Selection,
        sel_after: Selection,
        kind: EditKind,
        now: Instant,
    ) {
        let cur = &mut self.nodes[self.current];
        let can_merge = self.current != 0
            && cur.children.is_empty()
            && kind != EditKind::Other
            && cur.kind == kind
            && now.duration_since(cur.at) <= self.coalesce_window
            && cur.sel_after == sel_before;
        if can_merge {
            cur.forward.push(forward);
            cur.inverse.push(inverse);
            cur.sel_after = sel_after;
            cur.at = now;
            return;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            parent: self.current,
            children: Vec::new(),
            last_child: None,
            forward: vec![forward],
            inverse: vec![inverse],
            sel_before,
            sel_after,
            kind,
            at: now,
        });
        let parent = &mut self.nodes[self.current];
        parent.children.push(id);
        parent.last_child = Some(id);
        self.current = id;
    }

    /// Break coalescing so the next edit starts a fresh undo step.
    pub fn seal(&mut self) {
        if self.current != 0 {
            self.nodes[self.current].kind = EditKind::Other;
        }
    }

    pub fn can_undo(&self) -> bool {
        self.current != 0
    }

    pub fn can_redo(&self) -> bool {
        self.nodes[self.current].last_child.is_some()
    }

    pub fn undo(&mut self) -> Option<UndoStep> {
        if !self.can_undo() {
            return None;
        }
        let node = &self.nodes[self.current];
        let step = UndoStep { transactions: node.inverse.iter().rev().cloned().collect(), selection: node.sel_before };
        let (parent, me) = (node.parent, self.current);
        self.nodes[parent].last_child = Some(me);
        self.current = parent;
        Some(step)
    }

    pub fn redo(&mut self) -> Option<UndoStep> {
        let child = self.nodes[self.current].last_child?;
        let node = &self.nodes[child];
        let step = UndoStep { transactions: node.forward.clone(), selection: node.sel_after };
        self.current = child;
        Some(step)
    }

    /// Number of alternative futures from the current state.
    pub fn branches(&self) -> usize {
        self.nodes[self.current].children.len()
    }

    /// Choose which child `redo` follows (wraps around).
    pub fn switch_branch(&mut self) {
        let node = &mut self.nodes[self.current];
        if node.children.len() < 2 {
            return;
        }
        let idx = node.last_child.and_then(|c| node.children.iter().position(|&x| x == c)).unwrap_or(0);
        node.last_child = Some(node.children[(idx + 1) % node.children.len()]);
    }

    pub fn len(&self) -> usize {
        self.nodes.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;

    struct Doc {
        buf: Buffer,
        sel: Selection,
        hist: History,
    }

    impl Doc {
        fn new(s: &str) -> Self {
            Self { buf: Buffer::from(s), sel: Selection::cursor(s.chars().count()), hist: History::default() }
        }
        fn edit(&mut self, tx: Transaction, kind: EditKind, now: Instant) {
            let inv = tx.invert(&self.buf);
            tx.apply(&mut self.buf).unwrap();
            let before = self.sel;
            self.sel = tx.selection().unwrap_or_else(|| tx.map_selection(before));
            self.hist.commit(tx, inv, before, self.sel, kind, now);
        }
        fn undo(&mut self) {
            let step = self.hist.undo();
            self.run(step);
        }
        fn redo(&mut self) {
            let step = self.hist.redo();
            self.run(step);
        }
        fn run(&mut self, step: Option<UndoStep>) {
            let step = step.unwrap();
            for tx in step.transactions {
                tx.apply(&mut self.buf).unwrap();
            }
            self.sel = step.selection;
        }
    }

    #[test]
    fn coalesces_fast_typing_and_undoes_as_one() {
        let t0 = Instant::now();
        let mut d = Doc::new("");
        for (i, ch) in "hello".chars().enumerate() {
            d.edit(Transaction::insert(i, ch.to_string()), EditKind::Insert, t0 + Duration::from_millis(i as u64 * 50));
        }
        assert_eq!(d.hist.len(), 1);
        d.undo();
        assert_eq!(d.buf.to_string(), "");
        d.redo();
        assert_eq!(d.buf.to_string(), "hello");
    }

    #[test]
    fn pause_starts_new_step() {
        let t0 = Instant::now();
        let mut d = Doc::new("");
        d.edit(Transaction::insert(0, "a"), EditKind::Insert, t0);
        d.edit(Transaction::insert(1, "b"), EditKind::Insert, t0 + Duration::from_secs(2));
        assert_eq!(d.hist.len(), 2);
        d.undo();
        assert_eq!(d.buf.to_string(), "a");
    }

    #[test]
    fn branches_survive_undo_then_edit() {
        let t0 = Instant::now();
        let mut d = Doc::new("x");
        d.edit(Transaction::insert(1, "A"), EditKind::Other, t0);
        d.undo();
        d.edit(Transaction::insert(1, "B"), EditKind::Other, t0);
        assert_eq!(d.buf.to_string(), "xB");
        d.undo();
        assert_eq!(d.hist.branches(), 2);
        d.redo();
        assert_eq!(d.buf.to_string(), "xB");
        d.undo();
        d.hist.switch_branch();
        d.redo();
        assert_eq!(d.buf.to_string(), "xA");
    }
}
