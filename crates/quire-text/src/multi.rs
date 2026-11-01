// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Multi-cursor editing. Every operation turns a set of selections into one
//! [`Transaction`] plus the resulting selections, so undo treats a
//! multi-cursor edit as a single step.

use crate::motion::{next_word_end, prev_word_start, word_at};
use crate::{Assoc, Buffer, Change, Selection, Transaction};

/// Sort by position and merge selections that overlap or share a caret.
pub fn normalize(mut sels: Vec<Selection>) -> Vec<Selection> {
    sels.sort_by_key(|s| (s.start(), s.end()));
    let mut out: Vec<Selection> = Vec::with_capacity(sels.len());
    for s in sels {
        match out.last_mut() {
            Some(last) if s.start() < last.end() || (s.start() == last.end() && (s.is_empty() || last.is_empty())) => {
                let (start, end) = (last.start().min(s.start()), last.end().max(s.end()));
                // Keep the direction of the selection that was there first.
                *last = if last.head >= last.anchor { Selection::new(start, end) } else { Selection::new(end, start) };
            }
            _ => out.push(s),
        }
    }
    out
}

/// Replace each selection with `texts[i]` (or `texts[0]` for all). Carets land after the inserted text.
pub fn replace_each(sels: &[Selection], texts: &[String]) -> (Transaction, Vec<Selection>) {
    let sels = normalize(sels.to_vec());
    let pick = |i: usize| if texts.len() == sels.len() { &texts[i] } else { &texts[0] };
    let mut changes = Vec::with_capacity(sels.len());
    let mut out = Vec::with_capacity(sels.len());
    let mut shift: isize = 0;
    for (i, s) in sels.iter().enumerate() {
        let t = pick(i);
        let n = t.chars().count();
        changes.push(Change::new(s.start(), s.end(), t.clone()));
        let caret = (s.start() as isize + shift) as usize + n;
        out.push(Selection::cursor(caret));
        shift += n as isize - (s.end() - s.start()) as isize;
    }
    (Transaction::new(changes).expect("normalized selections never overlap"), normalize(out))
}

pub fn insert(sels: &[Selection], text: &str) -> (Transaction, Vec<Selection>) {
    replace_each(sels, &[text.to_string()])
}

fn delete_ranges(buf: &Buffer, sels: &[Selection], range_for: impl Fn(usize) -> (usize, usize)) -> Option<(Transaction, Vec<Selection>)> {
    let ranges: Vec<Selection> = normalize(sels.to_vec())
        .into_iter()
        .map(|s| {
            if s.is_empty() {
                let (a, b) = range_for(s.head);
                Selection::new(a.min(buf.len_chars()), b.min(buf.len_chars()))
            } else {
                s
            }
        })
        .collect();
    let ranges = normalize(ranges);
    if ranges.iter().all(Selection::is_empty) {
        return None;
    }
    Some(replace_each(&ranges, &[String::new()]))
}

/// Backspace at every cursor (word-wise with `word`).
pub fn delete_backward(buf: &Buffer, sels: &[Selection], word: bool) -> Option<(Transaction, Vec<Selection>)> {
    delete_ranges(buf, sels, |p| (if word { prev_word_start(buf, p) } else { p.saturating_sub(1) }, p))
}

/// Delete at every cursor (word-wise with `word`).
pub fn delete_forward(buf: &Buffer, sels: &[Selection], word: bool) -> Option<(Transaction, Vec<Selection>)> {
    delete_ranges(buf, sels, |p| (p, if word { next_word_end(buf, p) } else { p + 1 }))
}

/// Ctrl+D: select the word under the newest caret, or add the next occurrence of the selected text.
pub fn add_next_occurrence(buf: &Buffer, sels: &[Selection]) -> Vec<Selection> {
    let Some(last) = sels.last().copied() else { return sels.to_vec() };
    if last.is_empty() {
        let w = word_at(buf, last.head);
        let mut v = sels.to_vec();
        *v.last_mut().expect("non-empty") = Selection::new(w.start, w.end);
        return v;
    }
    let needle = buf.slice(last.range());
    let text = buf.to_string();
    let starts: Vec<usize> = text.match_indices(&needle).map(|(b, _)| buf.byte_to_char(b)).collect();
    let taken: Vec<usize> = sels.iter().map(Selection::start).collect();
    let n = needle.chars().count();
    let next = starts.iter().find(|&&s| s >= last.end() && !taken.contains(&s)).or_else(|| starts.iter().find(|s| !taken.contains(s)));
    let mut v = sels.to_vec();
    if let Some(&s) = next {
        v.push(Selection::new(s, s + n));
    }
    v
}

/// Select every occurrence of the current selection (or word under the caret).
pub fn all_occurrences(buf: &Buffer, sel: Selection) -> Vec<Selection> {
    let range = if sel.is_empty() { word_at(buf, sel.head) } else { sel.range() };
    if range.is_empty() {
        return vec![sel];
    }
    let needle = buf.slice(range);
    let n = needle.chars().count();
    buf.to_string().match_indices(&needle).map(|(b, _)| buf.byte_to_char(b)).map(|s| Selection::new(s, s + n)).collect()
}

/// Add a caret on the line above (`dir < 0`) or below at the same column.
pub fn add_cursor_vertical(buf: &Buffer, sel: Selection, dir: isize) -> Option<Selection> {
    let (line, col) = buf.line_col(sel.head);
    let target = line as isize + dir;
    if target < 0 || target as usize >= buf.len_lines() {
        return None;
    }
    let r = buf.line_range(target as usize);
    Some(Selection::cursor((r.start + col).min(r.end)))
}

/// Map secondary selections through a transaction made elsewhere (e.g. a smart edit).
pub fn map_all(tx: &Transaction, sels: &[Selection]) -> Vec<Selection> {
    normalize(sels.iter().map(|s| Selection::new(tx.map_pos(s.anchor, Assoc::After), tx.map_pos(s.head, Assoc::After))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        text: &str,
        sels: &[Selection],
        f: impl Fn(&Buffer, &[Selection]) -> Option<(Transaction, Vec<Selection>)>,
    ) -> (String, Vec<Selection>) {
        let mut b = Buffer::from(text);
        let (tx, s) = f(&b, sels).unwrap();
        tx.apply(&mut b).unwrap();
        (b.to_string(), s)
    }

    #[test]
    fn types_at_every_cursor() {
        let sels = [Selection::cursor(0), Selection::cursor(4), Selection::new(8, 11)];
        let (t, s) = run("abc\ndef\nghi", &sels, |_, s| Some(insert(s, "- ")));
        assert_eq!(t, "- abc\n- def\n- ");
        assert_eq!(s, vec![Selection::cursor(2), Selection::cursor(8), Selection::cursor(14)]);
    }

    #[test]
    fn deletes_and_merges() {
        let sels = [Selection::cursor(1), Selection::cursor(2), Selection::cursor(6)];
        let (t, s) = run("abcdefg", &sels, |b, s| delete_backward(b, s, false));
        assert_eq!(t, "cdeg");
        assert_eq!(s, vec![Selection::cursor(0), Selection::cursor(3)]);
        let (t, _) = run("one two three", &[Selection::cursor(3), Selection::cursor(13)], |b, s| delete_backward(b, s, true));
        assert_eq!(t, " two ");
        assert!(delete_backward(&Buffer::from("x"), &[Selection::cursor(0)], false).is_none());
    }

    #[test]
    fn occurrences_and_vertical() {
        let b = Buffer::from("foo bar foo baz foo");
        let s = add_next_occurrence(&b, &[Selection::cursor(1)]);
        assert_eq!(s, vec![Selection::new(0, 3)]);
        let s = add_next_occurrence(&b, &s);
        let s = add_next_occurrence(&b, &s);
        assert_eq!(s.iter().map(|x| x.start()).collect::<Vec<_>>(), vec![0, 8, 16]);
        assert_eq!(all_occurrences(&b, Selection::cursor(9)).len(), 3);
        let b = Buffer::from("long line\nab\nthird line");
        assert_eq!(add_cursor_vertical(&b, Selection::cursor(7), 1), Some(Selection::cursor(12)));
        assert_eq!(add_cursor_vertical(&b, Selection::cursor(12), 1), Some(Selection::cursor(15)));
        assert_eq!(add_cursor_vertical(&b, Selection::cursor(2), -1), None);
    }

    #[test]
    fn distributes_paste_lines() {
        let (t, _) = run("a\nb", &[Selection::cursor(1), Selection::cursor(3)], |_, s| Some(replace_each(s, &["1".into(), "2".into()])));
        assert_eq!(t, "a1\nb2");
    }
}
