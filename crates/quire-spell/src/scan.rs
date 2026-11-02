// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Markdown-aware tokenizing: which parts of a line are prose words worth
//! spell-checking, and which are code, math, links, tags and the like.
//!
//! Two layers:
//! - [`LineContext::advance`] tracks block structure across lines (front
//!   matter, fenced and indented code, `$$` math, HTML comments) and says
//!   whether a line is prose at all.
//! - [`for_each_word`] walks one prose line, skips inline constructs and
//!   hands every candidate word's byte range to a callback.
//!
//! Neither layer knows about dictionaries; the speller decides which
//! candidates are misspelled.

use std::ops::Range;

/// Block-level state carried from one line to the next.
///
/// Start each document with [`LineContext::new`] and feed it every line in
/// order (via [`crate::Speller::misspellings`] or [`LineContext::advance`]).
/// It is small and `Copy`, so an editor can store one per line and restart
/// checking from any line whose context it already knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineContext {
    block: Block,
    /// At least one line has been seen (front matter only opens on line 0).
    started: bool,
    /// The previous line was paragraph text, so an indented line is a lazy
    /// continuation rather than the start of an indented code block.
    paragraph: bool,
    /// Inside an indented (4 spaces / tab) code block.
    indented_code: bool,
    /// Inside a list, where indented lines are item content, not code.
    in_list: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Block {
    Normal,
    /// YAML front matter between `---` lines at the top of the document.
    FrontMatter,
    /// Fenced code: the fence character and the opening run length.
    Fence {
        marker: u8,
        len: u8,
    },
    /// A `$$` display-math block.
    Math,
    /// A multi-line `<!-- ... -->` HTML comment.
    Comment,
}

impl Default for LineContext {
    fn default() -> Self {
        Self::new()
    }
}

impl LineContext {
    /// Context for the first line of a document.
    pub const fn new() -> Self {
        Self { block: Block::Normal, started: false, paragraph: false, indented_code: false, in_list: false }
    }

    /// True while inside a fenced code block, front matter, `$$` math block or
    /// multi-line HTML comment (that is, the next line will not be checked
    /// unless it closes the block).
    pub fn in_block(self) -> bool {
        !matches!(self.block, Block::Normal)
    }

    /// Moves the context past `line` and returns whether `line` is prose that
    /// should be spell-checked.
    ///
    /// This is cheap (no dictionary lookups), so an editor can run it over a
    /// whole document to know the context of the first visible line.
    pub fn advance(&mut self, line: &str) -> bool {
        let first = !self.started;
        self.started = true;
        let mut line = line.strip_suffix('\n').unwrap_or(line);
        line = line.strip_suffix('\r').unwrap_or(line);
        if first {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }

        match self.block {
            Block::Normal => {}
            Block::FrontMatter => {
                let t = line.trim_end();
                if t == "---" || t == "..." {
                    self.end_block();
                }
                return false;
            }
            Block::Fence { marker, len } => {
                if is_fence_close(strip_quotes(line.trim_start()), marker, len) {
                    self.end_block();
                }
                return false;
            }
            Block::Math => {
                if line.contains("$$") {
                    self.end_block();
                }
                return false;
            }
            Block::Comment => {
                if line.contains("-->") {
                    self.end_block();
                }
                return false;
            }
        }

        if first && line.trim_end() == "---" {
            self.block = Block::FrontMatter;
            return false;
        }
        if line.trim().is_empty() {
            // Indented code survives blank lines; paragraphs do not.
            self.paragraph = false;
            return false;
        }

        let indent = indent_width(line);
        if indent >= 4 && (self.indented_code || (!self.paragraph && !self.in_list)) {
            self.indented_code = true;
            return false;
        }
        self.indented_code = false;

        let content = strip_quotes(line.trim_start());
        if let Some((marker, len)) = fence_open(content) {
            self.block = Block::Fence { marker, len };
            self.paragraph = false;
            return false;
        }
        if let Some(rest) = content.strip_prefix("$$") {
            if !rest.contains("$$") {
                self.block = Block::Math;
            }
            self.paragraph = false;
            return false;
        }
        if let Some(rest) = content.strip_prefix("<!--")
            && !rest.contains("-->")
        {
            self.block = Block::Comment;
            self.paragraph = false;
            return false;
        }

        if is_list_item(content) {
            self.in_list = true;
        } else if indent < 2 && !self.paragraph && !line.trim_start().starts_with('>') {
            self.in_list = false;
        }
        self.paragraph = !is_heading(content);
        true
    }

    fn end_block(&mut self) {
        self.block = Block::Normal;
        self.paragraph = false;
    }
}

/// Width of the leading whitespace, with tabs advancing to the next stop of 4.
fn indent_width(line: &str) -> usize {
    let mut width = 0;
    for b in line.bytes() {
        match b {
            b' ' => width += 1,
            b'\t' => width += 4 - width % 4,
            _ => break,
        }
    }
    width
}

/// Strips any `>` blockquote / callout markers from an already left-trimmed line.
fn strip_quotes(mut s: &str) -> &str {
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    s
}

fn fence_open(s: &str) -> Option<(u8, u8)> {
    let marker = *s.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = s.bytes().take_while(|&b| b == marker).count();
    if len < 3 {
        return None;
    }
    // A backtick fence's info string cannot contain backticks (that would be inline code).
    if marker == b'`' && s[len..].contains('`') {
        return None;
    }
    Some((marker, u8::try_from(len).unwrap_or(u8::MAX)))
}

fn is_fence_close(s: &str, marker: u8, open_len: u8) -> bool {
    let len = s.bytes().take_while(|&b| b == marker).count();
    len >= 3 && len >= usize::from(open_len) && s[len..].trim().is_empty()
}

fn is_list_item(s: &str) -> bool {
    let b = s.as_bytes();
    match b.first() {
        Some(b'-' | b'*' | b'+') => matches!(b.get(1), None | Some(b' ' | b'\t')),
        Some(c) if c.is_ascii_digit() => {
            let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
            digits <= 9 && matches!(b.get(digits), Some(b'.' | b')')) && matches!(b.get(digits + 1), None | Some(b' ' | b'\t'))
        }
        _ => false,
    }
}

fn is_heading(s: &str) -> bool {
    let hashes = s.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && matches!(s.as_bytes().get(hashes), None | Some(b' ' | b'\t'))
}

// ------------------------------------------------------------------ inline

/// Characters that end a whitespace-delimited "chunk" (the unit we test for
/// URLs, e-mail addresses, file names, tags...). Brackets, quotes and
/// emphasis markers separate a URL from the Markdown around it.
fn is_chunk_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '`' | '|' | '*' | '“' | '”' | '«' | '»')
}

/// Combining marks (accents in decomposed text) and joiners belong to the word.
pub(crate) fn is_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F | 0x200C | 0x200D)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_mark(c)
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

/// The char starting at byte `i` of `s`, if `i < end`.
fn char_at(s: &str, i: usize, end: usize) -> Option<char> {
    if i >= end { None } else { s[i..end].chars().next() }
}

/// Calls `f` with the byte range of every word on `line` that should be
/// spell-checked, skipping inline Markdown that is not prose.
///
/// Words already filtered out here: shorter than two letters, containing
/// digits or underscores, ALL-CAPS, or with an internal capital letter.
pub(crate) fn for_each_word(line: &str, f: &mut impl FnMut(Range<usize>)) {
    scan_range(line, 0, line.len(), f);
}

/// Positions from which a search for a closing delimiter is known to fail.
/// A failed search from `i` also fails from any later position, so
/// remembering it keeps lines full of unclosed `$`, `[[` or `<` linear.
struct Unclosed {
    math: usize,
    wikilink: usize,
    tag: usize,
}

fn scan_range(s: &str, start: usize, end: usize, f: &mut impl FnMut(Range<usize>)) {
    let b = s.as_bytes();
    let mut unclosed = Unclosed { math: end, wikilink: end, tag: end };
    let mut i = start;
    // `prev` is the char before `i` within this range (None at the start).
    let mut prev: Option<char> = None;
    while let Some(c) = char_at(s, i, end) {
        let at_chunk_start = prev.is_none_or(is_chunk_delim) && !is_chunk_delim(c);
        if at_chunk_start {
            let chunk_end = s[i..end].find(is_chunk_delim).map_or(end, |n| i + n);
            if skip_chunk(&s[i..chunk_end]) {
                prev = s[..chunk_end].chars().next_back();
                i = chunk_end;
                continue;
            }
        }

        let next = match c {
            '\\' => skip_escape(s, i, end),
            '`' => skip_code_span(s, i, end),
            '$' => skip_math(s, i, end, &mut unclosed.math),
            '<' => skip_html(s, i, end, &mut unclosed.tag),
            '&' => skip_entity(s, i, end),
            '[' if b.get(i + 1) == Some(&b'[') => skip_wikilink(s, i, end, &mut unclosed.wikilink, f),
            '[' if matches!(b.get(i + 1), Some(b'^' | b'!')) => skip_label(s, i, end),
            ']' if b.get(i + 1) == Some(&b'(') => skip_link_destination(s, i + 1, end),
            ']' if b.get(i + 1) == Some(&b'[') => s[i + 1..end].find(']').map_or(i + 1, |n| i + 1 + n + 1),
            c if is_word_char(c) => {
                let word_end = word_end(s, i, end);
                if worth_checking(&s[i..word_end]) {
                    f(i..word_end);
                }
                word_end
            }
            c => i + c.len_utf8(),
        };
        debug_assert!(next > i && s.is_char_boundary(next), "scanner must advance on char boundaries");
        prev = s[..next].chars().next_back();
        i = next;
    }
}

/// End of the word starting at `i`: word characters plus apostrophes that sit
/// between letters (`don't`, `it’s`), but not a trailing one (`users'`).
fn word_end(s: &str, mut i: usize, end: usize) -> usize {
    while let Some(c) = char_at(s, i, end) {
        let joins = is_word_char(c) || (is_apostrophe(c) && char_at(s, i + c.len_utf8(), end).is_some_and(char::is_alphabetic));
        if !joins {
            break;
        }
        i += c.len_utf8();
    }
    i
}

/// Shape rules that make a token not worth checking against a dictionary.
fn worth_checking(word: &str) -> bool {
    let mut chars = 0usize;
    let mut upper = 0usize;
    let mut lower = 0usize;
    for (n, c) in word.chars().enumerate() {
        chars += 1;
        if c.is_numeric() || c == '_' {
            return false; // `abc123`, `snake_case`
        }
        if c.is_uppercase() {
            if n > 0 {
                return false; // camelCase, PascalCase, iPhone, ALL-CAPS
            }
            upper += 1;
        } else if c.is_lowercase() {
            lower += 1;
        }
    }
    // Single letters, absurdly long tokens (hashes, base64) and caps-only tokens.
    (2..=48).contains(&chars) && (lower > 0 || upper == 0)
}

/// Whether a whole whitespace-delimited chunk is a URL, address, path, file
/// name, tag, mention or block id.
fn skip_chunk(chunk: &str) -> bool {
    let first = chunk.chars().next();
    let second = chunk.chars().nth(1);
    // `#tag`, `@mention`, `^block-id`
    if matches!(first, Some('#' | '@' | '^')) && second.is_some_and(|c| c.is_alphanumeric() || c == '_') {
        return true;
    }
    let t = chunk.trim_matches(|c: char| matches!(c, '\'' | '\u{2019}' | '.' | ',' | ';' | ':' | '!' | '?' | '_' | '~' | '='));
    if t.is_empty() {
        return false;
    }
    if t.contains("://") || t.contains(":\\") {
        return true; // URL, Windows path
    }
    let lower_prefix = |p: &str| t.len() >= p.len() && t.is_char_boundary(p.len()) && t[..p.len()].eq_ignore_ascii_case(p);
    if lower_prefix("www.") || lower_prefix("mailto:") || t.starts_with('/') || t.starts_with("./") || t.starts_with("../") {
        return true;
    }
    if let Some(at) = t.find('@')
        && at > 0
        && t[at + 1..].contains('.')
    {
        return true; // e-mail address
    }
    // A dot between two alphanumerics: example.com, main.rs, README.md, e.g, v1.2
    let mut prev: Option<char> = None;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '.' && prev.is_some_and(char::is_alphanumeric) && chars.peek().is_some_and(|n| n.is_alphanumeric()) {
            return true;
        }
        prev = Some(c);
    }
    false
}

/// `\` escapes one punctuation character; `\word` is a LaTeX-style command.
fn skip_escape(s: &str, i: usize, end: usize) -> usize {
    match char_at(s, i + 1, end) {
        Some(c) if c.is_alphabetic() => {
            let mut j = i + 1;
            while let Some(c) = char_at(s, j, end).filter(|c| c.is_alphabetic()) {
                j += c.len_utf8();
            }
            j
        }
        Some(c) => i + 1 + c.len_utf8(),
        None => i + 1,
    }
}

/// An inline code span: a run of N backticks up to the next run of exactly N.
fn skip_code_span(s: &str, i: usize, end: usize) -> usize {
    let b = s.as_bytes();
    let n = b[i..end].iter().take_while(|&&c| c == b'`').count();
    let mut j = i + n;
    while j < end {
        if b[j] == b'`' {
            let run = b[j..end].iter().take_while(|&&c| c == b'`').count();
            if run == n {
                return j + run;
            }
            j += run;
        } else {
            j += 1;
        }
    }
    // Unmatched: the backticks are literal text.
    i + n
}

/// Inline math: `$$...$$`, or `$...$` where the opening `$` is followed by a
/// non-space and the closing one follows a non-space and is not followed by a
/// digit (so prices like "$5 and $10" stay prose).
fn skip_math(s: &str, i: usize, end: usize, unclosed: &mut usize) -> usize {
    let b = s.as_bytes();
    if b.get(i + 1) == Some(&b'$') {
        return s[i + 2..end].find("$$").map_or(i + 2, |n| i + 2 + n + 2);
    }
    if i >= *unclosed || char_at(s, i + 1, end).is_none_or(char::is_whitespace) {
        return i + 1;
    }
    let mut j = i + 1;
    while let Some(c) = char_at(s, j, end) {
        if c == '\\' {
            j = skip_escape(s, j, end);
            continue;
        }
        if c == '$' {
            let before_ok = !b[j - 1].is_ascii_whitespace();
            let after_ok = !b.get(j + 1).is_some_and(u8::is_ascii_digit);
            if before_ok && after_ok {
                return j + 1;
            }
        }
        j += c.len_utf8();
    }
    *unclosed = i;
    i + 1
}

/// HTML tags, comments and `<autolinks>`.
fn skip_html(s: &str, i: usize, end: usize, unclosed: &mut usize) -> usize {
    let rest = &s[i..end];
    if let Some(comment) = rest.strip_prefix("<!--") {
        return comment.find("-->").map_or(end, |n| i + 4 + n + 3);
    }
    let opens_tag = rest[1..].chars().next().is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
    if opens_tag && i < *unclosed {
        match rest.find('>') {
            Some(n) => return i + n + 1,
            None => *unclosed = i,
        }
    }
    i + 1
}

/// `&amp;`, `&#8217;`, `&nbsp;`
fn skip_entity(s: &str, i: usize, end: usize) -> usize {
    let b = s.as_bytes();
    let mut j = i + 1;
    if b.get(j) == Some(&b'#') {
        j += 1;
    }
    let name_start = j;
    while j < end && b[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j > name_start && j < end && b[j] == b';' { j + 1 } else { i + 1 }
}

/// `[[target]]`, `[[target|alias]]`: the target is a note name and is not
/// checked; the alias is prose shown to the reader, so it is.
fn skip_wikilink(s: &str, i: usize, end: usize, unclosed: &mut usize, f: &mut impl FnMut(Range<usize>)) -> usize {
    let inner_start = i + 2;
    if i >= *unclosed {
        return inner_start;
    }
    let Some(n) = s[inner_start..end].find("]]") else {
        *unclosed = i;
        return inner_start;
    };
    let inner_end = inner_start + n;
    if let Some(p) = s[inner_start..inner_end].find('|') {
        scan_range(s, inner_start + p + 1, inner_end, f);
    }
    inner_end + 2
}

/// Footnote refs `[^1]` / `[^my-note]` and callout types `[!note]`: a label
/// without spaces in brackets.
fn skip_label(s: &str, i: usize, end: usize) -> usize {
    let label_start = i + 2;
    match s[label_start..end].find(|c: char| c == ']' || c.is_whitespace()) {
        Some(n) if n > 0 && s.as_bytes()[label_start + n] == b']' => label_start + n + 1,
        _ => i + 1,
    }
}

/// `](url "title")`: skip to the matching `)`, allowing nested parentheses.
/// `open` is the index of the `(`.
fn skip_link_destination(s: &str, open: usize, end: usize) -> usize {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut j = open;
    while j < end {
        match b[j] {
            b'\\' => j += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    // Unclosed: skip the destination up to the next whitespace.
    let stop = s[open..end].find(char::is_whitespace).map_or(end, |n| open + n);
    if stop > open { stop } else { open + 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for_each_word(line, &mut |r| out.push(&line[r]));
        out
    }

    #[test]
    fn plain_words() {
        assert_eq!(words("Hello, world! Don't panic."), ["Hello", "world", "Don't", "panic"]);
    }

    #[test]
    fn link_text_checked_url_skipped() {
        assert_eq!(words("see [the docs](https://example.org/pathh_(b)) now"), ["see", "the", "docs", "now"]);
    }

    #[test]
    fn wikilink_alias() {
        assert_eq!(words("[[Some Notte|shown txt]] and [[Hidden Wrd]]"), ["shown", "txt", "and"]);
    }

    #[test]
    fn context_tracks_fences() {
        let mut ctx = LineContext::new();
        let lines = ["text", "```rust", "let x = 1;", "```", "after", "~~~~", "~~~", "still code", "~~~~", "out"];
        let prose: Vec<bool> = lines.iter().map(|l| ctx.advance(l)).collect();
        assert_eq!(prose, [true, false, false, false, true, false, false, false, false, true]);
    }

    #[test]
    fn skip_link_destination_on_odd_input() {
        // Unbalanced and escaped parens never panic or loop.
        for s in ["](", "](\\", "](()", "]( x", "](a\\)b)"] {
            let mut n = 0;
            for_each_word(s, &mut |_| n += 1);
        }
    }
}
