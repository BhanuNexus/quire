// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Find & replace, in one note or across the vault.

use std::ops::Range;

use quire_text::{Buffer, Change, Transaction};
use quire_vault::NotePath;
use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct FindOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// Compile a pattern with options. Plain text is escaped; empty patterns are an error.
pub fn compile(pattern: &str, opts: FindOptions) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("empty pattern".into());
    }
    let core = if opts.regex { pattern.to_string() } else { regex::escape(pattern) };
    let pat = if opts.whole_word { format!(r"\b(?:{core})\b") } else { core };
    RegexBuilder::new(&pat).case_insensitive(!opts.case_sensitive).multi_line(true).build().map_err(|e| e.to_string())
}

/// Non-empty matches as byte ranges.
pub fn find_bytes(text: &str, re: &Regex) -> Vec<Range<usize>> {
    re.find_iter(text).filter(|m| !m.is_empty()).map(|m| m.range()).collect()
}

/// Matches as char ranges in `buf` (`text` must be `buf`'s contents).
pub fn find_chars(buf: &Buffer, text: &str, re: &Regex) -> Vec<Range<usize>> {
    find_bytes(text, re).into_iter().map(|r| buf.byte_to_char(r.start)..buf.byte_to_char(r.end)).collect()
}

/// The text a single match is replaced with (`$1` expands in regex mode).
pub fn expand(re: &Regex, text: &str, m: &Range<usize>, replacement: &str, opts: FindOptions) -> String {
    if !opts.regex {
        return replacement.to_string();
    }
    match re.captures_at(text, m.start) {
        Some(caps) if caps.get(0).is_some_and(|g| g.range() == *m) => {
            let mut out = String::new();
            caps.expand(replacement, &mut out);
            out
        }
        _ => replacement.to_string(),
    }
}

/// One transaction replacing every match (char ranges in `buf`).
pub fn replace_all(buf: &Buffer, text: &str, re: &Regex, replacement: &str, opts: FindOptions) -> (Transaction, usize) {
    let ms = find_bytes(text, re);
    let changes: Vec<Change> = ms
        .iter()
        .map(|m| Change::new(buf.byte_to_char(m.start), buf.byte_to_char(m.end), expand(re, text, m, replacement, opts)))
        .collect();
    let n = changes.len();
    (Transaction::new(changes).expect("regex matches never overlap"), n)
}

/// Replace all matches in a plain string (for notes that aren't open).
pub fn replace_in_string(text: &str, re: &Regex, replacement: &str, opts: FindOptions) -> (String, usize) {
    let ms = find_bytes(text, re);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for m in &ms {
        out.push_str(&text[pos..m.start]);
        out.push_str(&expand(re, text, m, replacement, opts));
        pos = m.end;
    }
    out.push_str(&text[pos..]);
    (out, ms.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    /// 1-based line number.
    pub line: usize,
    /// The full line, trimmed.
    pub context: String,
    /// Byte offset of the match in the note.
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    pub path: NotePath,
    pub count: usize,
    /// Up to a handful of example lines.
    pub lines: Vec<LineMatch>,
}

/// Matches across many notes. `notes` yields (path, current text).
pub fn find_in_notes<'a>(notes: impl Iterator<Item = (NotePath, &'a str)>, re: &Regex, max_lines: usize) -> Vec<FileMatches> {
    let mut out = Vec::new();
    for (path, text) in notes {
        let ms = find_bytes(text, re);
        if ms.is_empty() {
            continue;
        }
        let lines = ms
            .iter()
            .take(max_lines)
            .map(|m| {
                let ls = text[..m.start].rfind('\n').map_or(0, |i| i + 1);
                let le = text[m.end..].find('\n').map_or(text.len(), |i| m.end + i);
                LineMatch { line: text[..m.start].matches('\n').count() + 1, context: text[ls..le].trim().to_string(), offset: m.start }
            })
            .collect();
        out.push(FileMatches { path, count: ms.len(), lines });
    }
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_and_expansion() {
        let opts = FindOptions::default();
        let re = compile("rope", opts).unwrap();
        assert_eq!(find_bytes("Rope ropes trope", &re).len(), 3);
        let re = compile("rope", FindOptions { whole_word: true, ..opts }).unwrap();
        assert_eq!(find_bytes("Rope ropes trope", &re), vec![0..4]);
        let re = compile("rope", FindOptions { case_sensitive: true, ..opts }).unwrap();
        assert_eq!(find_bytes("Rope ropes", &re), vec![5..9]);
        assert!(compile("a.b(", FindOptions { regex: true, ..opts }).is_err());
        assert_eq!(find_bytes("a.b axb", &compile("a.b", opts).unwrap()), vec![0..3]);

        let ro = FindOptions { regex: true, ..opts };
        let re = compile(r"(\w+)@(\w+)", ro).unwrap();
        assert_eq!(replace_in_string("me@home, you@work", &re, "$2:$1", ro), ("home:me, work:you".into(), 2));
    }

    #[test]
    fn replace_all_is_one_transaction() {
        let text = "Ропе rope ROPE";
        let mut buf = Buffer::from(text);
        let re = compile("rope", FindOptions::default()).unwrap();
        let (tx, n) = replace_all(&buf, text, &re, "cord", FindOptions::default());
        assert_eq!(n, 2);
        tx.apply(&mut buf).unwrap();
        assert_eq!(buf.to_string(), "Ропе cord cord");
        assert_eq!(find_chars(&Buffer::from("ñ rope"), "ñ rope", &re), vec![2..6]);
    }

    #[test]
    fn vault_matches_have_context() {
        let re = compile("todo", FindOptions::default()).unwrap();
        let notes = vec![(NotePath::new("A.md"), "x\n- TODO one\n- todo two"), (NotePath::new("B.md"), "nothing")];
        let r = find_in_notes(notes.into_iter(), &re, 5);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].count, 2);
        assert_eq!(r[0].lines[0], LineMatch { line: 2, context: "- TODO one".into(), offset: 4 });
    }
}
