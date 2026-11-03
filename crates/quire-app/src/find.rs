// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Find & replace, in one note or across the vault.

use std::ops::Range;

use quire_text::{Buffer, Change, Transaction};
use quire_vault::NotePath;
use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FindOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    /// Accents must match ("cafe" doesn't find "café"). Off by default in
    /// the find bar (design §C.2); regular expressions always match as written.
    pub match_accents: bool,
}

/// Exact matching, as in 0.8 (the CLI, MCP, Vim and vault-wide replace).
impl Default for FindOptions {
    fn default() -> Self {
        Self { case_sensitive: false, whole_word: false, regex: false, match_accents: true }
    }
}

impl FindOptions {
    /// The in-note find bar's start: accents optional (design §C.2).
    pub fn find_bar() -> Self {
        Self { match_accents: false, ..Self::default() }
    }
}

/// Compile a pattern with options. Plain text is escaped; empty patterns are an error.
pub fn compile(pattern: &str, opts: FindOptions) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("empty pattern".into());
    }
    let core = if opts.regex {
        pattern.to_string()
    } else if opts.match_accents {
        regex::escape(pattern)
    } else {
        accent_insensitive(pattern, opts.case_sensitive)
    };
    let pat = if opts.whole_word { format!(r"\b(?:{core})\b") } else { core };
    RegexBuilder::new(&pat).case_insensitive(!opts.case_sensitive).multi_line(true).build().map_err(|e| e.to_string())
}

/// Every char that folds to `f` (one ASCII or base letter), by folded letter.
fn variants() -> &'static std::collections::HashMap<char, Vec<char>> {
    static TABLE: std::sync::OnceLock<std::collections::HashMap<char, Vec<char>>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t: std::collections::HashMap<char, Vec<char>> = std::collections::HashMap::new();
        let ranges = [0x00C0..=0x024F, 0x0370..=0x03FF, 0x0400..=0x04FF, 0x1E00..=0x1EFF, 0x1F00..=0x1FFF];
        for c in ranges.into_iter().flatten().filter_map(char::from_u32) {
            let f = quire_index::tokenize::fold_str(&c.to_string());
            let mut it = f.chars();
            if let (Some(base), None) = (it.next(), it.next())
                && base != c
                && base.is_alphanumeric()
            {
                t.entry(base).or_default().push(c);
            }
        }
        t
    })
}

/// Combining accents a decomposed letter may carry.
const MARKS: &str = r"[\x{0300}-\x{036F}\x{1AB0}-\x{1AFF}\x{1DC0}-\x{1DFF}]*";

/// A pattern that matches `text` with or without accents, on the original
/// text: every letter becomes the class of letters that fold to it ("e" →
/// `[eéèêë…]`), followed by optional combining accents; "ss", "ae" and "oe"
/// also match "ß", "æ" and "œ".
fn accent_insensitive(text: &str, case_sensitive: bool) -> String {
    use std::fmt::Write as _;
    let units: Vec<(char, bool)> = text
        .chars()
        .flat_map(|c| {
            let upper = c.is_uppercase();
            quire_index::tokenize::fold_str(&c.to_string()).chars().map(move |f| (f, upper)).collect::<Vec<_>>()
        })
        .collect();
    let class = |(f, upper): (char, bool)| -> String {
        if !f.is_alphanumeric() {
            return regex::escape(&f.to_string());
        }
        let mut members: Vec<char> = vec![f];
        if upper {
            members.extend(f.to_uppercase());
        }
        if let Some(v) = variants().get(&f) {
            members.extend(v.iter().copied().filter(|x| !case_sensitive || x.is_uppercase() == upper));
        }
        if case_sensitive && upper {
            members.retain(|x| x.is_uppercase() || !x.is_alphabetic());
        }
        members.sort_unstable();
        members.dedup();
        let mut out = String::from("[");
        for m in members {
            let _ = write!(out, "\\x{{{:X}}}", u32::from(m));
        }
        out.push(']');
        out.push_str(MARKS);
        out
    };
    let mut out = String::new();
    let mut i = 0;
    while i < units.len() {
        let pair = units.get(i + 1).map(|n| (units[i].0, n.0));
        let lig = match pair {
            Some(('s', 's')) => Some("ßẞ"),
            Some(('a', 'e')) => Some("æÆ"),
            Some(('o', 'e')) => Some("œŒ"),
            _ => None,
        };
        if let Some(lig) = lig {
            let _ = write!(out, "(?:[{lig}]|{}{})", class(units[i]), class(units[i + 1]));
            i += 2;
        } else {
            out.push_str(&class(units[i]));
            i += 1;
        }
    }
    out
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
    fn accents_are_optional_unless_asked() {
        let text = "Café, cafe, CAFÉ, cafe\u{301}, Straße, strasse, Æsir, Łódź";
        let words = |pattern: &str, opts: FindOptions| -> Vec<String> {
            find_bytes(text, &compile(pattern, opts).unwrap()).into_iter().map(|r| text[r].to_string()).collect()
        };
        let loose = FindOptions::find_bar();
        assert_eq!(words("cafe", loose), ["Café", "cafe", "CAFÉ", "cafe\u{301}"]);
        assert_eq!(words("café", loose), ["Café", "cafe", "CAFÉ", "cafe\u{301}"]);
        assert_eq!(words("strasse", loose), ["Straße", "strasse"]);
        assert_eq!(words("straße", loose), ["Straße", "strasse"]);
        assert_eq!(words("aesir", loose), ["Æsir"]);
        assert_eq!(words("lodz", loose), ["Łódź"]);
        let exact = FindOptions::default();
        assert_eq!(words("cafe", exact), ["cafe", "cafe"]);
        let cased = FindOptions { case_sensitive: true, ..loose };
        assert_eq!(words("Cafe", cased), ["Café"]);
        assert_eq!(words("CAFE", cased), ["CAFÉ"]);
        let word = FindOptions { whole_word: true, ..loose };
        assert_eq!(words("caf", word), Vec::<String>::new());
        assert_eq!(words("a.b(", loose), Vec::<String>::new(), "punctuation is escaped");
        let (out, n) = replace_in_string("un café", &compile("cafe", loose).unwrap(), "bar", loose);
        assert_eq!((out.as_str(), n), ("un bar", 1));
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
