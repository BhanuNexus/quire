// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Vim motions and text objects: pure functions over a [`Buffer`].
//!
//! Positions are char indices. A line's "end" is the index of its line
//! break (or the buffer's length on the last line), which plays the part of
//! Vim's NUL: the word motions treat it as a blank, and an operator can
//! reach it while the cursor in Normal mode never rests on it.

use std::ops::Range;

use crate::Buffer;

/// Character class for word motions: 0 blank (including line breaks), 1
/// punctuation, 2 keyword characters. WORD motions (`big`) see every
/// non-blank as one class.
fn class_of(c: Option<char>, big: bool) -> u8 {
    match c {
        None => 0,
        Some(c) if c.is_whitespace() => 0,
        Some(_) if big => 2,
        Some(c) if c.is_alphanumeric() || c == '_' => 2,
        Some(_) => 1,
    }
}

pub(super) fn cls(buf: &Buffer, pos: usize, big: bool) -> u8 {
    class_of(buf.char_at(pos), big)
}

pub(super) fn line_of(buf: &Buffer, pos: usize) -> usize {
    buf.char_to_line(pos)
}

pub(super) fn last_line(buf: &Buffer) -> usize {
    buf.len_lines().saturating_sub(1)
}

pub(super) fn line_start(buf: &Buffer, line: usize) -> usize {
    buf.line_to_char(line.min(last_line(buf)))
}

/// The line's end: its line break, or the end of the buffer.
pub(super) fn line_end(buf: &Buffer, line: usize) -> usize {
    buf.line_range(line.min(last_line(buf))).end
}

/// Where the Normal-mode cursor may go at most on `line`: its last char.
pub(super) fn line_last(buf: &Buffer, line: usize) -> usize {
    let (s, e) = (line_start(buf, line), line_end(buf, line));
    if e > s { e - 1 } else { s }
}

pub(super) fn line_is_empty(buf: &Buffer, line: usize) -> bool {
    line_start(buf, line) == line_end(buf, line)
}

pub(super) fn first_non_blank(buf: &Buffer, line: usize) -> usize {
    let (mut p, e) = (line_start(buf, line), line_end(buf, line));
    while p < e && buf.char_at(p).is_some_and(|c| c == ' ' || c == '\t') {
        p += 1;
    }
    p
}

/// Start of the line after `line`, or the end of the buffer.
pub(super) fn next_line_start(buf: &Buffer, line: usize) -> usize {
    if line < last_line(buf) { buf.line_to_char(line + 1) } else { buf.len_chars() }
}

/// Column (in chars) of `pos` within its line.
pub(super) fn col_of(buf: &Buffer, pos: usize) -> usize {
    pos - line_start(buf, line_of(buf, pos))
}

/// `col` on `line`, clamped to the line's last char (or its end with `past_end`).
pub(super) fn at_col(buf: &Buffer, line: usize, col: usize, past_end: bool) -> usize {
    let s = line_start(buf, line);
    let max = if past_end { line_end(buf, line) } else { line_last(buf, line) };
    (s + col).min(max)
}

fn at_empty_line(buf: &Buffer, pos: usize) -> bool {
    let l = line_of(buf, pos);
    pos == line_start(buf, l) && line_is_empty(buf, l)
}

/// One step forward. The second value is 1 when a line break was crossed
/// and 2 when the step landed on a line's end (Vim's NUL).
fn inc(buf: &Buffer, pos: usize) -> Option<(usize, u8)> {
    let len = buf.len_chars();
    if pos >= len {
        return None;
    }
    let np = pos + 1;
    let r = if buf.char_at(pos) == Some('\n') {
        1
    } else if np >= len || buf.char_at(np) == Some('\n') {
        2
    } else {
        0
    };
    Some((np, r))
}

/// One step back. The second value is 1 when a line break was crossed.
fn dec(buf: &Buffer, pos: usize) -> Option<(usize, u8)> {
    let p = pos.checked_sub(1)?;
    Some((p, u8::from(buf.char_at(p) == Some('\n'))))
}

/// `w` / `W`. With `eol` (an operator is waiting), the last word of a line
/// ends at the line's end instead of the next line's first word.
pub(super) fn word_fwd(buf: &Buffer, mut pos: usize, count: usize, big: bool, eol: bool) -> usize {
    for n in (0..count).rev() {
        let sclass = cls(buf, pos, big);
        let Some((np, r)) = inc(buf, pos) else { return pos };
        pos = np;
        if r >= 1 && eol && n == 0 {
            return pos;
        }
        if sclass != 0 {
            while cls(buf, pos, big) == sclass {
                let Some((np, r)) = inc(buf, pos) else { return pos };
                pos = np;
                if r >= 1 && eol && n == 0 {
                    return pos;
                }
            }
        }
        while cls(buf, pos, big) == 0 {
            if at_empty_line(buf, pos) && pos < buf.len_chars() {
                break;
            }
            let Some((np, r)) = inc(buf, pos) else { return pos };
            pos = np;
            if r >= 1 && eol && n == 0 {
                return pos;
            }
        }
    }
    pos
}

/// `b` / `B`.
pub(super) fn word_back(buf: &Buffer, mut pos: usize, count: usize, big: bool) -> usize {
    for _ in 0..count {
        let Some((np, _)) = dec(buf, pos) else { return pos };
        pos = np;
        let mut finished = false;
        while cls(buf, pos, big) == 0 {
            if at_empty_line(buf, pos) {
                finished = true;
                break;
            }
            let Some((np, _)) = dec(buf, pos) else { return pos };
            pos = np;
        }
        if finished {
            continue;
        }
        let c = cls(buf, pos, big);
        loop {
            match dec(buf, pos) {
                Some((np, _)) if cls(buf, np, big) == c => pos = np,
                _ => break,
            }
        }
    }
    pos
}

/// `e` / `E`. With `stop`, a cursor already at the end of a word stays
/// there (`cw` on the last letter of a word changes just that letter).
pub(super) fn word_end(buf: &Buffer, mut pos: usize, count: usize, big: bool, mut stop: bool) -> usize {
    let len = buf.len_chars();
    for _ in 0..count {
        let sclass = cls(buf, pos, big);
        let Some((np, _)) = inc(buf, pos) else { return pos };
        pos = np;
        if sclass == cls(buf, pos, big) && sclass != 0 {
            while pos < len && cls(buf, pos, big) == sclass {
                pos += 1;
            }
        } else if !stop || sclass == 0 {
            while cls(buf, pos, big) == 0 {
                if pos >= len {
                    return len.saturating_sub(1);
                }
                pos += 1;
            }
            let c = cls(buf, pos, big);
            while pos < len && cls(buf, pos, big) == c {
                pos += 1;
            }
        }
        pos = pos.saturating_sub(1);
        stop = false;
    }
    pos
}

/// `ge` / `gE`.
pub(super) fn word_end_back(buf: &Buffer, mut pos: usize, count: usize, big: bool) -> usize {
    for _ in 0..count {
        let sclass = cls(buf, pos, big);
        let Some((np, _)) = dec(buf, pos) else { return pos };
        pos = np;
        if sclass != 0 {
            while cls(buf, pos, big) == sclass {
                let Some((np, _)) = dec(buf, pos) else { return pos };
                pos = np;
            }
        }
        while cls(buf, pos, big) == 0 {
            if at_empty_line(buf, pos) {
                break;
            }
            let Some((np, _)) = dec(buf, pos) else { return pos };
            pos = np;
        }
    }
    pos
}

/// `}` / `{`: the next (or previous) empty line. The flag says the motion
/// ran into the end of the buffer (then it is inclusive for operators).
pub(super) fn paragraph(buf: &Buffer, pos: usize, count: usize, forward: bool) -> Option<(usize, bool)> {
    let last = last_line(buf);
    let mut curr = line_of(buf, pos);
    for n in (0..count).rev() {
        let mut did_skip = false;
        let mut first = true;
        loop {
            if !line_is_empty(buf, curr) {
                did_skip = true;
            }
            if !first && did_skip && line_is_empty(buf, curr) {
                break;
            }
            first = false;
            let next = if forward { (curr < last).then_some(curr + 1) } else { curr.checked_sub(1) };
            match next {
                Some(l) => curr = l,
                None if n > 0 => return None,
                None => break,
            }
        }
    }
    if curr == last && forward {
        let e = line_end(buf, curr);
        let s = line_start(buf, curr);
        return Some(if e > s { (e - 1, true) } else { (s, false) });
    }
    Some((line_start(buf, curr), false))
}

fn pair_of(c: char) -> Option<(char, char, bool)> {
    Some(match c {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    })
}

/// `%`: the bracket matching the first bracket at or after `pos` on its line.
pub(super) fn match_pair(buf: &Buffer, pos: usize) -> Option<usize> {
    let end = line_end(buf, line_of(buf, pos));
    let mut p = pos;
    let (open, close, forward) = loop {
        if p >= end {
            return None;
        }
        if let Some(t) = buf.char_at(p).and_then(pair_of) {
            break t;
        }
        p += 1;
    };
    let mut depth = 0usize;
    if forward {
        for (i, c) in buf.rope().chars_at(p).enumerate() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(p + i);
                }
            }
        }
    } else {
        let mut i = p + 1;
        while i > 0 {
            i -= 1;
            let c = buf.char_at(i)?;
            if c == close {
                depth += 1;
            } else if c == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// How `f`, `F`, `t` and `T` search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindKind {
    /// `f`: onto the next `c`.
    Forward,
    /// `F`: onto the previous `c`.
    Backward,
    /// `t`: just before the next `c`.
    Till,
    /// `T`: just after the previous `c`.
    TillBack,
}

impl FindKind {
    pub(super) const fn reversed(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
            Self::Till => Self::TillBack,
            Self::TillBack => Self::Till,
        }
    }

    pub(super) const fn forward(self) -> bool {
        matches!(self, Self::Forward | Self::Till)
    }
}

/// `f` and friends on the cursor's line. `repeat` (from `;` or `,`) makes
/// `t`/`T` skip a match right next to the cursor, as Vim does.
pub(super) fn find_char(buf: &Buffer, pos: usize, count: usize, kind: FindKind, ch: char, repeat: bool) -> Option<usize> {
    let line = line_of(buf, pos);
    let (s, e) = (line_start(buf, line), line_end(buf, line));
    let mut p = pos;
    let skip_adjacent = repeat && matches!(kind, FindKind::Till | FindKind::TillBack);
    if kind.forward() {
        if skip_adjacent && buf.char_at(p + 1) == Some(ch) {
            p += 1;
        }
        for _ in 0..count {
            p = (p + 1..e).find(|&i| buf.char_at(i) == Some(ch))?;
        }
        Some(if kind == FindKind::Till { p - 1 } else { p })
    } else {
        if skip_adjacent && p > s && buf.char_at(p - 1) == Some(ch) {
            p -= 1;
        }
        for _ in 0..count {
            p = (s..p).rev().find(|&i| buf.char_at(i) == Some(ch))?;
        }
        Some(if kind == FindKind::TillBack { p + 1 } else { p })
    }
}

// ------------------------------------------------------------ text objects

/// A text object (`iw`, `a(`, `ip`…): its range, and its lines (first and
/// last) when it is linewise.
pub(super) struct Object {
    pub range: Range<usize>,
    pub lines: Option<(usize, usize)>,
}

/// `iw`/`aw` (`iW`/`aW` with `big`) at `pos`, `count` words.
pub(super) fn word_object(buf: &Buffer, pos: usize, count: usize, big: bool, around: bool) -> Option<Object> {
    let len = buf.len_chars();
    let line = line_of(buf, pos);
    let (ls, le) = (line_start(buf, line), line_end(buf, line));
    if ls == le {
        return None;
    }
    let pos = pos.min(le.saturating_sub(1)).max(ls);
    // A run of one class on the line, from `p`.
    let run = |p: usize| -> Range<usize> {
        let c = cls(buf, p, big);
        let mut a = p;
        while a > ls && cls(buf, a - 1, big) == c {
            a -= 1;
        }
        let mut b = p + 1;
        while b < le && cls(buf, b, big) == c {
            b += 1;
        }
        a..b
    };
    let first = run(pos);
    let on_blank = cls(buf, pos, big) == 0;
    let mut start = first.start;
    let mut end = first.end;
    let mut took_trailing_space = false;
    // `count` objects: words and blank runs alternate (blanks count in `iw`).
    let mut left = count;
    if around && !on_blank {
        // The word plus the blanks after it (or before it when there are none).
        if end < le && cls(buf, end, big) == 0 {
            end = run(end).end;
            took_trailing_space = true;
        }
        left -= 1;
        while left > 0 && end < le {
            end = run(end).end;
            if end < le && cls(buf, end, big) == 0 {
                end = run(end).end;
            }
            left -= 1;
        }
        if !took_trailing_space {
            while start > ls && cls(buf, start - 1, big) == 0 && first.start > ls {
                start -= 1;
            }
            // Keep leading indentation.
            if (ls..start).all(|i| cls(buf, i, big) == 0) {
                start = first.start;
            }
        }
    } else if around && on_blank {
        // Blanks plus the following word.
        left -= 1;
        if end < le {
            end = run(end).end;
        }
        while left > 0 && end < le {
            end = run(end).end;
            if end < le {
                end = run(end).end;
            }
            left -= 1;
        }
    } else {
        left -= 1;
        while left > 0 && end < le {
            end = run(end).end;
            left -= 1;
        }
    }
    Some(Object { range: start..end.min(len), lines: None })
}

/// `i"`/`a"` (and `'`, `` ` ``) on the cursor's line.
pub(super) fn quote_object(buf: &Buffer, pos: usize, q: char, around: bool) -> Option<Object> {
    let line = line_of(buf, pos);
    let (ls, le) = (line_start(buf, line), line_end(buf, line));
    let quotes: Vec<usize> = (ls..le).filter(|&i| buf.char_at(i) == Some(q) && (i == ls || buf.char_at(i - 1) != Some('\\'))).collect();
    // The pair around the cursor, or the first pair after it.
    let mut pair = None;
    for w in quotes.chunks(2) {
        if let [a, b] = *w {
            if pos >= a && pos <= b {
                pair = Some((a, b));
                break;
            }
            if a > pos && pair.is_none() {
                pair = Some((a, b));
                break;
            }
        }
    }
    let (a, b) = pair?;
    if !around {
        return Some(Object { range: a + 1..b, lines: None });
    }
    let mut end = b + 1;
    while end < le && buf.char_at(end).is_some_and(|c| c == ' ' || c == '\t') {
        end += 1;
    }
    let mut start = a;
    if end == b + 1 {
        while start > ls && buf.char_at(start - 1).is_some_and(|c| c == ' ' || c == '\t') {
            start -= 1;
        }
    }
    Some(Object { range: start..end, lines: None })
}

/// `i(`/`a(` and the other bracket pairs, `count` levels out.
pub(super) fn bracket_object(buf: &Buffer, pos: usize, open: char, close: char, count: usize, around: bool) -> Option<Object> {
    let len = buf.len_chars();
    let pos = pos.min(len.saturating_sub(1));
    // Find the enclosing opener: walk back counting nesting.
    let mut a = None;
    let mut depth = 0usize;
    let mut need = count;
    // On an opener, that bracket is the first level; on a closer, its opener is.
    let mut i = pos + 1;
    while i > 0 {
        i -= 1;
        let c = buf.char_at(i)?;
        if c == close && i != pos {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                need -= 1;
                if need == 0 {
                    a = Some(i);
                    break;
                }
            } else {
                depth -= 1;
            }
        }
    }
    // Not inside a pair: like Vim, use the next one on the line.
    let a = match a {
        Some(a) => a,
        None if count == 1 => {
            let le = line_end(buf, line_of(buf, pos));
            (pos..le).find(|&i| buf.char_at(i) == Some(open))?
        }
        None => return None,
    };
    let mut depth = 0usize;
    let mut b = None;
    for (k, c) in buf.rope().chars_at(a).enumerate() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                b = Some(a + k);
                break;
            }
        }
    }
    let b = b?;
    if around {
        return Some(Object { range: a..b + 1, lines: None });
    }
    let start = a + 1;
    // `foo(\n  x\n)`: the inner block is the lines between the brackets.
    let (al, bl) = (line_of(buf, a), line_of(buf, b));
    if buf.char_at(start) == Some('\n') && first_non_blank(buf, bl) == b && bl > al + 1 {
        return Some(Object { range: line_start(buf, al + 1)..line_start(buf, bl), lines: Some((al + 1, bl - 1)) });
    }
    Some(Object { range: start..b, lines: None })
}

/// `ip`/`ap`: a run of non-empty lines (or of empty lines), linewise.
pub(super) fn paragraph_object(buf: &Buffer, pos: usize, count: usize, around: bool) -> Object {
    let last = last_line(buf);
    let mut l0 = line_of(buf, pos);
    let empty = line_is_empty(buf, l0);
    while l0 > 0 && line_is_empty(buf, l0 - 1) == empty {
        l0 -= 1;
    }
    let mut l1 = line_of(buf, pos);
    let mut kind = empty;
    let mut runs = if around { count * 2 } else { count };
    loop {
        while l1 < last && line_is_empty(buf, l1 + 1) == kind {
            l1 += 1;
        }
        runs -= 1;
        if runs == 0 || l1 >= last {
            break;
        }
        l1 += 1;
        kind = !kind;
    }
    // `ap` at the end of the buffer takes the blank lines before it instead.
    if around && runs > 0 && !empty {
        while l0 > 0 && line_is_empty(buf, l0 - 1) {
            l0 -= 1;
        }
    }
    Object { range: line_start(buf, l0)..next_line_start(buf, l1), lines: Some((l0, l1)) }
}

/// `it`/`at`: the innermost `<tag>…</tag>` around `pos`.
pub(super) fn tag_object(buf: &Buffer, pos: usize, count: usize, around: bool) -> Option<Object> {
    let text = buf.to_string();
    let chars: Vec<char> = text.chars().collect();
    // (name, start, end, closing) for each tag, in order.
    let mut tags: Vec<(String, usize, usize, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            let closing = chars.get(i + 1) == Some(&'/');
            let ns = i + 1 + usize::from(closing);
            let mut ne = ns;
            while ne < chars.len() && (chars[ne].is_alphanumeric() || chars[ne] == '-' || chars[ne] == ':' || chars[ne] == '_') {
                ne += 1;
            }
            if ne > ns
                && let Some(gt) = (ne..chars.len()).find(|&k| chars[k] == '>')
            {
                let self_closing = chars[gt - 1] == '/';
                if !self_closing {
                    tags.push((chars[ns..ne].iter().collect(), i, gt + 1, closing));
                }
                i = gt + 1;
                continue;
            }
        }
        i += 1;
    }
    // Pair them with a stack; the pairs around `pos`, innermost first.
    let mut stack: Vec<usize> = Vec::new();
    let mut around_pos: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (k, t) in tags.iter().enumerate() {
        if t.3 {
            if let Some(j) = stack.iter().rposition(|&j| tags[j].0 == t.0) {
                let o = &tags[stack[j]];
                stack.truncate(j);
                let (os, oe, cs, ce) = (o.1, o.2, t.1, t.2);
                if os <= pos && pos < ce {
                    around_pos.push((os, oe, cs, ce));
                }
            }
        } else {
            stack.push(k);
        }
    }
    around_pos.sort_by_key(|b| b.3 - b.0);
    let (os, oe, cs, ce) = *around_pos.get(count.max(1) - 1)?;
    Some(Object { range: if around { os..ce } else { oe..cs }, lines: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_stop_where_vim_stops() {
        let b = Buffer::from("foo.bar baz\n\n  qux");
        assert_eq!(word_fwd(&b, 0, 1, false, false), 3);
        assert_eq!(word_fwd(&b, 0, 1, true, false), 8);
        assert_eq!(word_fwd(&b, 8, 1, false, false), 12, "an empty line is a word");
        assert_eq!(word_fwd(&b, 12, 1, false, false), 15);
        assert_eq!(word_fwd(&b, 8, 1, false, true), 11, "with an operator, the line's last word ends at its end");
        assert_eq!(word_back(&b, 15, 1, false), 12);
        assert_eq!(word_back(&b, 12, 1, false), 8);
        assert_eq!(word_end(&b, 0, 1, false, false), 2);
        assert_eq!(word_end(&b, 2, 1, false, false), 3);
        assert_eq!(word_end_back(&b, 8, 1, false), 6);
    }

    #[test]
    fn brackets_and_paragraphs() {
        let b = Buffer::from("a (b [c] d) e\n\npara\ntwo\n\nend");
        assert_eq!(match_pair(&b, 0), Some(10));
        assert_eq!(match_pair(&b, 10), Some(2));
        assert_eq!(paragraph(&b, 0, 1, true), Some((14, false)));
        assert_eq!(paragraph(&b, 15, 1, true), Some((24, false)));
        assert_eq!(paragraph(&b, 15, 2, true), Some((27, true)));
        let o = bracket_object(&b, 6, '(', ')', 1, false).unwrap();
        assert_eq!(o.range, 3..10);
        assert_eq!(find_char(&b, 0, 1, FindKind::Till, 'b', false), Some(2));
    }
}
