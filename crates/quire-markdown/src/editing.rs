// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Smart edits. Each returns `Some(Transaction)` when it applies, or `None` so
//! the frontend falls back to its default behaviour. All positions are chars.

use quire_text::{Buffer, Change, Selection, Transaction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prefix {
    Bullet(char),
    Ordered { n: u64, delim: char },
    Task { bullet: String, done: bool },
    Quote(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePrefix {
    pub indent: String,
    pub kind: Prefix,
    /// Length in chars of indent + marker (+ trailing space).
    pub len: usize,
}

/// Recognise a list/task/quote prefix at the start of `line`.
pub fn parse_prefix(line: &str) -> Option<LinePrefix> {
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let rest = &line[indent.len()..];
    let ilen = indent.chars().count();

    if rest.starts_with('>') {
        let q: String = rest.chars().take_while(|c| *c == '>' || *c == ' ').collect();
        let len = ilen + q.chars().count();
        return Some(LinePrefix { indent, kind: Prefix::Quote(q), len });
    }
    let mut chars = rest.chars();
    let first = chars.next()?;
    if matches!(first, '-' | '*' | '+') && rest[1..].starts_with(' ') {
        let after = &rest[2..];
        for (pat, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
            if after.starts_with(pat) || after == pat.trim_end() {
                let bullet = format!("{first} ");
                let len = ilen + 2 + pat.len().min(after.len());
                return Some(LinePrefix { indent, kind: Prefix::Task { bullet, done }, len });
            }
        }
        return Some(LinePrefix { indent, kind: Prefix::Bullet(first), len: ilen + 2 });
    }
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 9 {
        let tail = &rest[digits.len()..];
        if let Some(delim) = tail.chars().next().filter(|c| *c == '.' || *c == ')')
            && tail[1..].starts_with(' ')
        {
            let n = digits.parse().ok()?;
            return Some(LinePrefix { indent, kind: Prefix::Ordered { n, delim }, len: ilen + digits.len() + 2 });
        }
    }
    None
}

fn next_marker(p: &LinePrefix) -> String {
    let m = match &p.kind {
        Prefix::Bullet(c) => format!("{c} "),
        Prefix::Ordered { n, delim } => format!("{}{delim} ", n + 1),
        Prefix::Task { bullet, .. } => format!("{bullet}[ ] "),
        Prefix::Quote(q) => q.clone(),
    };
    format!("{}{m}", p.indent)
}

/// Enter inside a list/task/quote: continue it, or end it on an empty item.
pub fn on_enter(buf: &Buffer, sel: Selection) -> Option<Transaction> {
    if !sel.is_empty() {
        return None;
    }
    let line = buf.char_to_line(sel.head);
    let range = buf.line_range(line);
    let text = buf.line(line);
    let prefix = parse_prefix(&text)?;
    let line_start = range.start;
    if sel.head < line_start + prefix.len {
        return None;
    }
    let content_empty = text.chars().skip(prefix.len).all(char::is_whitespace);
    if content_empty {
        // Leave the list: clear the marker, keep the caret on this line.
        return Some(Transaction::replace(line_start, range.end, "").with_selection(Selection::cursor(line_start)));
    }
    let insert = format!("\n{}", next_marker(&prefix));
    let caret = sel.head + insert.chars().count();
    Some(Transaction::insert(sel.head, insert).with_selection(Selection::cursor(caret)))
}

fn lines_of(buf: &Buffer, sel: Selection) -> std::ops::RangeInclusive<usize> {
    let first = buf.char_to_line(sel.start());
    let mut last = buf.char_to_line(sel.end());
    // A selection ending at column 0 doesn't include that line.
    if last > first && sel.end() == buf.line_to_char(last) {
        last -= 1;
    }
    first..=last
}

/// Tab / Shift-Tab on list items or multi-line selections.
pub fn indent(buf: &Buffer, sel: Selection, unit: &str, outdent: bool) -> Option<Transaction> {
    let lines = lines_of(buf, sel);
    let multi = lines.start() != lines.end();
    if !multi && parse_prefix(&buf.line(*lines.start())).is_none_or(|p| matches!(p.kind, Prefix::Quote(_))) {
        return None;
    }
    let mut changes = Vec::new();
    for l in lines {
        let start = buf.line_to_char(l);
        let text = buf.line(l);
        if outdent {
            let remove = if text.starts_with(unit) {
                unit.chars().count()
            } else if text.starts_with('\t') {
                1
            } else {
                text.chars().take(4).take_while(|c| *c == ' ').count()
            };
            if remove > 0 {
                changes.push(Change::new(start, start + remove, ""));
            }
        } else if !text.is_empty() || !multi {
            changes.push(Change::new(start, start, unit));
        }
    }
    let tx = Transaction::new(changes).ok()?;
    if tx.is_empty() {
        return None;
    }
    let mapped = tx.map_selection(sel);
    Some(tx.with_selection(mapped))
}

/// Cycle a line: plain → `- [ ] ` task → done `[x]` → open `[ ]`.
pub fn toggle_task(buf: &Buffer, sel: Selection) -> Option<Transaction> {
    let line = buf.char_to_line(sel.head);
    let start = buf.line_to_char(line);
    let text = buf.line(line);
    let tx = match parse_prefix(&text) {
        Some(LinePrefix { indent, kind: Prefix::Task { bullet, done }, .. }) => {
            let at = start + indent.chars().count() + bullet.chars().count() + 1;
            Transaction::replace(at, at + 1, if done { " " } else { "x" })
        }
        Some(LinePrefix { indent, kind: Prefix::Bullet(_), len }) => {
            let _ = indent;
            Transaction::insert(start + len, "[ ] ")
        }
        Some(LinePrefix { indent, kind: Prefix::Ordered { .. }, len }) => {
            let ilen = indent.chars().count();
            Transaction::replace(start + ilen, start + len, "- [ ] ")
        }
        _ => {
            let ilen = text.chars().take_while(|c| c.is_whitespace()).count();
            Transaction::insert(start + ilen, "- [ ] ")
        }
    };
    let mapped = tx.map_selection(sel);
    Some(tx.with_selection(mapped))
}

/// Wrap/unwrap the selection with `marker` (`**`, `_`, `` ` ``, `==`, `~~`).
pub fn toggle_wrap(buf: &Buffer, sel: Selection, marker: &str) -> Transaction {
    let m = marker.chars().count();
    let (s, e) = (sel.start(), sel.end());
    if sel.is_empty() {
        let pair = format!("{marker}{marker}");
        return Transaction::insert(s, pair).with_selection(Selection::cursor(s + m));
    }
    let len = buf.len_chars();
    if s >= m && e + m <= len && buf.slice(s - m..s) == marker && buf.slice(e..e + m) == marker {
        let tx = Transaction::new(vec![Change::new(s - m, s, ""), Change::new(e, e + m, "")]).expect("disjoint");
        return tx.with_selection(Selection::new(s - m, e - m));
    }
    let inner = buf.slice(s..e);
    if e - s >= 2 * m && inner.starts_with(marker) && inner.ends_with(marker) {
        let tx = Transaction::new(vec![Change::new(s, s + m, ""), Change::new(e - m, e, "")]).expect("disjoint");
        return tx.with_selection(Selection::new(s, e - 2 * m));
    }
    let tx = Transaction::new(vec![Change::new(s, s, marker), Change::new(e, e, marker)]).expect("disjoint");
    tx.with_selection(Selection::new(s + m, e + m))
}

/// Set the current line's heading level (0 = paragraph). Same level toggles off.
pub fn set_heading(buf: &Buffer, sel: Selection, level: u8) -> Transaction {
    let line = buf.char_to_line(sel.head);
    let start = buf.line_to_char(line);
    let text = buf.line(line);
    let hashes = text.chars().take_while(|c| *c == '#').count();
    let existing = if hashes > 0 && hashes <= 6 && text[hashes..].starts_with(' ') { hashes } else { 0 };
    let strip = if existing > 0 { existing + 1 } else { 0 };
    let target = if existing == level as usize { 0 } else { level as usize };
    let new_prefix = if target == 0 { String::new() } else { format!("{} ", "#".repeat(target)) };
    let tx = Transaction::replace(start, start + strip, new_prefix);
    let mapped = tx.map_selection(sel);
    tx.with_selection(mapped)
}

/// Toggle a line prefix such as `> ` on every selected line.
pub fn toggle_line_prefix(buf: &Buffer, sel: Selection, prefix: &str) -> Transaction {
    let lines: Vec<usize> = lines_of(buf, sel).collect();
    let all_have = lines.iter().all(|&l| buf.line(l).starts_with(prefix));
    let n = prefix.chars().count();
    let changes = lines
        .iter()
        .map(|&l| {
            let s = buf.line_to_char(l);
            if all_have { Change::new(s, s + n, "") } else { Change::new(s, s, prefix) }
        })
        .collect();
    let tx = Transaction::new(changes).expect("one change per line");
    let mapped = tx.map_selection(sel);
    tx.with_selection(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, caret: usize, f: impl Fn(&Buffer, Selection) -> Option<Transaction>) -> Option<(String, usize)> {
        let mut buf = Buffer::from(text);
        let tx = f(&buf, Selection::cursor(caret))?;
        tx.apply(&mut buf).unwrap();
        Some((buf.to_string(), tx.selection().unwrap().head))
    }

    #[test]
    fn continues_lists() {
        assert_eq!(run("- item", 6, on_enter), Some(("- item\n- ".into(), 9)));
        assert_eq!(run("  3. three", 10, on_enter), Some(("  3. three\n  4. ".into(), 16)));
        assert_eq!(run("- [x] done", 10, on_enter), Some(("- [x] done\n- [ ] ".into(), 17)));
        assert_eq!(run("> > quoted", 10, on_enter), Some(("> > quoted\n> > ".into(), 15)));
        assert_eq!(run("plain", 5, on_enter), None);
    }

    #[test]
    fn empty_item_exits_list() {
        assert_eq!(run("- a\n- ", 6, on_enter), Some(("- a\n".into(), 4)));
        assert_eq!(run("- a\n- [ ] ", 10, on_enter), Some(("- a\n".into(), 4)));
    }

    #[test]
    fn toggles_tasks() {
        assert_eq!(run("buy milk", 3, toggle_task).unwrap().0, "- [ ] buy milk");
        assert_eq!(run("- [ ] buy milk", 8, toggle_task).unwrap().0, "- [x] buy milk");
        assert_eq!(run("- [x] buy milk", 8, toggle_task).unwrap().0, "- [ ] buy milk");
        assert_eq!(run("- buy milk", 3, toggle_task).unwrap().0, "- [ ] buy milk");
    }

    #[test]
    fn wraps_and_unwraps() {
        let mut buf = Buffer::from("make this bold");
        let tx = toggle_wrap(&buf, Selection::new(5, 9), "**");
        tx.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "make **this** bold");
        let sel = tx.selection().unwrap();
        let tx2 = toggle_wrap(&buf, sel, "**");
        tx2.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "make this bold");
    }

    #[test]
    fn headings_and_indent() {
        let mut buf = Buffer::from("## Title");
        set_heading(&buf, Selection::cursor(4), 1).apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "# Title");
        set_heading(&buf, Selection::cursor(4), 1).apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "Title");

        assert_eq!(run("- a", 3, |b, s| indent(b, s, "\t", false)).unwrap().0, "\t- a");
        assert_eq!(run("\t- a", 3, |b, s| indent(b, s, "\t", true)).unwrap().0, "- a");
        assert!(run("plain", 3, |b, s| indent(b, s, "\t", false)).is_none());
    }
}
