// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::ops::Range;

use ropey::Rope;

use crate::change::Change;

/// The document text. A thin, intention-revealing wrapper over [`ropey::Rope`]:
/// O(log n) edits, cheap clones via structural sharing, and fast line lookups.
#[derive(Debug, Clone, Default)]
pub struct Buffer {
    rope: Rope,
}

impl Buffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    /// Text in a char range as an owned `String`.
    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.slice(range).to_string()
    }

    pub fn char_at(&self, idx: usize) -> Option<char> {
        (idx < self.len_chars()).then(|| self.rope.char(idx))
    }

    pub fn char_to_line(&self, char_idx: usize) -> usize {
        self.rope.char_to_line(char_idx.min(self.len_chars()))
    }

    pub fn line_to_char(&self, line: usize) -> usize {
        self.rope.line_to_char(line.min(self.len_lines()))
    }

    pub fn char_to_byte(&self, char_idx: usize) -> usize {
        self.rope.char_to_byte(char_idx.min(self.len_chars()))
    }

    pub fn byte_to_char(&self, byte_idx: usize) -> usize {
        self.rope.byte_to_char(byte_idx.min(self.len_bytes()))
    }

    /// Char range of `line`, excluding its line terminator.
    pub fn line_range(&self, line: usize) -> Range<usize> {
        let start = self.line_to_char(line);
        let slice = self.rope.line(line.min(self.len_lines().saturating_sub(1)));
        let mut len = slice.len_chars();
        // Trim "\n" or "\r\n".
        if len > 0 && slice.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && slice.char(len - 1) == '\r' {
                len -= 1;
            }
        }
        start..start + len
    }

    /// Line text without its terminator.
    pub fn line(&self, line: usize) -> String {
        self.slice(self.line_range(line))
    }

    /// (line, column) for a char index, both zero based. Column is in chars.
    pub fn line_col(&self, char_idx: usize) -> (usize, usize) {
        let line = self.char_to_line(char_idx);
        (line, char_idx.min(self.len_chars()) - self.line_to_char(line))
    }

    pub(crate) fn apply_change(&mut self, change: &Change) {
        if change.start < change.end {
            self.rope.remove(change.start..change.end);
        }
        if !change.text.is_empty() {
            self.rope.insert(change.start, &change.text);
        }
    }
}

impl From<&str> for Buffer {
    fn from(s: &str) -> Self {
        Self { rope: Rope::from_str(s) }
    }
}

impl From<String> for Buffer {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl std::fmt::Display for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in self.rope.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_helpers_handle_crlf_and_unicode() {
        let b = Buffer::from("héllo\r\nwörld\nlast");
        assert_eq!(b.len_lines(), 3);
        assert_eq!(b.line(0), "héllo");
        assert_eq!(b.line(1), "wörld");
        assert_eq!(b.line(2), "last");
        assert_eq!(b.line_col(8), (1, 1));
    }
}
