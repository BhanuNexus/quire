// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::ops::Range;

/// A selection in char indices. `anchor` stays put while `head` moves.
/// When `anchor == head` the selection is a caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub const fn cursor(pos: usize) -> Self {
        Self { anchor: pos, head: pos }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Clamp both ends to `len` (useful after external reloads).
    pub fn clamp(self, len: usize) -> Self {
        Self::new(self.anchor.min(len), self.head.min(len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_ordered() {
        let s = Selection::new(9, 3);
        assert_eq!(s.range(), 3..9);
        assert!(!s.is_empty());
        assert!(Selection::cursor(4).is_empty());
    }
}
