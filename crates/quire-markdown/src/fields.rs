// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Typed links (0.12): where a link sits in a field, the field names it.
//!
//! Three places give a link a type, the same ones Dataview and Breadcrumbs
//! read (docs/formats/links.md):
//!
//! * an inline field on its own line: `parent:: [[Rope]]` (also after a
//!   list marker or `>`; a task line needs the bracketed form);
//! * a bracketed inline field anywhere: `[supports:: [[Claim]]]`, or with
//!   parentheses, `(source:: [[Book]])`;
//! * a front-matter property: `up: "[[Rope]]"`, `up: ["[[A]]", "[[B]]"]` or
//!   a block list of links.
//!
//! The type is the field's name normalized as Dataview does: formatting
//! (`**`, `_`, `~`) dropped, lower case, spaces as `-`. Nothing here
//! decides what a type means (`up` is a parent); `quire-index` does, from
//! the vault's `[links]` settings.

use std::ops::Range;

use crate::extract::{Link, LinkKind};
use crate::scan::{Inline, wikilink_at};

/// How a field is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldStyle {
    /// `key:: value` on its own line.
    Line,
    /// `[key:: value]`.
    Bracket,
    /// `(key:: value)`: the key is hidden in reading view.
    Paren,
    /// A front-matter property.
    FrontMatter,
}

/// An inline field in the body of a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineField {
    /// The normalized name (`parent`, `see-also`).
    pub name: String,
    /// The whole field, brackets included for the bracketed forms; for a
    /// line field, the line from the key to its end (without the newline).
    pub range: Range<usize>,
    pub key_range: Range<usize>,
    /// The value, trimmed.
    pub value_range: Range<usize>,
    pub style: FieldStyle,
}

/// The longest key read as a field name; longer text before `::` is prose.
const MAX_KEY: usize = 64;

/// A field name as Dataview indexes it: wrapping formatting (`**`, `_`,
/// `~`) dropped, lower case, runs of spaces as one `-`. `**Parent Note**`
/// → `parent-note`; `see_also` stays `see_also`.
pub fn normalize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut space = false;
    for c in key.trim().trim_matches(['*', '_', '~', '`']).chars() {
        if matches!(c, '*' | '~' | '`') {
            continue;
        }
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push('-');
            space = false;
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Whether `key` (as written, before `::`) can be a field name. As in
/// Dataview: letters, digits, `-`, `_`, `/` and spaces (emoji and other
/// non-ASCII letters too), starting with a letter or digit, optionally
/// wrapped in `**`, `_` or `~` formatting.
fn is_key(key: &str) -> bool {
    let k = key.trim().trim_matches(['*', '_', '~']);
    k.len() <= MAX_KEY
        && k.chars().next().is_some_and(|c| c.is_alphanumeric() || !c.is_ascii())
        && k.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/' | ' ' | '\t' | '*' | '~') || (!c.is_ascii() && !c.is_whitespace()))
}

/// Whether `name` can name a typed-link field (what [`add_link`] writes).
pub fn is_field_name(name: &str) -> bool {
    is_key(name)
}

/// Is byte `pos` inside one of the sorted, merged `verbatim` ranges?
fn in_verbatim(verbatim: &[Range<usize>], pos: usize) -> bool {
    let i = verbatim.partition_point(|r| r.end <= pos);
    verbatim.get(i).is_some_and(|r| r.start <= pos)
}

/// Where the text of a line starts after quote markers and a list marker,
/// and whether the line is a task (`- [ ] …`).
fn line_body(line: &str) -> (usize, bool) {
    let b = line.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if i < b.len() && b[i] == b'>' {
            i += 1;
            continue;
        }
        break;
    }
    let mut j = i;
    if j < b.len() && matches!(b[j], b'-' | b'*' | b'+') && b.get(j + 1) == Some(&b' ') {
        j += 2;
    } else {
        let digits = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && digits <= 9 && matches!(b.get(j + digits), Some(b'.' | b')')) && b.get(j + digits + 1) == Some(&b' ') {
            j += digits + 2;
        }
    }
    if j > i {
        while j < b.len() && b[j] == b' ' {
            j += 1;
        }
        let task = b.len() >= j + 3 && b[j] == b'[' && b[j + 2] == b']' && b.get(j + 3).is_none_or(|c| *c == b' ');
        return (j, task);
    }
    (i, false)
}

/// The bracket that closes the one at `open` (`[` or `(`), counting nested
/// pairs of the same kind, on the same line.
fn closing(line: &str, open: usize) -> Option<usize> {
    let b = line.as_bytes();
    let (o, c) = if b[open] == b'[' { (b'[', b']') } else { (b'(', b')') };
    let mut depth = 0usize;
    for (i, &ch) in b.iter().enumerate().skip(open) {
        if ch == o {
            depth += 1;
        } else if ch == c {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn trimmed(src: &str, r: Range<usize>) -> Range<usize> {
    let s = &src[r.clone()];
    let lead = s.len() - s.trim_start().len();
    r.start + lead..r.start + lead + s.trim().len()
}

/// Every inline field in `src` outside `verbatim` (code, math, front
/// matter…), in order. Cheap when the note has no `::`.
pub fn inline_fields_with(src: &str, verbatim: &[Range<usize>]) -> Vec<InlineField> {
    let mut out = Vec::new();
    if !src.contains("::") {
        return out;
    }
    let mut at = 0;
    for raw in src.split_inclusive('\n') {
        let start = at;
        at += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if !line.contains("::") || in_verbatim(verbatim, start) {
            continue;
        }
        let mut bracketed: Vec<Range<usize>> = Vec::new();
        // Bracketed and parenthesised fields, anywhere on the line.
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            let opens = (c == b'[' && b.get(i + 1) != Some(&b'[') && (i == 0 || b[i - 1] != b'[')) || c == b'(';
            if opens
                && let Some(sep) = line[i + 1..].find("::").map(|n| i + 1 + n)
                && is_key(&line[i + 1..sep])
                && !in_verbatim(verbatim, start + i)
                && let Some(close) = closing(line, i)
                && close > sep
            {
                let key_range = trimmed(src, start + i + 1..start + sep);
                out.push(InlineField {
                    name: normalize_key(&src[key_range.clone()]),
                    range: start + i..start + close + 1,
                    key_range,
                    value_range: trimmed(src, start + sep + 2..start + close),
                    style: if c == b'[' { FieldStyle::Bracket } else { FieldStyle::Paren },
                });
                bracketed.push(i..close + 1);
                i = close + 1;
                continue;
            }
            i += 1;
        }
        // A line field: `key:: value` with nothing before the key.
        let (body, task) = line_body(line);
        if task || bracketed.iter().any(|r| r.start == body) {
            continue;
        }
        if let Some(sep) = line[body..].find("::").map(|n| body + n)
            && is_key(&line[body..sep])
            && !bracketed.iter().any(|r| r.contains(&sep))
            && !in_verbatim(verbatim, start + sep)
        {
            let key_range = trimmed(src, start + body..start + sep);
            out.push(InlineField {
                name: normalize_key(&src[key_range.clone()]),
                range: start + body..start + line.len(),
                key_range,
                value_range: trimmed(src, start + sep + 2..start + line.len()),
                style: FieldStyle::Line,
            });
        }
    }
    out.sort_by_key(|f| f.range.start);
    out
}

/// [`inline_fields_with`] for a whole note (parses it for code ranges).
pub fn inline_fields(src: &str) -> Vec<InlineField> {
    inline_fields_with(src, &crate::scan::verbatim_ranges(src))
}

/// The key of a front-matter line (`key: …` or `key:`), its byte range in
/// the line, and where its value starts.
fn fm_key(line: &str) -> Option<(Range<usize>, usize)> {
    let lead = line.len() - line.trim_start().len();
    let t = &line[lead..];
    if t.starts_with('-') || t.starts_with('#') {
        return None;
    }
    if let Some(q) = t.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let close = t[1..].find(q)? + 1;
        let rest = &t[close + 1..];
        let after = rest.strip_prefix(':')?;
        if !(after.is_empty() || after.starts_with([' ', '\t'])) {
            return None;
        }
        return Some((lead + 1..lead + close, lead + close + 2));
    }
    let colon = t.find(": ").or_else(|| t.find(":\t")).or_else(|| t.ends_with(':').then(|| t.len() - 1))?;
    let key = &t[..colon];
    if key.trim().is_empty() || key.contains(['[', '{', '"']) {
        return None;
    }
    Some((lead..lead + key.trim_end().len(), lead + colon + 1))
}

/// What a left-to-right read of a property's value text found.
struct ValueScan {
    /// The quote (`'` or `"`) still open at the end.
    quote: Option<char>,
    /// Where a `# comment` starts (a `#` after a space, outside quotes).
    comment: Option<usize>,
    /// Where the flow list or map that opens the value closes, if it does.
    close: Option<usize>,
}

/// Read `value` (text of a property's value, from where it starts) the way
/// YAML does. A quote opens only at the start of a scalar (at the start,
/// after `[`, `{`, `,` or a list dash), so the apostrophe in `Bob's` opens
/// nothing. In single quotes `''` is a quote; in double quotes `\` escapes
/// the next character.
fn scan_value(value: &str) -> ValueScan {
    scan(value, false)
}

/// [`scan_value`]; with `lines`, a comment ends at its line and the read goes on.
fn scan(value: &str, lines: bool) -> ValueScan {
    let mut close = None;
    let chars: Vec<(usize, char)> = value.char_indices().collect();
    let ws_after = |k: usize| chars.get(k + 1).is_none_or(|(_, c)| c.is_whitespace());
    let (mut open, mut at_start, mut depth, mut prev): (Option<char>, bool, usize, Option<char>) = (None, true, 0, None);
    let mut k = 0;
    while k < chars.len() {
        let (i, c) = chars[k];
        if let Some(q) = open {
            if q == '\'' && c == '\'' {
                if chars.get(k + 1).is_some_and(|(_, n)| *n == '\'') {
                    k += 1;
                } else {
                    open = None;
                    at_start = false;
                }
            } else if q == '"' && c == '\\' {
                k += 1;
            } else if q == '"' && c == '"' {
                open = None;
                at_start = false;
            }
            k += 1;
            continue;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' => {}
            '#' if prev.is_none_or(char::is_whitespace) => {
                if !lines {
                    return ValueScan { quote: None, comment: Some(i), close };
                }
                while chars.get(k + 1).is_some_and(|(_, n)| *n != '\n') {
                    k += 1;
                }
            }
            '\'' | '"' if at_start => open = Some(c),
            '[' | '{' if at_start => {
                depth += 1;
                at_start = true;
            }
            ',' if depth > 0 => at_start = true,
            ']' | '}' if depth > 0 => {
                depth -= 1;
                at_start = false;
                if depth == 0 && close.is_none() {
                    close = Some(i);
                }
            }
            ':' if depth > 0 && ws_after(k) => at_start = true,
            '-' | '?' if at_start && depth == 0 && ws_after(k) => {}
            _ => at_start = false,
        }
        prev = Some(c);
        k += 1;
    }
    ValueScan { quote: open, comment: None, close }
}

/// The YAML quote (`'` or `"`) still open at the end of `value`, if any.
///
/// `value` is the text of a property's value up to some point (see
/// [`front_matter_context`] for a whole note).
pub fn yaml_quote_at(value: &str) -> Option<char> {
    scan_value(value).quote
}

/// The text of a double-quoted YAML scalar's body (`\"`, `\\`, `\t`, `\uXXXX`).
fn unescape_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                } else {
                    out.push_str("\\u");
                    out.push_str(&hex);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// One line of front matter, by what it can hold.
enum LineKind {
    /// Blank, or only a comment: nothing to read.
    Skip,
    /// `key: value`: the key's range and where its value starts.
    Key(Range<usize>, usize),
    /// `- value`.
    Item(usize),
    /// Inside a block scalar (`|` or `>`): plain text, no comments or quotes.
    Literal,
    /// Anything else: a later line of a multi-line value.
    Cont,
}

struct FmLine<'a> {
    start: usize,
    text: &'a str,
    kind: LineKind,
}

fn fm_lines(src: &str, end: usize) -> Vec<FmLine<'_>> {
    let block = &src[..end.min(src.len())];
    let mut out = Vec::new();
    let mut literal_in: Option<usize> = None;
    let mut at = 0;
    for (n, raw) in block.split_inclusive('\n').enumerate() {
        let start = at;
        at += raw.len();
        let text = raw.trim_end_matches(['\n', '\r']);
        if n == 0 || text.trim_end() == "---" || text.trim_end() == "..." {
            continue;
        }
        let lead = text.len() - text.trim_start().len();
        let t = &text[lead..];
        if let Some(k) = literal_in {
            if t.is_empty() || lead > k {
                out.push(FmLine { start, text, kind: LineKind::Literal });
                continue;
            }
            literal_in = None;
        }
        let kind = if t.is_empty() || t.starts_with('#') {
            LineKind::Skip
        } else if let Some((k, v)) = fm_key(text) {
            LineKind::Key(k, v)
        } else if t == "-" || t.starts_with("- ") {
            LineKind::Item(lead + 1 + t[1..].len() - t[1..].trim_start().len())
        } else {
            LineKind::Cont
        };
        let value_from = match &kind {
            LineKind::Key(_, v) | LineKind::Item(v) => Some(*v),
            _ => None,
        };
        if value_from.is_some_and(|v| text[v.min(text.len())..].trim_start().starts_with(['|', '>'])) {
            literal_in = Some(lead);
        }
        out.push(FmLine { start, text, kind });
    }
    out
}

/// Byte ranges (in `s`) of backtick code spans.
fn code_spans(s: &str) -> Vec<Range<usize>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let run = b[i..].iter().take_while(|c| **c == b'`').count();
        let mut j = i + run;
        let mut found = None;
        while j < b.len() {
            if b[j] == b'`' {
                let r = b[j..].iter().take_while(|c| **c == b'`').count();
                if r == run {
                    found = Some(j + r);
                    break;
                }
                j += r;
            } else {
                j += 1;
            }
        }
        match found {
            Some(e) => {
                out.push(i..e);
                i = e;
            }
            None => i += run,
        }
    }
    out
}

/// The links in front-matter values, each typed by its property's name
/// (the nearest key above it, for list items and nested maps). `end` is
/// where the front-matter block ends.
///
/// Comments (`# …` after a value, or on a line of their own) and code
/// spans are not read. Only wikilinks and embeds count; Markdown links in
/// properties are not links (docs/formats/links.md).
pub(crate) fn front_matter_links(src: &str, end: usize) -> Vec<Link> {
    let mut out = Vec::new();
    if !src[..end.min(src.len())].contains("[[") {
        return out;
    }
    let mut key: Option<String> = None;
    for l in fm_lines(src, end) {
        let value_from = match &l.kind {
            LineKind::Skip => continue,
            LineKind::Key(k, v) => {
                key = Some(normalize_key(&l.text[k.clone()]));
                *v
            }
            LineKind::Item(v) => *v,
            LineKind::Literal | LineKind::Cont => 0,
        };
        let Some(name) = key.as_ref().filter(|k| !k.is_empty()) else { continue };
        let value_from = value_from.min(l.text.len());
        let limit = if matches!(l.kind, LineKind::Literal) {
            l.text.len()
        } else {
            scan_value(&l.text[value_from..]).comment.map_or(l.text.len(), |c| value_from + c)
        };
        let spans = code_spans(&l.text[..limit]);
        let mut i = value_from;
        while let Some(off) = l.text[i.min(limit)..limit].find("[[") {
            let open = i + off;
            let embed = open > 0 && l.text.as_bytes()[open - 1] == b'!';
            if spans.iter().any(|r| r.contains(&open)) {
                i = open + 2;
                continue;
            }
            match wikilink_at(src, l.start + open) {
                Some((Inline::WikiLink { range, link }, end)) if end - l.start <= limit => {
                    let range = if embed { range.start - 1..range.end } else { range };
                    let q = scan_value(&l.text[value_from..open]).quote;
                    let unq = |s: &str| match q {
                        // In a single-quoted YAML value, `''` is one `'`.
                        Some('\'') => s.replace("''", "'"),
                        Some(_) => unescape_dq(s),
                        None => s.to_string(),
                    };
                    out.push(Link {
                        kind: if embed { LinkKind::Embed } else { LinkKind::Wiki },
                        display: unq(link.display()),
                        target: unq(&link.target),
                        heading: link.heading.as_deref().map(unq),
                        range,
                        field: Some(name.clone()),
                    });
                    i = end - l.start;
                }
                _ => i = open + 2,
            }
        }
    }
    out
}

/// How YAML reads the text at a place in front matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FmContext {
    /// Inside single quotes (`'` is written `''`).
    Single,
    /// Inside double quotes (`\` and `"` are escaped).
    Double,
    /// Inside a block scalar: text as it is.
    Literal,
    /// In a flow list or map, not in quotes.
    Flow,
    /// An unquoted scalar, in block context: the bytes it spans (without
    /// trailing space and comment).
    Plain(Range<usize>),
    /// A later line of a multi-line value: can't be told.
    Unknown,
}

/// How YAML reads byte `at` of `src`, a position inside the front matter
/// that ends at byte `end`.
pub fn front_matter_context(src: &str, end: usize, at: usize) -> FmContext {
    for l in fm_lines(src, end) {
        if at < l.start || at > l.start + l.text.len() {
            continue;
        }
        let value_from = match l.kind {
            LineKind::Literal => return FmContext::Literal,
            LineKind::Skip => return FmContext::Unknown,
            // A later line of a multi-line value: a quote can still be read from the line's own start.
            LineKind::Cont => {
                return match scan_value(&l.text[..(at - l.start).min(l.text.len())]).quote {
                    Some('\'') => FmContext::Single,
                    Some(_) => FmContext::Double,
                    None => FmContext::Unknown,
                };
            }
            LineKind::Key(_, v) | LineKind::Item(v) => v.min(l.text.len()),
        };
        let rel = at - l.start;
        let value = &l.text[value_from..];
        if let Some(q) = scan_value(&l.text[value_from..rel.max(value_from)]).quote {
            return if q == '\'' { FmContext::Single } else { FmContext::Double };
        }
        let value_end = scan_value(value).comment.map_or(l.text.len(), |c| value_from + c);
        let trimmed = l.text[..value_end].trim_end();
        let first = value.trim_start();
        let begin = l.text.len() - first.len();
        if first.starts_with(['[', '{']) {
            return FmContext::Flow;
        }
        return FmContext::Plain(l.start + begin..l.start + trimmed.len().max(begin));
    }
    FmContext::Unknown
}

/// Give each link inside an inline field's value that field's name.
pub(crate) fn type_links(links: &mut [Link], fields: &[InlineField]) {
    if fields.is_empty() {
        return;
    }
    for l in links.iter_mut().filter(|l| l.field.is_none() && l.is_internal()) {
        // The innermost field holding the link (a bracketed field inside a line field wins).
        let i = fields.partition_point(|f| f.range.start <= l.range.start);
        let best = fields[..i]
            .iter()
            .filter(|f| f.value_range.start <= l.range.start && l.range.end <= f.value_range.end)
            .min_by_key(|f| f.range.len());
        if let Some(f) = best {
            l.field = Some(f.name.clone());
        }
    }
}

/// How a note already writes its typed links: in front matter or as inline fields.
///
/// A note with neither uses front matter (Obsidian's properties,
/// Breadcrumbs' default), unless it already has inline fields of other
/// kinds (`status:: done`).
pub fn preferred_style(src: &str) -> FieldStyle {
    let ex = crate::extract(src);
    let fm_end = ex.front_matter.as_ref().map_or(0, |f| f.range.end);
    let typed = ex.links.iter().filter(|l| l.field.is_some());
    let (fm, inline) = typed.fold((0, 0), |(a, b), l| if l.range.start < fm_end { (a + 1, b) } else { (a, b + 1) });
    if inline > fm || (fm == 0 && inline_fields(src).iter().any(|f| f.style == FieldStyle::Line)) {
        FieldStyle::Line
    } else {
        FieldStyle::FrontMatter
    }
}

/// A double-quoted YAML scalar holding `s`.
fn dq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `raw` (one plain scalar) as an item of a flow list.
fn flow_item(raw: &str) -> String {
    let t = crate::frontmatter::yaml_scalar(raw);
    if !t.starts_with('"') && t.contains([',', '[', ']', '{', '}']) { dq(&t) } else { t }
}

/// The edit that adds a link to `target` in field `field` of a note.
///
/// `target` is a note name (written `[[target]]`), `field` a name as
/// [`is_field_name`] allows, `style` [`FieldStyle::Line`] or
/// [`FieldStyle::FrontMatter`]. `None` when the note already has that link
/// in that field, or when it can't be added without rewriting what the
/// note has there ([`try_add_link`] says which).
///
/// * Front matter: a list field gets another item in its own style (`[…,
///   "[[X]]"]`, also over several lines, or `- "[[X]]"`); a single value
///   becomes a list of two, with its text and trailing comment as they were;
///   a missing field is added as `field: "[[X]]"`.
/// * Inline: an existing `field:: …` line gets `, [[X]]`; else a new line
///   `field:: [[X]]` goes after the last line field, or after the title
///   heading, or at the top of the body.
pub fn add_link(src: &str, field: &str, target: &str, style: FieldStyle) -> Option<crate::frontmatter::Edit> {
    try_add_link(src, field, target, style).ok().flatten()
}

/// [`add_link`] that says why it didn't: `Ok(None)` is "already linked";
/// `Err` is a value it won't rewrite (a block scalar, a multi-line text,
/// an anchor, a nested map) or a front matter it can't read.
pub fn try_add_link(src: &str, field: &str, target: &str, style: FieldStyle) -> Result<Option<crate::frontmatter::Edit>, &'static str> {
    use crate::frontmatter::{self, Edit, Value};
    let name = normalize_key(field);
    let link = format!("[[{}]]", target.trim());
    let ex = crate::extract(src);
    let already = ex.links.iter().any(|l| l.field.as_deref() == Some(name.as_str()) && l.target.eq_ignore_ascii_case(target.trim()));
    if already {
        return Ok(None);
    }
    if !is_key(field) || target.trim().is_empty() || target.contains(['[', ']', '|']) || target.chars().any(char::is_control) {
        return Ok(None);
    }
    let nl = if src.contains("\r\n") { "\r\n" } else { "\n" };
    if style == FieldStyle::FrontMatter {
        let fm = ex.front_matter.as_ref();
        let key = fm.and_then(|f| f.fields.keys().find(|k| normalize_key(k) == name).cloned()).unwrap_or_else(|| field.trim().to_string());
        let item = dq(&link);
        let edit = match frontmatter::field_text(src, &key) {
            None => frontmatter::set_field(src, &key, &Value::Str(link)),
            Some(text) => add_to_field(src, text, &key, &item, fm.and_then(|f| f.fields.get(&key)))?,
        };
        // Never hand back front matter that no longer parses.
        let out = edit.apply(src);
        if frontmatter::yaml_valid(src) == Some(true) && frontmatter::yaml_valid(&out) != Some(true) {
            return Err("the result wouldn't be valid YAML");
        }
        return Ok(Some(edit));
    }
    let fields = inline_fields(src);
    let line_fields: Vec<&InlineField> = fields.iter().filter(|f| f.style == FieldStyle::Line).collect();
    if let Some(f) = line_fields.iter().find(|f| f.name == name) {
        let sep = if f.value_range.is_empty() { "" } else { ", " };
        let at = f.value_range.end.max(f.key_range.end + 2);
        let lead = if f.value_range.is_empty() && !src[..at].ends_with(' ') { " " } else { "" };
        return Ok(Some(Edit { range: at..at, text: format!("{lead}{sep}{link}") }));
    }
    let key = field.trim();
    let body = ex.front_matter.as_ref().map_or(0, |f| f.range.end);
    let at = if let Some(last) = line_fields.last() {
        src[last.range.end..].find('\n').map_or(src.len(), |i| last.range.end + i + 1)
    } else {
        let rest = &src[body..];
        let first = rest.trim_start_matches(['\n', '\r']);
        let skipped = rest.len() - first.len();
        if first.starts_with("# ") { first.find('\n').map_or(src.len(), |i| body + skipped + i + 1) } else { body }
    };
    let pre = if at > 0 && !src[..at].ends_with('\n') { nl } else { "" };
    Ok(Some(Edit { range: at..at, text: format!("{pre}{key}:: {link}{nl}") }))
}

/// The edit that adds the YAML item `item` to an existing property whose
/// lines are `text` (a slice of `src`).
fn add_to_field(
    src: &str,
    text: &str,
    key: &str,
    item: &str,
    value: Option<&crate::frontmatter::Value>,
) -> Result<crate::frontmatter::Edit, &'static str> {
    use crate::frontmatter::{self, Edit, Value};
    // `text` is a slice of `src`: its offset there.
    let at = text.as_ptr() as usize - src.as_ptr() as usize;
    // Blank and comment lines after the value aren't part of it.
    let mut content = text.trim_end_matches(['\n', '\r']);
    while let Some(i) = content.rfind('\n') {
        let last = content[i + 1..].trim();
        if last.is_empty() || last.starts_with('#') {
            content = content[..i].trim_end_matches('\r');
        } else {
            break;
        }
    }
    let first_end = content.find('\n').unwrap_or(content.len());
    let first = content[..first_end].trim_end_matches('\r');
    let multiline = first_end < content.len();
    let (_, vfrom) = fm_key(first).ok_or("its line isn't `key: value`")?;
    let vfrom = vfrom.min(first.len());
    let vend = scan_value(&first[vfrom..]).comment.map_or(first.len(), |c| vfrom + c);
    let raw = &first[vfrom..vend];
    let vtext = raw.trim();
    let vstart = vfrom + raw.len() - raw.trim_start().len();
    if vtext.is_empty() {
        if !multiline {
            return Ok(Edit { range: at + vfrom..at + vfrom, text: format!(" {item}") });
        }
        return match value {
            Some(Value::List(_)) => {
                frontmatter::append_list_item(src, key, item).ok_or("it is a list written in a way that can't be extended")
            }
            _ => Err("it holds nested properties"),
        };
    }
    if vtext.starts_with('[') && !vtext.starts_with("[[") {
        // A flow list, maybe over several lines, maybe with comments inside.
        let open = at + vstart;
        let list = &src[open..at + content.len()];
        let close = scan(list, true).close.ok_or("its list isn't closed on a line the reader can follow")?;
        let before = list[..close].trim_end();
        let last_line = before.rfind('\n').map_or(0, |i| i + 1);
        if scan(&before[last_line..], false).comment.is_some() {
            return Err("its last item is followed by a comment");
        }
        let sep = match before.chars().next_back() {
            Some('[') => "",
            Some(',') => " ",
            _ => ", ",
        };
        let ins = open + before.len();
        return Ok(Edit { range: ins..ins, text: format!("{sep}{item}") });
    }
    if multiline || vtext.starts_with(['|', '>', '&', '*', '!']) {
        return Err("it is a multi-line text, block scalar, anchor or alias");
    }
    // One value on one line: make it a list of two, leaving its text and the comment alone.
    let quoted = vtext.len() >= 2
        && vtext.starts_with(['"', '\''])
        && vtext.ends_with(vtext.chars().next().unwrap_or('"'))
        && scan_value(vtext).quote.is_none();
    let old = if quoted { vtext.to_string() } else { flow_item(vtext) };
    Ok(Edit { range: at + vstart..at + vstart + vtext.len(), text: format!("[{old}, {item}]") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract;

    fn typed(src: &str) -> Vec<(Option<String>, String)> {
        extract(src).links.into_iter().map(|l| (l.field, l.target)).collect()
    }

    fn t(field: &str, target: &str) -> (Option<String>, String) {
        (Some(field.into()), target.into())
    }

    #[test]
    fn keys_normalize_like_dataview() {
        assert_eq!(normalize_key("Parent"), "parent");
        assert_eq!(normalize_key("**Basic  Field**"), "basic-field");
        assert_eq!(normalize_key(" see_also "), "see_also");
        assert_eq!(normalize_key("_Up_"), "up");
        assert_eq!(normalize_key("Связь"), "связь");
    }

    #[test]
    fn line_fields_type_their_links() {
        let src = "# N\nparent:: [[Rope]]\nchild:: [[A]], [[B|bee]]\n- up:: [[List Parent]]\n> same:: [[Quoted]]\nplain [[Untyped]]\n";
        assert_eq!(
            typed(src),
            vec![
                t("parent", "Rope"),
                t("child", "A"),
                t("child", "B"),
                t("up", "List Parent"),
                t("same", "Quoted"),
                (None, "Untyped".into())
            ]
        );
    }

    #[test]
    fn bracketed_and_paren_fields() {
        let src = "Claim [supports:: [[Theory]]] and (source:: [[Book#Ch 2]]) but [[Free]] and [md](Other.md).\n";
        assert_eq!(typed(src), vec![t("supports", "Theory"), t("source", "Book"), (None, "Free".into()), (None, "Other".into())]);
        let f = inline_fields(src);
        assert_eq!(f.len(), 2);
        assert_eq!(&src[f[0].range.clone()], "[supports:: [[Theory]]]");
        assert_eq!(&src[f[1].value_range.clone()], "[[Book#Ch 2]]");
        assert_eq!(f[1].style, FieldStyle::Paren);
    }

    #[test]
    fn tasks_need_brackets_and_code_is_skipped() {
        let src = "- [ ] up:: [[NotAField]] [project:: [[P]]]\n`up:: [[Code]]`\n```\nup:: [[Fenced]]\n```\nhttp://x.y::z\n";
        assert_eq!(typed(src), vec![(None, "NotAField".into()), t("project", "P")]);
    }

    #[test]
    fn markdown_links_in_fields() {
        assert_eq!(typed("next:: [Two](Two.md)\n"), vec![t("next", "Two")]);
    }

    #[test]
    fn front_matter_links_are_typed() {
        let src = "---\ntitle: X\nup: \"[[Parent]]\"\nrelated: [\"[[A]]\", \"[[B|b]]\"]\ndown:\n  - \"[[C]]\"\n  - '[[D#h]]'\n\"Odd Key\": \"[[E]]\"\ncover: \"![[pic.png]]\"\n---\nBody [[F]]\n";
        let links = extract(src).links;
        let got: Vec<_> = links.iter().map(|l| (l.field.clone(), l.target.clone())).collect();
        assert_eq!(
            got,
            vec![
                t("up", "Parent"),
                t("related", "A"),
                t("related", "B"),
                t("down", "C"),
                t("down", "D"),
                t("odd-key", "E"),
                t("cover", "pic.png"),
                (None, "F".into())
            ]
        );
        for l in &links {
            assert!(src[l.range.clone()].starts_with("[[") || src[l.range.clone()].starts_with("![["));
        }
        assert_eq!(links[6].kind, LinkKind::Embed);
    }

    #[test]
    fn adding_links_keeps_the_note_s_style() {
        let add = |src: &str, f: &str, t: &str| {
            let style = preferred_style(src);
            add_link(src, f, t, style).map(|e| e.apply(src))
        };
        // Front matter: a new field, then a list from a single link, then a flow list.
        assert_eq!(add("# A\nbody\n", "up", "Root").unwrap(), "---\nup: \"[[Root]]\"\n---\n# A\nbody\n");
        let one = "---\ntitle: A  # kept\nup: \"[[Root]]\"\n---\nbody\n";
        assert_eq!(add(one, "up", "Other").unwrap(), "---\ntitle: A  # kept\nup: [\"[[Root]]\", \"[[Other]]\"]\n---\nbody\n");
        let flow = "---\nrelated: [\"[[A]]\"]   # c\nz: 1\n---\n";
        assert_eq!(add(flow, "related", "B").unwrap(), "---\nrelated: [\"[[A]]\", \"[[B]]\"]   # c\nz: 1\n---\n");
        let block = "---\ndown:\n  - \"[[A]]\"\nz: 1\n---\n";
        assert_eq!(add(block, "Down", "B").unwrap(), "---\ndown:\n  - \"[[A]]\"\n  - \"[[B]]\"\nz: 1\n---\n");
        assert!(add(block, "down", "a").is_none(), "already linked");
        let bare = "---\nup: [[A]]\n---\n";
        assert_eq!(add(bare, "up", "B").unwrap(), "---\nup: [\"[[A]]\", \"[[B]]\"]\n---\n");
        // Inline: append to the field's line, else a new line after the others or the title.
        let inline = "# T\nparent:: [[A]]\nstatus:: draft\n\nText.\n";
        assert_eq!(add(inline, "parent", "B").unwrap(), "# T\nparent:: [[A]], [[B]]\nstatus:: draft\n\nText.\n");
        assert_eq!(add(inline, "supports", "C").unwrap(), "# T\nparent:: [[A]]\nstatus:: draft\nsupports:: [[C]]\n\nText.\n");
        let titled = "---\ntags: [x]\n---\n# T\r\nstatus:: x [[Q]]\r\n";
        assert_eq!(preferred_style(titled), FieldStyle::Line);
        assert_eq!(add(titled, "next", "N").unwrap(), "---\ntags: [x]\n---\n# T\r\nstatus:: x [[Q]]\r\nnext:: [[N]]\r\n");
        assert_eq!(add_link("# T", "up", "R", FieldStyle::Line).unwrap().apply("# T"), "# T\nup:: [[R]]\n");
        // Every edit reads back as the typed link it adds.
        for src in [one, flow, block, inline, titled] {
            let out = add(src, "see also", "Zed").unwrap();
            assert!(extract(&out).links.iter().any(|l| l.field.as_deref() == Some("see-also") && l.target == "Zed"), "{out}");
        }
    }

    #[test]
    fn yaml_quotes() {
        assert_eq!(yaml_quote_at(" [\"[[A's]]\", '"), Some('\''));
        assert_eq!(yaml_quote_at(" 'it''s "), Some('\''));
        assert_eq!(yaml_quote_at(" ['a', \"b\\\"c"), Some('"'));
        // An apostrophe inside a plain value opens nothing.
        assert_eq!(yaml_quote_at(" Bob's "), None);
        assert_eq!(yaml_quote_at(" a 'b "), None);
        assert_eq!(yaml_quote_at(" 'a' "), None);
        let links = extract("---\nup: [\"[[A's]]\", '[[B''s|x]]']\n---\n").links;
        assert_eq!(links.iter().map(|l| l.target.as_str()).collect::<Vec<_>>(), ["A's", "B's"]);
    }

    #[test]
    fn front_matter_links_skip_comments_and_code_spans() {
        let src = "---\nup: x # see [[Old]]\n# [[Whole line]]\nrel: \"[[Kept # not a comment]]\"  # [[Gone]]\ncode: \"`[[In code]]`\"\nlist:\n  - \"[[A]]\" # [[B]]\n  # [[C]]\n  - '[[D]]'\nlit: |\n  # [[Literal]] stays\n---\n";
        let got: Vec<_> = extract(src).links.into_iter().map(|l| l.target).collect();
        assert_eq!(got, ["Kept", "A", "D", "Literal"]);
        // A Markdown link in a property is not a typed link (docs/formats/links.md).
        assert!(typed("---\nup: \"[P](Projects.md)\"\n---\n").is_empty());
    }

    #[test]
    fn adding_a_link_keeps_comments_layout_and_nesting() {
        let add = |src: &str, f: &str, t: &str| try_add_link(src, f, t, FieldStyle::FrontMatter).map(|e| e.map(|e| e.apply(src)));
        // Trailing comment survives; the value keeps its own quotes.
        assert_eq!(
            add("---\nup: \"[[A]]\"   # parent\n---\n", "up", "X").unwrap().unwrap(),
            "---\nup: [\"[[A]]\", \"[[X]]\"]   # parent\n---\n"
        );
        assert_eq!(add("---\nup: 'a'  # c\n---\n", "up", "X").unwrap().unwrap(), "---\nup: ['a', \"[[X]]\"]  # c\n---\n");
        // A multi-line flow list stays multi-line.
        let multi = "---\nup: [\n  \"[[A]]\",   # first\n  \"[[B]]\"\n]\nz: 1\n---\n";
        assert_eq!(add(multi, "up", "X").unwrap().unwrap(), "---\nup: [\n  \"[[A]]\",   # first\n  \"[[B]]\", \"[[X]]\"\n]\nz: 1\n---\n");
        // Block scalars and the like are refused, not collapsed.
        assert!(add("---\nup: >\n  folded\n---\n", "up", "X").is_err());
        assert!(add("---\nup: &a x\n---\n", "up", "X").is_err());
        // A nested key's link doesn't become a top-level property of its own.
        let nested = "---\nmeta:\n  up: \"[[A]]\"\n---\n";
        assert_eq!(add(nested, "up", "X").unwrap().unwrap(), "---\nmeta:\n  up: \"[[A]]\"\nup: \"[[X]]\"\n---\n");
        // With the key twice, only the one being changed changes.
        let dup = "---\nup: \"[[A]]\"\nup: \"[[B]]\"\n---\n";
        let out = add(dup, "up", "X").unwrap().unwrap();
        assert!(out.contains("up: \"[[A]]\"\n") && !out.contains("[[A]]\", \"[[B]]"), "{out}");
        // A target with a quote or backslash is escaped, in every form.
        for src in ["---\nz: 1\n---\n", "---\nup: [\"[[A]]\"]\n---\n", "---\nup:\n  - \"[[A]]\"\n---\n", "---\nup: \"[[A]]\"\n---\n"] {
            let out = add(src, "up", "Say \"hi\" \\ there").unwrap().unwrap();
            assert_eq!(crate::frontmatter::yaml_valid(&out), Some(true), "{out}");
            assert!(extract(&out).links.iter().any(|l| l.target == "Say \"hi\" \\ there"), "{out}");
        }
    }

    #[test]
    fn only_dataview_keys_make_fields() {
        // Keys are words (as Dataview reads them); a link or a URL before `::` isn't one.
        assert!(typed("[[X]]:: y [[Z]]\n").iter().all(|l| l.0.is_none()));
        assert!(inline_fields("see http://x.y::z\n").is_empty());
        assert_eq!(typed("**Up**:: [[X]]\n"), vec![t("up", "X")]);
        assert_eq!(typed("🎅:: [[Gift]]\n"), vec![t("🎅", "Gift")]);
    }
}
