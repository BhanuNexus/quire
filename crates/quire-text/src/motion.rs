// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Cursor motions and text-object ranges. Pure functions over a [`Buffer`];
//! all positions are char indices.

use std::ops::Range;

use crate::Buffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Punct,
    Newline,
}

fn class(c: char) -> Class {
    if c == '\n' {
        Class::Newline
    } else if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Start of the word before `pos` (Ctrl+Left).
pub fn prev_word_start(buf: &Buffer, pos: usize) -> usize {
    let mut i = pos.min(buf.len_chars());
    // Skip spaces (but stop at a line break so Ctrl+Left doesn't fly across lines).
    while i > 0 && buf.char_at(i - 1).map(class) == Some(Class::Space) {
        i -= 1;
    }
    if i > 0 && buf.char_at(i - 1) == Some('\n') {
        return if i == pos { i - 1 } else { i };
    }
    let Some(cls) = i.checked_sub(1).and_then(|j| buf.char_at(j)).map(class) else { return i };
    while i > 0 && buf.char_at(i - 1).map(class) == Some(cls) {
        i -= 1;
    }
    i
}

/// End of the word after `pos` (Ctrl+Right).
pub fn next_word_end(buf: &Buffer, pos: usize) -> usize {
    let len = buf.len_chars();
    let mut i = pos.min(len);
    while i < len && buf.char_at(i).map(class) == Some(Class::Space) {
        i += 1;
    }
    if i < len && buf.char_at(i) == Some('\n') {
        return if i == pos { i + 1 } else { i };
    }
    let Some(cls) = buf.char_at(i).map(class) else { return i };
    while i < len && buf.char_at(i).map(class) == Some(cls) {
        i += 1;
    }
    i
}

/// The word (or run of punctuation/space) under `pos`, for double-click.
pub fn word_at(buf: &Buffer, pos: usize) -> Range<usize> {
    let len = buf.len_chars();
    let pick = |i: usize| buf.char_at(i).map(class).filter(|c| *c != Class::Newline);
    let (anchor, cls) = match pick(pos) {
        Some(Class::Word) => (pos, Class::Word),
        _ => match pos.checked_sub(1).and_then(|p| pick(p).map(|c| (p, c))) {
            Some((p, Class::Word)) => (p, Class::Word),
            _ => match pick(pos) {
                Some(c) => (pos, c),
                None => return pos..pos,
            },
        },
    };
    let mut s = anchor;
    while s > 0 && pick(s - 1) == Some(cls) {
        s -= 1;
    }
    let mut e = anchor + 1;
    while e < len && pick(e) == Some(cls) {
        e += 1;
    }
    s..e
}

pub fn line_start(buf: &Buffer, pos: usize) -> usize {
    buf.line_to_char(buf.char_to_line(pos))
}

/// End of the logical line, before its line break.
pub fn line_end(buf: &Buffer, pos: usize) -> usize {
    buf.line_range(buf.char_to_line(pos)).end
}

/// Home key: first non-blank character, or the line start if already there.
pub fn smart_home(buf: &Buffer, pos: usize) -> usize {
    let start = line_start(buf, pos);
    let end = line_end(buf, pos);
    let mut first = start;
    while first < end && buf.char_at(first).is_some_and(|c| c == ' ' || c == '\t') {
        first += 1;
    }
    if pos == first { start } else { first }
}

/// Whole line including its line break, for triple-click.
pub fn line_with_break(buf: &Buffer, pos: usize) -> Range<usize> {
    let line = buf.char_to_line(pos);
    let start = buf.line_to_char(line);
    let end = if line + 1 < buf.len_lines() { buf.line_to_char(line + 1) } else { buf.len_chars() };
    start..end
}

/// Next exact occurrence of `needle` at or after `from`, wrapping around.
pub fn find_next(buf: &Buffer, needle: &str, from: usize) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let text = buf.to_string();
    let from_b = buf.char_to_byte(from);
    let hit = text[from_b..].find(needle).map(|i| from_b + i).or_else(|| text[..from_b].find(needle))?;
    let s = buf.byte_to_char(hit);
    Some(s..s + needle.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_motions() {
        let b = Buffer::from("hello, big_world  next\nline two");
        assert_eq!(next_word_end(&b, 0), 5);
        assert_eq!(next_word_end(&b, 5), 6);
        assert_eq!(next_word_end(&b, 6), 16);
        assert_eq!(prev_word_start(&b, 16), 7);
        assert_eq!(prev_word_start(&b, 7), 5);
        // Line breaks stop motion.
        assert_eq!(next_word_end(&b, 22), 23);
        assert_eq!(prev_word_start(&b, 23), 22);
    }

    #[test]
    fn objects_and_lines() {
        let b = Buffer::from("  indented word\nnext");
        assert_eq!(word_at(&b, 4), 2..10);
        assert_eq!(word_at(&b, 10), 2..10);
        assert_eq!(smart_home(&b, 8), 2);
        assert_eq!(smart_home(&b, 2), 0);
        assert_eq!(line_end(&b, 3), 15);
        assert_eq!(line_with_break(&b, 3), 0..16);
        assert_eq!(line_with_break(&b, 17), 16..20);
        assert_eq!(find_next(&b, "e", 13), Some(17..18));
        assert_eq!(find_next(&b, "ind", 10), Some(2..5));
    }
}
