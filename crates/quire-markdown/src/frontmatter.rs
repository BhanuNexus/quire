// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A deliberately small YAML-subset front-matter reader and writer.
//!
//! Supports what note apps actually write: `key: value`, quoted strings,
//! inline lists `key: [a, b]` and maps `key: {a: 1}`, block lists (`key:`
//! then `- item` lines), nested maps (`author:` then indented `name: Ada`)
//! and block scalars (`key: |`). Lists of maps and other shapes are kept as
//! their raw text (shown read-only) rather than rejected.
//!
//! Writers change only the lines of the field they touch:
//! [`set_field_path`], [`remove_field_path`], [`rename_field_path`] and
//! [`edit_list_items`] leave every other byte of the note as it was.
//!
//! Where the block starts and ends is decided in one place, [`fences`]: any
//! code that needs to know where front matter ends calls it, so the reader,
//! the writers, the index, the linter and the spell checker always agree.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;

/// A front-matter value. In JSON (agent change sets, view queries) it is a
/// string, an array of strings or an object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Value {
    Str(String),
    List(Vec<String>),
    /// A nested map (`author:` then indented `name: Ada`, or `{name: Ada}`).
    Map(BTreeMap<String, Self>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            Self::List(_) | Self::Map(_) => None,
        }
    }

    /// Strings are treated as one-element lists; comma-separated strings are split.
    /// A map is not a list (empty).
    pub fn as_list(&self) -> Vec<String> {
        match self {
            Self::List(v) => v.clone(),
            Self::Str(s) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
            Self::Map(_) => Vec::new(),
        }
    }

    pub const fn as_map(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Map(m) => Some(m),
            Self::Str(_) | Self::List(_) => None,
        }
    }

    /// The value at a dotted path inside nested maps (`["author", "name"]`).
    pub fn at_path(&self, path: &[&str]) -> Option<&Self> {
        match path.split_first() {
            None => Some(self),
            Some((head, rest)) => self.as_map()?.get(*head)?.at_path(rest),
        }
    }

    /// Every leaf under this value with its dotted path relative to it
    /// (`("name", "Ada")`); a non-map is one leaf with an empty path.
    pub fn leaves(&self) -> Vec<(String, &Self)> {
        let mut out = Vec::new();
        walk_leaves("", self, &mut out);
        out
    }
}

fn walk_leaves<'a>(prefix: &str, v: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    match v {
        Value::Map(m) if !m.is_empty() => {
            for (k, child) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                walk_leaves(&p, child, out);
            }
        }
        _ => out.push((prefix.to_string(), v)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrontMatter {
    pub fields: BTreeMap<String, Value>,
    /// Byte range of the whole block including `---` fences.
    pub range: Range<usize>,
}

impl FrontMatter {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// A value by dotted path: `get_path("author.name")`.
    pub fn get_path(&self, dotted: &str) -> Option<&Value> {
        if let Some(v) = self.fields.get(dotted) {
            return Some(v);
        }
        let parts: Vec<&str> = dotted.split('.').collect();
        let (head, rest) = parts.split_first()?;
        self.fields.get(*head)?.at_path(rest)
    }

    pub fn title(&self) -> Option<&str> {
        self.get("title").and_then(Value::as_str)
    }

    pub fn tags(&self) -> Vec<String> {
        let mut v = Vec::new();
        for key in ["tags", "tag"] {
            if let Some(val) = self.get(key) {
                v.extend(val.as_list().into_iter().map(|t| t.trim_start_matches('#').to_string()));
            }
        }
        v
    }

    pub fn aliases(&self) -> Vec<String> {
        ["aliases", "alias"].iter().filter_map(|k| self.get(k)).flat_map(Value::as_list).collect()
    }

    /// The note's word-count goal (`goal: 1500`), shown in the status bar.
    /// `None` unless it is a positive number.
    pub fn word_goal(&self) -> Option<usize> {
        self.get("goal").and_then(Value::as_str).and_then(parse_goal)
    }
}

/// A word-count goal: a positive number, digits optionally grouped with
/// `,` or `_` (`1500`, `1,500`, `1500.0`), rounded to a whole word count.
/// Anything else (text, zero, negative, `inf`) is no goal.
pub fn parse_goal(s: &str) -> Option<usize> {
    let s: String = s.trim().chars().filter(|c| !matches!(c, ',' | '_')).collect();
    if !s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let n = s.parse::<f64>().ok().filter(|n| n.is_finite())?.round();
    // Below 2^52, so the cast is exact.
    (1.0..4_503_599_627_370_496.0).contains(&n).then_some(n as usize)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        unescape_double(&s[1..s.len() - 1])
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("''", "'")
    } else {
        s.to_string()
    }
}

/// The inside of a double-quoted scalar: YAML's escapes (`\\n`, `\\t`,
/// `\\"`, `\\uXXXX`, …). An unknown escape is kept as written.
fn unescape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let hex = |it: &mut std::iter::Peekable<std::str::Chars<'_>>, n: usize| -> Option<char> {
            let digits: String = it.clone().take(n).collect();
            let ch = (digits.len() == n).then(|| u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32)).flatten()?;
            for _ in 0..n {
                it.next();
            }
            Some(ch)
        };
        match it.peek().copied() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some(k @ ('x' | 'u' | 'U')) => {
                let mut ahead = it.clone();
                ahead.next();
                let width = match k {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                if let Some(ch) = hex(&mut ahead, width) {
                    out.push(ch);
                    it = ahead;
                } else {
                    // Not a valid escape: the backslash stays as written.
                    out.push('\\');
                }
                continue;
            }
            _ => {
                out.push('\\');
                continue;
            }
        }
        it.next();
    }
    out
}

// ------------------------------------------------------------------ reader

/// One line of the front-matter body.
#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    /// Byte offset of the line in the source.
    start: usize,
    /// Byte offset just after the line, its newline included.
    end: usize,
    /// Leading spaces (and tabs, which YAML forbids but people type).
    indent: usize,
    /// The line without indentation, trailing whitespace or newline.
    text: &'a str,
    /// The line without its newline.
    full: &'a str,
}

impl Line<'_> {
    fn is_blank_or_comment(&self) -> bool {
        self.text.is_empty() || self.text.starts_with('#')
    }

    fn is_item(&self) -> bool {
        self.text == "-" || self.text.starts_with("- ")
    }

    fn item_text(&self) -> &str {
        self.text.strip_prefix('-').unwrap_or(self.text).trim()
    }
}

/// How a field is written, so writers can keep its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `key: value`
    Scalar,
    /// `key: [a, b]`
    FlowList,
    /// `key: {a: 1}`
    FlowMap,
    /// `key:` then `- a` lines
    BlockList,
    /// `key:` then indented `k: v` lines
    BlockMap,
    /// `key: |` then text
    BlockScalar,
    /// A list of maps or deeper nesting, kept as text
    Raw,
    /// `key:` with nothing under it
    Empty,
}

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    /// Index of the key's line.
    line: usize,
    /// One past the last line that belongs to the field.
    end: usize,
    indent: usize,
    value: Value,
    shape: Shape,
    /// The fields of a block map.
    children: Vec<Self>,
    /// A flow value nested past [`MAX_NESTING`]: its deep part is kept as text.
    capped: bool,
}

impl Entry {
    /// Whether the value is text the reader could not take apart (a list of
    /// maps, or nesting past the limit): writers must not go through it.
    fn opaque(&self) -> bool {
        self.shape == Shape::Raw || self.capped
    }
}

/// An edit that would have to go through or rewrite a value kept as text.
///
/// Such a value (maps nested more than 64 deep, a list of maps) is not
/// read into fields, so writing a field inside it would re-emit it from
/// what was read and lose or change text. The note is left as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The dotted path asked for.
    pub path: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Can't change “{}”: it is inside a value this editor keeps as text (nested too deeply, or a list of maps). Edit it in the note's text; the note is unchanged.",
            self.path
        )
    }
}

impl std::error::Error for Refused {}

/// The parsed block with positions, for writers.
#[derive(Debug)]
struct Doc<'a> {
    lines: Vec<Line<'a>>,
    entries: Vec<Entry>,
    /// Byte offset of the closing fence line.
    close: usize,
    /// End of the block (after the closing fence and its newline).
    block_end: usize,
    nl: &'static str,
    /// Indentation step of nested maps (2 unless the file says otherwise).
    unit: usize,
}

/// Split `key: value`. `strict` requires `: ` or a trailing `:` (YAML);
/// otherwise a bare `key:value` also counts, as Quire has always read it.
fn split_key(text: &str, strict: bool) -> Option<(String, &str)> {
    if let Some(q) = text.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let close = text[1..].find(q)? + 1;
        let rest = text[close + 1..].trim_start();
        let val = rest.strip_prefix(':')?;
        return Some((unquote(&text[..=close]), val.trim()));
    }
    let at =
        text.find(": ").or_else(|| text.ends_with(':').then(|| text.len() - 1)).or_else(|| if strict { None } else { text.find(':') })?;
    let key = text[..at].trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), text[at + 1..].trim()))
}

/// Split a flow collection's inside at top-level commas, respecting quotes
/// and nested brackets. Returns trimmed parts with their byte ranges in `s`.
fn split_flow(s: &str) -> Vec<(Range<usize>, &str)> {
    fn push<'a>(s: &'a str, a: usize, b: usize, out: &mut Vec<(Range<usize>, &'a str)>) {
        let part = &s[a..b];
        let lead = part.len() - part.trim_start().len();
        let t = part.trim();
        if !t.is_empty() {
            out.push((a + lead..a + lead + t.len(), t));
        }
    }
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0);
    for (i, c) in s.char_indices() {
        match c {
            '"' | '\'' if quote == Some(c) => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(c),
            '[' | '{' if quote.is_none() => depth += 1,
            ']' | '}' if quote.is_none() => depth -= 1,
            ',' if quote.is_none() && depth == 0 => {
                push(s, start, i, &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(s, start, s.len(), &mut out);
    out
}

/// How deeply nested maps are read: deeper ones are kept as text. A note
/// from anywhere (a synced or shared vault, an import) must not be able to
/// exhaust the stack of whoever reads its properties.
const MAX_NESTING: usize = 64;

/// A value written on one line.
fn parse_inline(v: &str) -> (Value, Shape, bool) {
    let mut capped = false;
    let (value, shape) = parse_inline_at(v, 0, &mut capped);
    (value, shape, capped)
}

fn parse_inline_at(v: &str, depth: usize, capped: &mut bool) -> (Value, Shape) {
    let v = v.trim();
    if depth >= MAX_NESTING {
        *capped |= v.starts_with(['[', '{']);
        return (Value::Str(v.to_string()), Shape::Scalar);
    }
    if v.len() >= 2 && v.starts_with('[') && v.ends_with(']') {
        let items = split_flow(&v[1..v.len() - 1]).into_iter().map(|(_, s)| unquote(s)).filter(|s| !s.is_empty()).collect();
        return (Value::List(items), Shape::FlowList);
    }
    if v.len() >= 2 && v.starts_with('{') && v.ends_with('}') {
        let mut m = BTreeMap::new();
        for (_, part) in split_flow(&v[1..v.len() - 1]) {
            match split_key(part, false) {
                Some((k, val)) => {
                    m.insert(k, parse_inline_at(val, depth + 1, capped).0);
                }
                None => {
                    m.insert(unquote(part), Value::Str(String::new()));
                }
            }
        }
        return (Value::Map(m), Shape::FlowMap);
    }
    (Value::Str(unquote(v)), Shape::Scalar)
}

fn next_content(lines: &[Line<'_>], mut i: usize) -> Option<usize> {
    while i < lines.len() {
        if !lines[i].is_blank_or_comment() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A block list starting at `j`: plain items give a list; maps or nesting
/// keep the block's text. Returns the value, its shape and the end line.
fn parse_seq(lines: &[Line<'_>], j: usize) -> (Value, Shape, usize) {
    let item_indent = lines[j].indent;
    let (mut items, mut complex, mut k, mut last) = (Vec::new(), false, j, j);
    while k < lines.len() {
        let l = &lines[k];
        if l.is_blank_or_comment() {
            k += 1;
            continue;
        }
        if l.indent < item_indent || (l.indent == item_indent && !l.is_item()) {
            break;
        }
        if l.indent == item_indent {
            let t = l.item_text();
            let quoted = t.starts_with(['"', '\'']);
            if !quoted && (split_key(t, true).is_some() || t.starts_with(['[', '{', '|', '>', '-'])) {
                complex = true;
            }
            items.push(unquote(t));
        } else {
            complex = true;
        }
        last = k;
        k += 1;
    }
    if complex {
        let raw: Vec<&str> = lines[j..=last].iter().map(|l| l.full).collect();
        (Value::Str(raw.join("\n")), Shape::Raw, last + 1)
    } else {
        (Value::List(items), Shape::BlockList, last + 1)
    }
}

/// The fields at `indent` starting at line `i`. Returns them and the line
/// where the block ended.
fn parse_entries(lines: &[Line<'_>], i: usize, indent: usize) -> (Vec<Entry>, usize) {
    parse_entries_at(lines, i, indent, 0)
}

fn parse_entries_at(lines: &[Line<'_>], mut i: usize, indent: usize, depth: usize) -> (Vec<Entry>, usize) {
    let mut out = Vec::new();
    while i < lines.len() {
        let l = &lines[i];
        if l.is_blank_or_comment() {
            i += 1;
            continue;
        }
        if l.indent < indent {
            break;
        }
        let split = (l.indent == indent && !l.is_item()).then(|| split_key(l.text, false)).flatten();
        let Some((key, val)) = split else {
            // A stray deeper line or an item without a key: belongs to the field above.
            if let Some(prev) = out.last_mut() {
                let prev: &mut Entry = prev;
                prev.end = i + 1;
            }
            i += 1;
            continue;
        };
        let mut e = Entry {
            key,
            line: i,
            end: i + 1,
            indent,
            value: Value::Str(String::new()),
            shape: Shape::Scalar,
            children: Vec::new(),
            capped: false,
        };
        let block_scalar = matches!(val.trim_end_matches(|c: char| c.is_ascii_digit()), "|" | "|-" | "|+" | ">" | ">-" | ">+");
        if block_scalar {
            let (mut j, mut last) = (i + 1, i);
            while j < lines.len() && (lines[j].text.is_empty() || lines[j].indent > indent) {
                if !lines[j].text.is_empty() {
                    last = j;
                }
                j += 1;
            }
            let body = &lines[i + 1..=last.max(i)];
            let body = if last > i { body } else { &[] };
            let min = body.iter().filter(|l| !l.text.is_empty()).map(|l| l.indent).min().unwrap_or(0);
            let parts: Vec<&str> =
                body.iter().map(|l| if l.text.is_empty() { "" } else { l.full.get(min..).unwrap_or("").trim_end() }).collect();
            let joined = if val.starts_with('>') { parts.join(" ") } else { parts.join("\n") };
            e.value = Value::Str(joined.trim_end().to_string());
            e.shape = Shape::BlockScalar;
            e.end = last + 1;
            i = last + 1;
        } else if !val.is_empty() {
            (e.value, e.shape, e.capped) = parse_inline(val);
            i += 1;
        } else {
            match next_content(lines, i + 1) {
                Some(j) if lines[j].is_item() && lines[j].indent >= indent => {
                    let (v, shape, end) = parse_seq(lines, j);
                    (e.value, e.shape, e.end) = (v, shape, end);
                    i = end;
                }
                Some(j) if lines[j].indent > indent && depth + 1 >= MAX_NESTING => {
                    // Too deep to read as fields: the block's text, as written.
                    let mut last = j;
                    let mut k = j;
                    while k < lines.len() && (lines[k].is_blank_or_comment() || lines[k].indent > indent) {
                        if !lines[k].is_blank_or_comment() {
                            last = k;
                        }
                        k += 1;
                    }
                    let raw: Vec<&str> = lines[j..=last].iter().map(|l| l.full).collect();
                    (e.value, e.shape, e.end) = (Value::Str(raw.join("\n")), Shape::Raw, last + 1);
                    i = last + 1;
                }
                Some(j) if lines[j].indent > indent => {
                    let (children, next) = parse_entries_at(lines, j, lines[j].indent, depth + 1);
                    e.end = children.iter().map(|c| c.end).max().unwrap_or(j + 1);
                    let mut m = BTreeMap::new();
                    for c in &children {
                        m.insert(c.key.clone(), c.value.clone());
                    }
                    e.value = Value::Map(m);
                    e.shape = Shape::BlockMap;
                    e.children = children;
                    i = next.max(e.end);
                }
                _ => {
                    e.value = Value::List(Vec::new());
                    e.shape = Shape::Empty;
                    i += 1;
                }
            }
        }
        out.push(e);
    }
    (out, i)
}

/// Where a note's front matter is. Found by [`fences`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fences {
    /// The opening `---` line with its line break: `0..4`, or `0..5` with CRLF.
    pub open: Range<usize>,
    /// The closing line with its line break, if it has one.
    pub close: Range<usize>,
    /// The opening line's line break, which writers use inside the block.
    pub nl: &'static str,
}

impl Fences {
    /// The lines between the fences.
    pub const fn inner(&self) -> Range<usize> {
        self.open.end..self.close.start
    }

    /// Where the body starts: just after the closing line.
    pub const fn body_start(&self) -> usize {
        self.close.end
    }
}

/// Where the front matter at the start of `src` is: the one rule for every
/// reader and writer of front matter in Quire.
///
/// - **Opens:** the first line of the file is exactly `---`, ended by `\n`
///   or `\r\n`. Nothing may come before it or after it on the line: a
///   byte-order mark, a space, a blank line first or `---  ` means the note
///   has no front matter.
/// - **Closes:** at the first later line that is `---` or `...` once its
///   line break and any trailing white space are taken off. A line that is
///   indented (`  ---`) or longer (`----`, `--- x`) does not close it. The
///   closing line may be the last line, with no line break.
/// - **Unclosed:** with no closing line the note has no front matter, and
///   the whole file is body.
///
/// [`is_opening_line`] and [`is_closing_line`] are the same tests, for code
/// that reads one line at a time.
pub fn fences(src: &str) -> Option<Fences> {
    let open_end = src.find('\n')? + 1;
    let first = &src[..open_end];
    if !is_opening_line(first) {
        return None;
    }
    let nl = if first.ends_with("\r\n") { "\r\n" } else { "\n" };
    let mut pos = open_end;
    for line in src[open_end..].split_inclusive('\n') {
        if is_closing_line(line) {
            return Some(Fences { open: 0..open_end, close: pos..pos + line.len(), nl });
        }
        pos += line.len();
    }
    None
}

/// Whether `line`, a note's first line, opens front matter: `---` and its
/// line break (`\n` or `\r\n`), or a line given without its break. See
/// [`fences`].
pub fn is_opening_line(line: &str) -> bool {
    matches!(line, "---" | "---\n" | "---\r\n" | "---\r")
}

/// Whether `line`, inside front matter, closes it: `---` or `...` once its
/// line break and trailing white space are taken off. See [`fences`].
pub fn is_closing_line(line: &str) -> bool {
    matches!(line.trim_end(), "---" | "...")
}

fn parse_doc(src: &str) -> Option<Doc<'_>> {
    let f = fences(src)?;
    let lines: Vec<Line<'_>> = src[f.inner()]
        .split_inclusive('\n')
        .scan(f.open.end, |pos, raw| {
            let start = *pos;
            *pos += raw.len();
            let full = raw.trim_end_matches(['\n', '\r']);
            let t = full.trim_end();
            let indent = full.len() - full.trim_start_matches([' ', '\t']).len();
            Some(Line { start, end: start + raw.len(), indent, text: t.trim_start(), full })
        })
        .collect();
    let (entries, _) = parse_entries(&lines, 0, 0);
    let unit = entries.iter().find_map(|e| e.children.first().map(|c| c.indent - e.indent)).filter(|u| *u > 0).unwrap_or(2);
    Some(Doc { lines, entries, close: f.close.start, block_end: f.close.end, nl: f.nl, unit })
}

/// Parse front matter at the very start of `src`, if present.
pub fn parse(src: &str) -> Option<FrontMatter> {
    let doc = parse_doc(src)?;
    let mut fields = BTreeMap::new();
    for e in doc.entries {
        fields.insert(e.key, e.value);
    }
    Some(FrontMatter { fields, range: 0..doc.block_end })
}

/// Every top-level field in the order written, a key written twice included.
///
/// [`parse`] keeps the last of two fields with the same key. Code that must
/// fail safe on a repeated key (any `trust: untrusted` wins) reads them all
/// here. Empty without front matter.
pub fn fields_in_order(src: &str) -> Vec<(String, Value)> {
    parse_doc(src).map(|doc| doc.entries.into_iter().map(|e| (e.key, e.value)).collect()).unwrap_or_default()
}

// ------------------------------------------------------------------ writer

/// A front-matter edit: replace `range` (bytes of the source) with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

/// Inferred type of a property, for the properties panel and views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Date,
    Number,
    Bool,
    List,
    Text,
    /// Nested fields (`author.name`).
    Map,
}

impl Value {
    pub fn kind(&self) -> Kind {
        match self {
            Self::List(_) => Kind::List,
            Self::Str(s) => kind_of(s),
            Self::Map(_) => Kind::Map,
        }
    }

    /// One-line display form: lists joined with ", ", maps in YAML flow form.
    pub fn display(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::List(v) => v.join(", "),
            Self::Map(_) => self.flow(),
        }
    }

    /// YAML flow form: `a`, `[a, b]`, `{k: v, n: [x]}`.
    pub fn flow(&self) -> String {
        match self {
            Self::Str(s) => yaml_scalar(s),
            Self::List(items) => format!("[{}]", flow_items(items)),
            Self::Map(m) => {
                let parts: Vec<String> = m.iter().map(|(k, v)| format!("{}: {}", yaml_scalar(k), v.flow())).collect();
                format!("{{{}}}", parts.join(", "))
            }
        }
    }
}

pub fn kind_of(s: &str) -> Kind {
    let s = s.trim();
    if is_date(s) {
        Kind::Date
    } else if s.parse::<f64>().is_ok() {
        Kind::Number
    } else if matches!(s, "true" | "false") {
        Kind::Bool
    } else {
        Kind::Text
    }
}

/// `YYYY-MM-DD`, optionally followed by a time.
pub fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && (b.len() == 10 || matches!(b[10], b'T' | b' '))
}

/// Quote a scalar only when YAML would misread it.
///
/// A value with a line break or another control character is always
/// written on one line, double-quoted with escapes (`\\n`), so it can't
/// end the front matter or start another field. A value that starts or ends
/// with white space is quoted, so it reads back as it was given: a plain
/// scalar loses that white space.
pub fn yaml_scalar(s: &str) -> String {
    if s.chars().any(needs_escape) {
        let mut q = String::with_capacity(s.len() + 2);
        q.push('"');
        for c in s.chars() {
            match c {
                '\\' => q.push_str("\\\\"),
                '"' => q.push_str("\\\""),
                '\n' => q.push_str("\\n"),
                '\r' => q.push_str("\\r"),
                '\t' => q.push_str("\\t"),
                c if needs_escape(c) => {
                    let _ = std::fmt::Write::write_fmt(&mut q, format_args!("\\u{:04X}", u32::from(c)));
                }
                c => q.push(c),
            }
        }
        q.push('"');
        return q;
    }
    let risky = s.is_empty()
        || s.trim().len() != s.len()
        || s.contains(": ")
        || s.contains(" #")
        || s.starts_with(['#', '[', '{', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`', '-', '?'])
        || s.ends_with(':')
        || matches!(s.to_ascii_lowercase().as_str(), "yes" | "no" | "null" | "~");
    if risky { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) } else { s.to_string() }
}

/// The deepest block nesting [`load_yaml`] reads.
pub const MAX_YAML_DEPTH: usize = 64;

/// Whether `text` nests block collections no deeper than [`MAX_YAML_DEPTH`].
///
/// A cheap pass over the lines, with no recursion, made before the text goes
/// to `yaml-rust2`, which limits flow nesting (`[[[…`) but not block
/// nesting: `- - - …` or ever deeper indentation makes it, and the values it
/// builds, recurse until the stack is gone, and a stack overflow aborts the
/// whole app. Depth is the number of indentation levels in force plus the
/// `-`, `?` or `:` indicators that open a collection on the same line.
/// Block scalars and multi-line strings count like any other line, so a
/// pathological one is refused too: refusing is the safe side.
pub fn yaml_depth_ok(text: &str) -> bool {
    let mut stack: Vec<usize> = Vec::new();
    for line in text.lines() {
        let body = line.trim_start_matches([' ', '\t']);
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        if body == "---" || body == "..." {
            stack.clear();
            continue;
        }
        let indent = line.len() - body.len();
        while stack.last().is_some_and(|&t| t >= indent) {
            stack.pop();
        }
        stack.push(indent);
        let mut runs = 0usize;
        let mut rest = body;
        while let Some(tail) = rest.strip_prefix(['-', '?', ':']) {
            if !(tail.is_empty() || tail.starts_with([' ', '\t'])) {
                break;
            }
            runs += 1;
            rest = tail.trim_start_matches([' ', '\t']);
        }
        if stack.len() + runs.saturating_sub(1) > MAX_YAML_DEPTH {
            return false;
        }
    }
    true
}

/// Read `text` as YAML with `yaml-rust2`, after [`yaml_depth_ok`]. Every
/// place that hands user YAML to that parser goes through here.
pub fn load_yaml(text: &str) -> Result<Vec<yaml_rust2::Yaml>, String> {
    if !yaml_depth_ok(text) {
        return Err("it is nested too deeply to read".into());
    }
    #[expect(clippy::disallowed_methods, reason = "load_yaml is the one YAML parse, after the depth check above")]
    yaml_rust2::YamlLoader::load_from_str(text).map_err(|e| e.to_string())
}

/// Whether the front matter at the start of `src` is valid YAML, read by a
/// real YAML parser (the reader here is lenient). `None` without front matter.
pub fn yaml_valid(src: &str) -> Option<bool> {
    let fm = parse(src)?;
    let block = &src[..fm.range.end];
    let mut lines: Vec<&str> = block.split_inclusive('\n').collect();
    lines.remove(0);
    lines.pop();
    Some(load_yaml(&lines.concat()).is_ok())
}

/// Like [`yaml_scalar`], for a value that is text (a tag).
///
/// A word that YAML would read as a boolean, number, date or null is quoted,
/// and so is one with a character that ends an item in a flow list (`,` `[`
/// `]` `{` `}`).
pub fn yaml_text_scalar(s: &str) -> String {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let typed = matches!(lower.as_str(), "true" | "false" | "on" | "off" | "y" | "n" | "yes" | "no" | "null" | "~")
        || t.parse::<f64>().is_ok()
        || lower.starts_with("0x")
        || lower.starts_with("0o")
        || lower.starts_with(['.', '+', '-']) && lower[1..].starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == 'i' || c == 'n')
        || is_date(t)
        || t.contains([',', '[', ']', '{', '}']);
    if typed && !t.chars().any(needs_escape) { format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\"")) } else { yaml_scalar(t) }
}

/// Characters a scalar can't hold as they are: line breaks (including
/// Unicode's) and other control characters.
fn needs_escape(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}')
}

/// A key as it can be written: control characters become spaces.
fn clean_key(k: &str) -> String {
    k.chars().map(|c| if needs_escape(c) { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// `key: value` line(s) for a field, with trailing newline.
pub fn format_field(key: &str, value: &Value) -> String {
    match value {
        Value::Str(s) => format!("{key}: {}\n", yaml_scalar(s)),
        Value::List(items) => format!("{key}: [{}]\n", flow_items(items)),
        Value::Map(_) => format!("{key}: {}\n", value.flow()),
    }
}

/// A field at `indent` with nested maps in block form.
fn format_entry(key: &str, value: &Value, indent: usize, unit: usize, nl: &str) -> String {
    let pad = " ".repeat(indent);
    let key = yaml_scalar(key);
    match value {
        Value::Map(m) if !m.is_empty() => {
            let mut s = format!("{pad}{key}:{nl}");
            for (k, v) in m {
                s.push_str(&format_entry(k, v, indent + unit, unit, nl));
            }
            s
        }
        _ => format!("{pad}{}{nl}", format_field(&key, value).trim_end_matches('\n')),
    }
}

/// `a:` / `  b:` / `    c: value` for a path that doesn't exist yet.
fn format_chain(path: &[&str], value: &Value, indent: usize, unit: usize, nl: &str) -> String {
    match path {
        [] => String::new(),
        [leaf] => format_entry(leaf, value, indent, unit, nl),
        [head, rest @ ..] => {
            format!("{}{}:{nl}{}", " ".repeat(indent), yaml_scalar(head), format_chain(rest, value, indent + unit, unit, nl))
        }
    }
}

/// List items for a flow list, quoting any that contain a comma.
fn flow_items(items: &[String]) -> String {
    items.iter().map(|i| flow_item(i)).collect::<Vec<_>>().join(", ")
}

fn flow_item(i: &str) -> String {
    let q = yaml_scalar(i);
    if (q.contains(',') || q.contains(['[', ']', '{', '}'])) && !q.starts_with('"') { format!("\"{q}\"") } else { q }
}

/// Bytes of an entry's lines.
fn entry_range(doc: &Doc<'_>, e: &Entry) -> Range<usize> {
    doc.lines[e.line].start..doc.lines[e.end - 1].end
}

/// Follow `path` down the block. Returns the entries on the way (the last is the leaf).
fn find_path<'d>(doc: &'d Doc<'_>, path: &[&str]) -> Option<Vec<&'d Entry>> {
    let mut entries = &doc.entries;
    let mut chain = Vec::new();
    for (depth, key) in path.iter().enumerate() {
        let e = entries.iter().rev().find(|e| e.key == *key)?;
        chain.push(e);
        if depth + 1 < path.len() {
            if e.shape != Shape::BlockMap {
                return None;
            }
            entries = &e.children;
        }
    }
    Some(chain)
}

/// Replace `range` with `text`, keeping a missing final newline missing.
fn replace_lines(src: &str, range: Range<usize>, text: String, nl: &str) -> Edit {
    let text = if src[range.clone()].ends_with('\n') { text } else { text.trim_end_matches(nl).to_string() };
    Edit { range, text }
}

/// Set (add or replace) a top-level field, preserving everything else in the block.
pub fn set_field(src: &str, key: &str, value: &Value) -> Edit {
    let key = clean_key(key);
    let key = key.as_str();
    match parse_doc(src) {
        None => Edit { range: 0..0, text: format!("---\n{}---\n", format_field(key, value)) },
        Some(doc) => match doc.entries.iter().rev().find(|e| e.key == key) {
            Some(e) => {
                let line = format_field(key, value).replace('\n', doc.nl);
                replace_lines(src, entry_range(&doc, e), line, doc.nl)
            }
            None => Edit { range: doc.close..doc.close, text: format_field(key, value).replace('\n', doc.nl) },
        },
    }
}

/// Insert `key: value` into a map in place.
fn set_in_map(m: &mut BTreeMap<String, Value>, path: &[&str], value: Value) {
    match path {
        [] => {}
        [leaf] => {
            m.insert((*leaf).to_string(), value);
        }
        [head, rest @ ..] => {
            let child = m.entry((*head).to_string()).or_insert_with(|| Value::Map(BTreeMap::new()));
            if !matches!(child, Value::Map(_)) {
                *child = Value::Map(BTreeMap::new());
            }
            if let Value::Map(cm) = child {
                set_in_map(cm, rest, value);
            }
        }
    }
}

/// Set a field at a path (`["author", "name"]`), creating missing parents.
///
/// Only the leaf's line or lines change (or, when a parent is written
/// inline as `{…}`, that parent's line); every other byte stays.
///
/// [`Refused`] when the path runs through a value the reader keeps as text
/// (maps nested past 64 levels, a list of maps): writing inside it would
/// rewrite that text from what was read. Replacing such an entry as a whole
/// (a path that ends at it) is allowed.
pub fn set_field_path(src: &str, path: &[&str], value: &Value) -> Result<Edit, Refused> {
    if path.iter().any(|k| k.chars().any(needs_escape)) {
        let clean: Vec<String> = path.iter().map(|k| clean_key(k)).collect();
        return set_field_path(src, &clean.iter().map(String::as_str).collect::<Vec<_>>(), value);
    }
    if path.len() <= 1 {
        return Ok(set_field(src, path.first().copied().unwrap_or_default(), value));
    }
    let Some(doc) = parse_doc(src) else {
        return Ok(Edit { range: 0..0, text: format!("---\n{}---\n", format_chain(path, value, 0, 2, "\n")) });
    };
    let (nl, unit) = (doc.nl, doc.unit);
    let mut entries = &doc.entries;
    let (mut insert_at, mut indent) = (doc.close, 0);
    for (depth, key) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        let Some(e) = entries.iter().rev().find(|e| e.key == *key) else {
            return Ok(Edit { range: insert_at..insert_at, text: format_chain(&path[depth..], value, indent, unit, nl) });
        };
        if last {
            return Ok(replace_lines(src, entry_range(&doc, e), format_entry(key, value, e.indent, unit, nl), nl));
        }
        if e.shape == Shape::BlockMap {
            indent = e.children.first().map_or(e.indent + unit, |c| c.indent);
            insert_at = doc.lines[e.end - 1].end;
            entries = &e.children;
            continue;
        }
        // Text kept as it was written can't be rewritten from what was read.
        if e.opaque() {
            return Err(Refused { path: path.join(".") });
        }
        // The parent is written inline or isn't a map yet: rewrite it whole.
        let mut merged = match &e.value {
            Value::Map(m) => m.clone(),
            _ => BTreeMap::new(),
        };
        set_in_map(&mut merged, &path[depth + 1..], value.clone());
        let merged = Value::Map(merged);
        let text = if e.shape == Shape::FlowMap {
            format!("{}{}: {}{nl}", " ".repeat(e.indent), yaml_scalar(key), merged.flow())
        } else {
            format_entry(key, &merged, e.indent, unit, nl)
        };
        return Ok(replace_lines(src, entry_range(&doc, e), text, nl));
    }
    Ok(Edit { range: insert_at..insert_at, text: String::new() })
}

/// Remove a top-level field; `None` when it isn't there. Removing the last
/// field removes the whole block.
pub fn remove_field(src: &str, key: &str) -> Option<Edit> {
    remove_field_path(src, &[key.trim()]).ok().flatten()
}

/// Remove a field at a path; `None` when it isn't there. A nested map left
/// empty is written `key: {}`; removing the last top-level field removes the
/// whole block.
///
/// [`Refused`] when the path runs through text the reader keeps as it was
/// written (see [`set_field_path`]); the note is left alone.
pub fn remove_field_path(src: &str, path: &[&str]) -> Result<Option<Edit>, Refused> {
    let Some(doc) = parse_doc(src) else { return Ok(None) };
    let (nl, unit) = (doc.nl, doc.unit);
    // Inside an inline map: rewrite that line.
    for depth in 0..path.len() {
        let Some(chain) = find_path(&doc, &path[..=depth]) else { return Ok(None) };
        let e = chain[depth];
        if depth + 1 < path.len() && e.shape != Shape::BlockMap {
            if e.opaque() {
                return Err(Refused { path: path.join(".") });
            }
            let Value::Map(mut m) = e.value.clone() else { return Ok(None) };
            if remove_in_map(&mut m, &path[depth + 1..]).is_none() {
                return Ok(None);
            }
            let v = Value::Map(m);
            let text = if e.shape == Shape::FlowMap {
                format!("{}{}: {}{nl}", " ".repeat(e.indent), yaml_scalar(&e.key), v.flow())
            } else {
                format_entry(&e.key, &v, e.indent, unit, nl)
            };
            return Ok(Some(replace_lines(src, entry_range(&doc, e), text, nl)));
        }
    }
    let Some(chain) = find_path(&doc, path) else { return Ok(None) };
    let Some(&leaf) = chain.last() else { return Ok(None) };
    if chain.len() == 1 {
        let distinct: std::collections::BTreeSet<&str> = doc.entries.iter().map(|e| e.key.as_str()).collect();
        if distinct.len() == 1 {
            return Ok(Some(Edit { range: 0..doc.block_end, text: String::new() }));
        }
        return Ok(Some(Edit { range: entry_range(&doc, leaf), text: String::new() }));
    }
    let parent = chain[chain.len() - 2];
    if parent.children.iter().all(|c| c.key == leaf.key) {
        let text = format!("{}{}: {{}}{nl}", " ".repeat(parent.indent), yaml_scalar(&parent.key));
        return Ok(Some(replace_lines(src, entry_range(&doc, parent), text, nl)));
    }
    Ok(Some(Edit { range: entry_range(&doc, leaf), text: String::new() }))
}

fn remove_in_map(m: &mut BTreeMap<String, Value>, path: &[&str]) -> Option<()> {
    match path {
        [] => None,
        [leaf] => m.remove(*leaf).map(|_| ()),
        [head, rest @ ..] => match m.get_mut(*head)? {
            Value::Map(cm) => remove_in_map(cm, rest),
            _ => None,
        },
    }
}

/// Rename the key at `path` to `new_key`, keeping its value's bytes.
/// `None` when the field isn't there; [`Refused`] as for [`set_field_path`].
pub fn rename_field_path(src: &str, path: &[&str], new_key: &str) -> Result<Option<Edit>, Refused> {
    let Some(doc) = parse_doc(src) else { return Ok(None) };
    let new_key = clean_key(new_key);
    let new_key = new_key.as_str();
    for depth in 0..path.len() {
        let Some(chain) = find_path(&doc, &path[..=depth]) else { return Ok(None) };
        let e = chain[depth];
        if depth + 1 < path.len() && e.shape != Shape::BlockMap {
            if e.opaque() {
                return Err(Refused { path: path.join(".") });
            }
            // Inside an inline map: rewrite its line.
            let Value::Map(mut m) = e.value.clone() else { return Ok(None) };
            if rename_in_map(&mut m, &path[depth + 1..], new_key).is_none() {
                return Ok(None);
            }
            let text = format!("{}{}: {}{}", " ".repeat(e.indent), yaml_scalar(&e.key), Value::Map(m).flow(), doc.nl);
            return Ok(Some(replace_lines(src, entry_range(&doc, e), text, doc.nl)));
        }
    }
    Ok(rename_leaf(src, &doc, path, new_key))
}

fn rename_leaf(src: &str, doc: &Doc<'_>, path: &[&str], new_key: &str) -> Option<Edit> {
    let leaf = *find_path(doc, path)?.last()?;
    let line = &doc.lines[leaf.line];
    let key_start = line.start + leaf.indent;
    let body = &src[key_start..line.end];
    let key_len = if body.starts_with(['"', '\'']) {
        let q = body.chars().next().unwrap_or('"');
        body[1..].find(q).map_or(0, |i| i + 2)
    } else {
        body.find(": ").or_else(|| body.trim_end().ends_with(':').then(|| body.trim_end().len() - 1)).or_else(|| body.find(':'))?
    };
    let key_text = &src[key_start..key_start + key_len];
    let trailing = &key_text[key_text.trim_end().len()..];
    Some(Edit { range: key_start..key_start + key_len, text: format!("{}{trailing}", yaml_scalar(new_key)) })
}

fn rename_in_map(m: &mut BTreeMap<String, Value>, path: &[&str], new_key: &str) -> Option<()> {
    match path {
        [] => None,
        [leaf] => {
            let v = m.remove(*leaf)?;
            m.insert(new_key.to_string(), v);
            Some(())
        }
        [head, rest @ ..] => match m.get_mut(*head)? {
            Value::Map(cm) => rename_in_map(cm, rest, new_key),
            _ => None,
        },
    }
}

/// What to do with one list item in [`edit_list_items`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemChange {
    Keep,
    Replace(String),
    Remove,
}

/// Change the items of a list field (block list, `[a, b]`, or `a, b`)
/// without touching other items' bytes. `f` sees each item unquoted.
/// `None` when the field isn't there or nothing changed.
pub fn edit_list_items(src: &str, path: &[&str], mut f: impl FnMut(&str) -> ItemChange) -> Option<Edit> {
    let doc = parse_doc(src)?;
    let e = *find_path(&doc, path)?.last()?;
    let nl = doc.nl;
    let mut changed = false;
    match e.shape {
        Shape::BlockList => {
            let mut out = String::new();
            let mut kept = 0;
            let item_indent = doc.lines[e.line + 1..e.end].iter().find(|l| l.is_item()).map_or(e.indent, |l| l.indent);
            for (i, l) in doc.lines[e.line..e.end].iter().enumerate() {
                let raw = &src[l.start..l.end];
                if i == 0 || !l.is_item() || l.indent != item_indent {
                    out.push_str(raw);
                    continue;
                }
                match f(&unquote(l.item_text())) {
                    ItemChange::Keep => {
                        kept += 1;
                        out.push_str(raw);
                    }
                    ItemChange::Replace(s) => {
                        changed = true;
                        kept += 1;
                        let ending = &raw[l.full.len()..];
                        let dash = l.full.find('-').unwrap_or(0);
                        let _ = write!(out, "{}- {}{ending}", &l.full[..dash], yaml_scalar(&s));
                    }
                    ItemChange::Remove => changed = true,
                }
            }
            if !changed {
                return None;
            }
            if kept == 0 {
                let text = format!("{}{}: []{nl}", " ".repeat(e.indent), yaml_scalar(&e.key));
                return Some(replace_lines(src, entry_range(&doc, e), text, nl));
            }
            Some(Edit { range: entry_range(&doc, e), text: out })
        }
        Shape::FlowList | Shape::Scalar => {
            let line = &doc.lines[e.line];
            let text = line.full;
            let colon = split_key_offset(text)?;
            let after = &text[colon + 1..];
            let value_off = colon + 1 + (after.len() - after.trim_start().len());
            let value = text[value_off..].trim_end();
            let (inner_off, inner) =
                if e.shape == Shape::FlowList { (value_off + 1, &value[1..value.len() - 1]) } else { (value_off, value) };
            let parts: Vec<(Range<usize>, &str)> = if e.shape == Shape::FlowList {
                split_flow(inner)
            } else {
                let mut v = Vec::new();
                let mut at = 0;
                for p in inner.split(',') {
                    let lead = p.len() - p.trim_start().len();
                    let t = p.trim();
                    if !t.is_empty() {
                        v.push((at + lead..at + lead + t.len(), t));
                    }
                    at += p.len() + 1;
                }
                v
            };
            let mut items: Vec<String> = Vec::new();
            for (_, p) in &parts {
                match f(&unquote(p)) {
                    ItemChange::Keep => items.push((*p).to_string()),
                    ItemChange::Replace(s) => {
                        changed = true;
                        items.push(if e.shape == Shape::FlowList { flow_item(&s) } else { yaml_scalar(&s) });
                    }
                    ItemChange::Remove => changed = true,
                }
            }
            if !changed {
                return None;
            }
            let new_inner = items.join(", ");
            let (from, to) = (line.start + inner_off, line.start + inner_off + inner.len());
            if e.shape == Shape::Scalar && items.is_empty() {
                let text = format!("{}{}: []{nl}", " ".repeat(e.indent), yaml_scalar(&e.key));
                return Some(replace_lines(src, entry_range(&doc, e), text, nl));
            }
            Some(Edit { range: from..to, text: new_inner })
        }
        _ => None,
    }
}

/// `- first line` then the rest of `item`'s lines lined up under it, at `indent`.
fn block_item(item: &str, indent: usize, nl: &str) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    for (i, l) in item.lines().enumerate() {
        let lead = if i == 0 { format!("{pad}- ") } else { format!("{pad}  ") };
        let _ = write!(out, "{lead}{l}{nl}");
    }
    out
}

/// Add one item to the end of a list field, as a block list item (`- …`).
///
/// `item` may span lines (`start: …` / `end: …` for a list of maps): the
/// first line follows `- ` and the others line up under it. Existing items
/// keep their bytes; a missing field, an empty one (`key:` or `key: []`) or
/// a flow list (`key: [a, b]`) is written as a block list. `None` when the
/// field holds something other than a list.
pub fn append_list_item(src: &str, key: &str, item: &str) -> Option<Edit> {
    let key = clean_key(key);
    let Some(doc) = parse_doc(src) else {
        return Some(Edit { range: 0..0, text: format!("---\n{}:\n{}---\n", yaml_scalar(&key), block_item(item, 2, "\n")) });
    };
    let nl = doc.nl;
    let Some(e) = doc.entries.iter().rev().find(|e| e.key == key) else {
        return Some(Edit { range: doc.close..doc.close, text: format!("{}:{nl}{}", yaml_scalar(&key), block_item(item, doc.unit, nl)) });
    };
    let range = entry_range(&doc, e);
    match e.shape {
        Shape::BlockList | Shape::Raw => {
            let indent = doc.lines[e.line + 1..e.end].iter().find(|l| l.is_item()).map_or(e.indent + doc.unit, |l| l.indent);
            // After the last line that belongs to the list (blank lines stay where they are).
            let last = doc.lines[e.line..e.end].iter().rev().find(|l| !l.is_blank_or_comment()).map_or(range.end, |l| l.end);
            let text = block_item(item, indent, nl);
            let text = if src[..last].ends_with('\n') { text } else { format!("{nl}{text}") };
            Some(Edit { range: last..last, text })
        }
        Shape::Empty | Shape::FlowList | Shape::Scalar => {
            let items: Vec<String> = match (&e.shape, &e.value) {
                (Shape::FlowList, _) => {
                    let line = doc.lines[e.line].text;
                    let v = line[split_key_offset(line)? + 1..].trim();
                    split_flow(&v[1..v.len() - 1]).into_iter().map(|(_, s)| s.to_string()).collect()
                }
                (Shape::Scalar, Value::Str(s)) if !s.trim().is_empty() => return None,
                _ => Vec::new(),
            };
            let indent = e.indent + doc.unit;
            let mut text = format!("{}{}:{nl}", " ".repeat(e.indent), yaml_scalar(&e.key));
            for i in items {
                text.push_str(&block_item(&i, indent, nl));
            }
            text.push_str(&block_item(item, indent, nl));
            Some(replace_lines(src, range, text, nl))
        }
        Shape::FlowMap | Shape::BlockMap | Shape::BlockScalar => None,
    }
}

/// The raw text of every line of a field (`key:` and what belongs to it), for
/// shapes the reader keeps as text (lists of maps). `None` when it isn't there.
pub fn field_text<'a>(src: &'a str, key: &str) -> Option<&'a str> {
    let doc = parse_doc(src)?;
    let e = doc.entries.iter().rev().find(|e| e.key == key)?;
    Some(&src[entry_range(&doc, e)])
}

/// Byte offset of the `:` after a line's key.
fn split_key_offset(text: &str) -> Option<usize> {
    let t = text.trim_start();
    let lead = text.len() - t.len();
    if t.starts_with(['"', '\'']) {
        let q = t.chars().next()?;
        let close = t[1..].find(q)? + 1;
        return t[close + 1..].find(':').map(|i| lead + close + 1 + i);
    }
    t.find(": ").or_else(|| t.ends_with(':').then(|| t.len() - 1)).or_else(|| t.find(':')).map(|i| lead + i)
}

/// Every key path in the block, depth first (`["author"]`, `["author", "name"]`).
pub fn key_paths(src: &str) -> Vec<Vec<String>> {
    fn walk(prefix: &[String], entries: &[Entry], out: &mut Vec<Vec<String>>) {
        for e in entries {
            let mut p = prefix.to_vec();
            p.push(e.key.clone());
            out.push(p.clone());
            walk(&p, &e.children, out);
        }
    }
    let mut out = Vec::new();
    if let Some(doc) = parse_doc(src) {
        walk(&[], &doc.entries, &mut out);
    }
    out
}

impl Edit {
    pub fn apply(&self, src: &str) -> String {
        let mut s = src.to_string();
        s.replace_range(self.range.clone(), &self.text);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_with_edge_spaces_read_back_as_given() {
        // D5: " A" was written `a: A` and read back as "A"; it is quoted now.
        for v in [" A", "A ", "  ", " a: b ", "\u{a0}x"] {
            let out = set_field("body\n", "a", &Value::Str(v.into())).apply("body\n");
            assert_eq!(parse(&out).unwrap().get("a"), Some(&Value::Str(v.into())), "{v:?} -> {out:?}");
        }
        assert_eq!(set_field("", "a", &Value::Str(" A".into())).apply(""), "---\na: \" A\"\n---\n");
        // Values without edge spaces are written as before.
        assert_eq!(yaml_scalar("A"), "A");
        assert_eq!(yaml_scalar("x: y"), "\"x: y\"");
    }

    #[test]
    fn fields_in_order_keeps_repeated_keys() {
        let src = "---\ntrust: untrusted\nname: x\ntrust: trusted\n...\nbody\n";
        let all = fields_in_order(src);
        let keys: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["trust", "name", "trust"]);
        assert_eq!(all[0].1, Value::Str("untrusted".into()));
        // `parse` keeps the last one.
        assert_eq!(parse(src).unwrap().get("trust"), Some(&Value::Str("trusted".into())));
        assert!(fields_in_order("no front matter").is_empty());
    }

    /// A hostile note (synced, shared or imported) nests maps tens of
    /// thousands deep: reading its properties must neither exhaust the
    /// stack nor take quadratic time. Deep parts are kept as text.
    #[test]
    fn deeply_nested_properties_are_read_without_exhausting_the_stack() {
        let flow = format!("---\ntitle: ok\nk: {}x{}\n---\nbody\n", "{a: ".repeat(100_000), "}".repeat(100_000));
        let mut block = String::from("---\ntitle: ok\n");
        for d in 0..1500 {
            block.push_str(&" ".repeat(d));
            block.push_str("k:\n");
        }
        block.push_str(&" ".repeat(1500));
        block.push_str("leaf: 1\n---\nbody\n");
        let started = std::time::Instant::now();
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                for src in [flow, block] {
                    let fm = parse(&src).unwrap();
                    assert_eq!(fm.fields.get("title"), Some(&Value::Str("ok".into())));
                    assert!(matches!(fm.fields.get("k"), Some(Value::Map(_))));
                    assert_eq!(&src[fm.range.end..], "body\n");
                    let _ = fm.fields["k"].flow();
                }
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        // Ordinary nesting is unchanged.
        let fm = parse("---\na:\n  b:\n    c: 1\nd: {e: {f: 2}}\n---\n").unwrap();
        assert_eq!(fm.fields["a"].flow(), "{b: {c: 1}}");
        assert_eq!(fm.fields["d"].flow(), "{e: {f: 2}}");
    }

    #[test]
    fn parses_common_shapes() {
        let src =
            "---\ntitle: \"Rope notes\"\ntags: [rust, perf]\naliases:\n  - ropes\n  - 'text buffers'\ncreated: 2026-11-01\n---\n# Body\n";
        let fm = parse(src).unwrap();
        assert_eq!(fm.title(), Some("Rope notes"));
        assert_eq!(fm.tags(), vec!["rust", "perf"]);
        assert_eq!(fm.aliases(), vec!["ropes", "text buffers"]);
        assert_eq!(fm.get("created").and_then(Value::as_str), Some("2026-11-01"));
        assert_eq!(&src[fm.range.end..], "# Body\n");
    }

    #[test]
    fn absent_or_unterminated() {
        assert!(parse("# no front matter").is_none());
        assert!(parse("---\ntitle: x\n").is_none());
    }

    #[test]
    fn set_and_remove_fields() {
        let src = "---\ntitle: Plan\ntags:\n  - a\n  - b\nstatus: todo\n---\nbody\n";
        let e = set_field(src, "status", &Value::Str("done".into()));
        assert_eq!(e.apply(src), "---\ntitle: Plan\ntags:\n  - a\n  - b\nstatus: done\n---\nbody\n");
        let e = set_field(src, "tags", &Value::List(vec!["x".into(), "y, z".into()]));
        assert_eq!(e.apply(src), "---\ntitle: Plan\ntags: [x, \"y, z\"]\nstatus: todo\n---\nbody\n");
        let e = set_field(src, "due", &Value::Str("2026-11-05".into()));
        assert_eq!(e.apply(src), "---\ntitle: Plan\ntags:\n  - a\n  - b\nstatus: todo\ndue: 2026-11-05\n---\nbody\n");
        let e = remove_field(src, "tags").unwrap();
        assert_eq!(e.apply(src), "---\ntitle: Plan\nstatus: todo\n---\nbody\n");
        assert!(remove_field(src, "nope").is_none());
        assert_eq!(set_field("body", "k", &Value::Str("Plan: v2".into())).apply("body"), "---\nk: \"Plan: v2\"\n---\nbody");
        assert_eq!(remove_field("---\nk: v\n---\nbody", "k").unwrap().apply("---\nk: v\n---\nbody"), "body");
        // Round trip through the parser.
        for tricky in ["a: b # c", "say \"hi\": now", "back\\slash: x"] {
            let out = set_field(src, "note", &Value::Str(tricky.into())).apply(src);
            assert_eq!(parse(&out).unwrap().get("note").and_then(Value::as_str), Some(tricky));
        }
    }

    #[test]
    fn appends_list_items_keeping_other_bytes() {
        let entry = "start: 2026-11-06T10:00:00+01:00\nend: 2026-11-06T10:30:00+01:00";
        // A list of maps (kept as text by the reader) gets one more item.
        let src = "---\ntype: task\ntime_entries:\n    - start: 2026-11-05T09:00:00+01:00   # morning\n      end: 2026-11-05T09:20:00+01:00\ndue: 2026-11-30\n---\nbody\n";
        let out = append_list_item(src, "time_entries", entry).unwrap().apply(src);
        assert_eq!(
            out,
            "---\ntype: task\ntime_entries:\n    - start: 2026-11-05T09:00:00+01:00   # morning\n      end: 2026-11-05T09:20:00+01:00\n    - start: 2026-11-06T10:00:00+01:00\n      end: 2026-11-06T10:30:00+01:00\ndue: 2026-11-30\n---\nbody\n"
        );
        // Missing: a new block list before the closing fence.
        let src = "---\ntype: task\n---\nbody\n";
        let out = append_list_item(src, "time_entries", entry).unwrap().apply(src);
        assert_eq!(
            out,
            "---\ntype: task\ntime_entries:\n  - start: 2026-11-06T10:00:00+01:00\n    end: 2026-11-06T10:30:00+01:00\n---\nbody\n"
        );
        // Empty and flow lists become block lists; CRLF stays CRLF.
        let src = "---\r\ntags: []\r\nk: v\r\n---\r\n";
        assert_eq!(append_list_item(src, "tags", "a").unwrap().apply(src), "---\r\ntags:\r\n  - a\r\nk: v\r\n---\r\n");
        let src = "---\ntags: [x, \"y, z\"]\n---\n";
        assert_eq!(append_list_item(src, "tags", "a").unwrap().apply(src), "---\ntags:\n  - x\n  - \"y, z\"\n  - a\n---\n");
        let src = "---\ntags:\n  - x\n\n# c\nk: v\n---\n";
        assert_eq!(append_list_item(src, "tags", "a").unwrap().apply(src), "---\ntags:\n  - x\n  - a\n\n# c\nk: v\n---\n");
        assert!(append_list_item("---\nk: text\n---\n", "k", "a").is_none());
        assert_eq!(append_list_item("body", "k", "a").unwrap().apply("body"), "---\nk:\n  - a\n---\nbody");
        assert_eq!(field_text("---\na: 1\nb:\n  - x\n---\n", "b"), Some("b:\n  - x\n"));
    }

    #[test]
    fn word_goals() {
        let goal = |src: &str| parse(src).and_then(|fm| fm.word_goal());
        assert_eq!(goal("---\ngoal: 1500\n---\n"), Some(1500));
        assert_eq!(goal("---\ngoal: \"1,500\"\n---\n"), Some(1500));
        assert_eq!(goal("---\ngoal: 2_000\n---\n"), Some(2000));
        assert_eq!(goal("---\ngoal: 750.4\n---\n"), Some(750));
        assert_eq!(goal("---\ntitle: x\n---\n"), None);
        for bad in ["0", "-5", "0.2", "lots", "inf", "NaN", "1e3", "", "[1500]"] {
            assert_eq!(goal(&format!("---\ngoal: {bad}\n---\n")), None, "{bad}");
        }
        assert_eq!(goal("---\ngoal:\n  - 1500\n---\n"), None, "a list is no goal");
        assert_eq!(parse_goal(" 640 "), Some(640));
    }

    #[test]
    fn kinds() {
        assert_eq!(Value::Str("2026-11-01".into()).kind(), Kind::Date);
        assert_eq!(Value::Str("3.5".into()).kind(), Kind::Number);
        assert_eq!(Value::Str("true".into()).kind(), Kind::Bool);
        assert_eq!(Value::Str("hello".into()).kind(), Kind::Text);
        assert_eq!(Value::List(vec![]).kind(), Kind::List);
        assert_eq!(Value::Map(BTreeMap::new()).kind(), Kind::Map);
    }

    #[test]
    fn values_as_json() {
        let v = Value::Map(BTreeMap::from([("a".to_string(), Value::List(vec!["x".into()])), ("b".to_string(), Value::Str("y".into()))]));
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#"{"a":["x"],"b":"y"}"#);
        assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), v);
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
    }

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    #[test]
    fn nested_maps_write_in_flow_form() {
        let author = map(&[("name", s("Ada")), ("tags", Value::List(vec!["x".into(), "y, z".into()]))]);
        assert_eq!(format_field("author", &author), "author: {name: Ada, tags: [x, \"y, z\"]}\n");
        assert_eq!(author.at_path(&["name"]).and_then(Value::as_str), Some("Ada"));
        assert_eq!(author.at_path(&[]), Some(&author));
        assert!(author.at_path(&["nope"]).is_none());
        assert!(author.as_list().is_empty() && author.as_str().is_none());
    }

    const NESTED: &str = "---\n# who wrote it\nauthor:\n  name: Ada\n  orcid: 0000-0001\n  address:\n    city: London\n\ntags:\n- a\n- b\nmeta: {rev: 2, by: [x, y]}\nabstract: |\n  Line one\n  line two\nrefs:\n  - title: A\n    year: 1999\n  - title: B\ntitle: T\n---\nbody\n";

    #[test]
    fn review_multiline_value_round_trips() {
        let src = "---\ntitle: T\ntags: [a]\n---\nbody\n";
        let evil = "one\ntags: [evil]\n---\ninjected";
        let out = set_field(src, "summary", &s(evil)).apply(src);
        let fm = parse(&out).unwrap();
        assert_eq!(fm.tags(), ["a"], "{out}");
        assert_eq!(fm.get("summary"), Some(&s(evil)), "{out}");
        assert!(out.ends_with("---\nbody\n"), "{out}");
        assert_eq!(out.matches("\n---\n").count(), 1, "one closing fence: {out}");
        // Nested fields, list items, other line breaks and control characters too.
        for v in ["a\r\n---\r\nb: c", "x\u{2028}y", "tab\there", "back\\slash \"q\"\n- item", "nul\u{0}"] {
            let out = set_field_path(src, &["author", "name"], &s(v)).unwrap().apply(src);
            let fm = parse(&out).unwrap();
            assert_eq!(fm.get_path("author.name"), Some(&s(v.trim())), "{out}");
            assert_eq!(fm.tags(), ["a"], "{out}");
            let out = set_field(src, "aliases", &Value::List(vec![v.to_string(), "b".into()])).apply(src);
            assert_eq!(parse(&out).unwrap().aliases(), [v.trim().to_string(), "b".to_string()], "{out}");
        }
        // A key can't carry a line break either.
        let out = set_field(src, "k\ntags", &s("x")).apply(src);
        assert_eq!(parse(&out).unwrap().tags(), ["a"], "{out}");
        assert_eq!(parse(&out).unwrap().get("k tags"), Some(&s("x")), "{out}");
        // Escapes written by other tools read as YAML says.
        let fm = parse("---\nq: \"a\\nb \\u00e9 \\\\ \\\" C:\\q\"\n---\n").unwrap();
        assert_eq!(fm.get("q"), Some(&s("a\nb é \\ \" C:\\q")));
    }

    #[test]
    fn parses_nested_yaml() {
        let fm = parse(NESTED).unwrap();
        assert_eq!(fm.get_path("author.name"), Some(&s("Ada")));
        assert_eq!(fm.get_path("author.address.city"), Some(&s("London")));
        assert_eq!(fm.get_path("author.orcid"), Some(&s("0000-0001")));
        assert_eq!(fm.tags(), vec!["a", "b"], "items at the key's indent");
        assert_eq!(fm.get_path("meta.rev"), Some(&s("2")));
        assert_eq!(fm.get_path("meta.by"), Some(&Value::List(vec!["x".into(), "y".into()])));
        assert_eq!(fm.get("abstract"), Some(&s("Line one\nline two")));
        assert_eq!(fm.get("refs"), Some(&s("  - title: A\n    year: 1999\n  - title: B")), "lists of maps stay raw");
        assert_eq!(fm.title(), Some("T"));
        let leaves: Vec<String> = fm.get("author").unwrap().leaves().into_iter().map(|(p, _)| p).collect();
        assert_eq!(leaves, ["address.city", "name", "orcid"]);
        assert_eq!(key_paths(NESTED).len(), 10);
    }

    /// Lines of `after` that aren't in `before`, and vice versa.
    fn changed_lines(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
        let b: Vec<&str> = before.split_inclusive('\n').collect();
        let a: Vec<&str> = after.split_inclusive('\n').collect();
        let gone = b.iter().filter(|l| !a.contains(l)).map(|l| (*l).to_string()).collect();
        let new = a.iter().filter(|l| !b.contains(l)).map(|l| (*l).to_string()).collect();
        (gone, new)
    }

    #[test]
    fn path_edits_touch_only_their_lines() {
        let e = set_field_path(NESTED, &["author", "name"], &s("Ada Lovelace")).unwrap();
        let out = e.apply(NESTED);
        assert_eq!(changed_lines(NESTED, &out), (vec!["  name: Ada\n".into()], vec!["  name: Ada Lovelace\n".into()]));
        assert_eq!(parse(&out).unwrap().get_path("author.name"), Some(&s("Ada Lovelace")));

        // A new leaf goes at the end of its map, at its siblings' indent.
        let out = set_field_path(NESTED, &["author", "address", "zip"], &s("N1")).unwrap().apply(NESTED);
        assert!(out.contains("    city: London\n    zip: N1\n\ntags:"), "{out}");
        // Missing parents are created.
        let out = set_field_path(NESTED, &["review", "by", "name"], &s("Bo")).unwrap().apply(NESTED);
        assert!(out.contains("title: T\nreview:\n  by:\n    name: Bo\n---\n"), "{out}");
        assert_eq!(parse(&out).unwrap().get_path("review.by.name"), Some(&s("Bo")));
        // An inline map keeps its line and form.
        let out = set_field_path(NESTED, &["meta", "rev"], &s("3")).unwrap().apply(NESTED);
        assert_eq!(changed_lines(NESTED, &out).1, vec!["meta: {by: [x, y], rev: 3}\n".to_string()]);
        // A scalar parent becomes a map.
        let out = set_field_path(NESTED, &["title", "main"], &s("T")).unwrap().apply(NESTED);
        assert_eq!(parse(&out).unwrap().get_path("title.main"), Some(&s("T")));
        // No front matter yet.
        assert_eq!(set_field_path("body", &["a", "b"], &s("c")).unwrap().apply("body"), "---\na:\n  b: c\n---\nbody");
        // A map value is written as a block.
        let out = set_field_path(NESTED, &["author", "address"], &map(&[("city", s("Paris")), ("cc", s("FR"))])).unwrap().apply(NESTED);
        assert!(out.contains("  address:\n    cc: FR\n    city: Paris\n\n"), "{out}");
    }

    #[test]
    fn remove_and_rename_by_path() {
        let out = remove_field_path(NESTED, &["author", "orcid"]).unwrap().unwrap().apply(NESTED);
        assert_eq!(changed_lines(NESTED, &out), (vec!["  orcid: 0000-0001\n".into()], vec![]));
        let out = remove_field_path(NESTED, &["author", "address", "city"]).unwrap().unwrap().apply(NESTED);
        assert!(out.contains("  address: {}\n"), "{out}");
        let out = remove_field_path(NESTED, &["meta", "by"]).unwrap().unwrap().apply(NESTED);
        assert!(out.contains("meta: {rev: 2}\n"));
        assert!(remove_field_path(NESTED, &["author", "nope"]).unwrap().is_none());
        let out = remove_field_path(NESTED, &["author"]).unwrap().unwrap().apply(NESTED);
        assert!(!out.contains("Ada") && !out.contains("London") && out.contains("# who wrote it\n\ntags:"), "{out}");

        let out = rename_field_path(NESTED, &["author", "name"], "full name").unwrap().unwrap().apply(NESTED);
        assert_eq!(changed_lines(NESTED, &out), (vec!["  name: Ada\n".into()], vec!["  full name: Ada\n".into()]));
        let out = rename_field_path(NESTED, &["abstract"], "summary").unwrap().unwrap().apply(NESTED);
        assert_eq!(parse(&out).unwrap().get("summary"), Some(&s("Line one\nline two")));
        let out = rename_field_path(NESTED, &["meta", "rev"], "version").unwrap().unwrap().apply(NESTED);
        assert!(out.contains("meta: {by: [x, y], version: 2}\n"));
        let src = "---\n\"odd key\": 1\nk2:v\n---\n";
        assert_eq!(rename_field_path(src, &["odd key"], "even").unwrap().unwrap().apply(src), "---\neven: 1\nk2:v\n---\n");
        assert_eq!(rename_field_path(src, &["k2"], "k3").unwrap().unwrap().apply(src), "---\n\"odd key\": 1\nk3:v\n---\n");
    }

    #[test]
    fn list_items_change_in_place() {
        let rename = |t: &str| if t == "a" { ItemChange::Replace("z/a".into()) } else { ItemChange::Keep };
        let out = edit_list_items(NESTED, &["tags"], rename).unwrap().apply(NESTED);
        assert_eq!(changed_lines(NESTED, &out), (vec!["- a\n".into()], vec!["- z/a\n".into()]));
        let src = "---\ntags: [a,  \"b, c\", d]\ntag: a, d\n---\n";
        let out = edit_list_items(src, &["tags"], |t| if t == "d" { ItemChange::Remove } else { ItemChange::Keep }).unwrap().apply(src);
        assert_eq!(out, "---\ntags: [a, \"b, c\"]\ntag: a, d\n---\n");
        let out = edit_list_items(src, &["tag"], |t| if t == "a" { ItemChange::Replace("x".into()) } else { ItemChange::Keep })
            .unwrap()
            .apply(src);
        assert_eq!(out, "---\ntags: [a,  \"b, c\", d]\ntag: x, d\n---\n");
        assert!(edit_list_items(src, &["tags"], |_| ItemChange::Keep).is_none());
        let out = edit_list_items(NESTED, &["tags"], |_| ItemChange::Remove).unwrap().apply(NESTED);
        assert!(out.contains("\ntags: []\nmeta:"), "{out}");
    }

    #[test]
    fn crlf_and_missing_final_newline() {
        let src = "---\r\na:\r\n  b: 1\r\n---\r\nx";
        let out = set_field_path(src, &["a", "c"], &s("2")).unwrap().apply(src);
        assert_eq!(out, "---\r\na:\r\n  b: 1\r\n  c: 2\r\n---\r\nx");
        assert_eq!(parse(&out).unwrap().get_path("a.c"), Some(&s("2")));
    }

    /// Seeded random edits never change lines outside the edited field.
    #[test]
    fn random_path_edits_keep_other_lines() {
        let mut seed = 0x9e37_79b9_u64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let keys = ["author", "name", "city", "x", "tags", "meta"];
        let mut src = NESTED.to_string();
        for _ in 0..300 {
            let depth = 1 + rnd(3);
            let path: Vec<&str> = (0..depth).map(|_| keys[rnd(keys.len())]).collect();
            let before = src.clone();
            let value = s(&format!("v{}", rnd(100)));
            let edit = match rnd(3) {
                0 => Some(set_field_path(&src, &path, &value).unwrap()),
                1 => remove_field_path(&src, &path).unwrap(),
                _ => rename_field_path(&src, &path, keys[rnd(keys.len())]).unwrap(),
            };
            let Some(edit) = edit else { continue };
            src = edit.apply(&src);
            let fm = parse(&src).expect("still front matter");
            // Bytes outside the edit are unchanged.
            assert_eq!(&src[..edit.range.start], &before[..edit.range.start]);
            assert_eq!(&src[edit.range.start + edit.text.len()..], &before[edit.range.end..]);
            assert!(src.ends_with("---\nbody\n"));
            let _ = fm;
        }
    }

    // ---- text kept as written (nesting past the limit, lists of maps)

    fn flow_note(n: usize) -> String {
        format!("---\nz: 1\nk: {}x{}\ny: 2\n---\nbody\n", "{a: ".repeat(n), "}".repeat(n))
    }

    fn block_note(n: usize) -> String {
        let mut s = String::from("---\nz: 1\n");
        for d in 0..n {
            s.push_str(&" ".repeat(d * 2));
            let _ = writeln!(s, "k{d}:");
        }
        s.push_str(&" ".repeat(n * 2));
        s.push_str("leaf: 1\n---\nbody\n");
        s
    }

    fn block_path(depth: usize) -> Vec<String> {
        (0..depth).map(|d| format!("k{d}")).collect()
    }

    /// The reviewers' D-L3 / S-L3: an edit through a capped entry used to
    /// drop its deep part (10616 -> 4498 bytes) or re-quote it as a string.
    #[test]
    fn set_remove_and_rename_through_a_capped_flow_map_are_refused() {
        let src = flow_note(100);
        let p = ["k", "a", "b"];
        assert!(set_field_path(&src, &p, &s("v")).is_err());
        assert!(remove_field_path(&src, &["k", "a"]).is_err());
        assert!(rename_field_path(&src, &["k", "a"], "b").is_err());
        let err = set_field_path(&src, &p, &s("v")).unwrap_err();
        assert_eq!(err.path, "k.a.b");
        assert!(err.to_string().contains("k.a.b") && err.to_string().contains("the note is unchanged"), "{err}");
        // A flow map that is only just under the limit is still edited as before.
        let ok = format!("---\nk: {}x{}\n---\n", "{a: ".repeat(10), "}".repeat(10));
        assert!(set_field_path(&ok, &["k", "a", "b"], &s("v")).is_ok());
        // Whole-entry edits still work: they replace the entry on purpose.
        let out = set_field(&src, "k", &s("new")).apply(&src);
        assert_eq!(out, src.replace(&format!("k: {}x{}", "{a: ".repeat(100), "}".repeat(100)), "k: new"));
        // The capped entry sits among other keys: editing those keeps its bytes.
        let out = set_field_path(&src, &["z"], &s("9")).unwrap().apply(&src);
        assert_eq!(out, src.replacen("z: 1", "z: 9", 1));
        let out = remove_field(&src, "y").unwrap().apply(&src);
        assert_eq!(out, src.replace("y: 2\n", ""));
    }

    #[test]
    fn set_remove_and_rename_through_a_capped_block_map_are_refused() {
        let src = block_note(100);
        for depth in [63, 64, 80] {
            let mut p = block_path(depth);
            p.push("newkey".into());
            let pr: Vec<&str> = p.iter().map(String::as_str).collect();
            if depth > 63 {
                assert!(set_field_path(&src, &pr, &s("v")).is_err(), "set below k{}", depth - 1);
                assert!(remove_field_path(&src, &pr).is_err(), "remove below k{}", depth - 1);
                assert!(rename_field_path(&src, &pr, "other").is_err(), "rename below k{}", depth - 1);
            }
        }
        // Down to the last readable level it works and keeps the capped text.
        let mut p = block_path(63);
        p.push("newkey".into());
        let pr: Vec<&str> = p.iter().map(String::as_str).collect();
        let edit = set_field_path(&src, &pr, &s("v")).unwrap();
        let out = edit.apply(&src);
        assert!(out.contains("leaf: 1\n") && out.len() > src.len(), "the capped text stays");
        let leaf = format!("{}leaf: 1\n", " ".repeat(200));
        assert!(out.contains(&leaf));
        // The capped entry itself can be renamed or removed as one whole.
        let p63 = block_path(64);
        let pr: Vec<&str> = p63.iter().map(String::as_str).collect();
        assert!(rename_field_path(&src, &pr, "renamed").unwrap().is_some());
        // Other keys: the capped bytes are identical.
        let out = set_field(&src, "z", &s("2")).apply(&src);
        assert_eq!(out, src.replacen("z: 1", "z: 2", 1));
    }

    #[test]
    fn a_list_of_maps_is_not_rewritten_by_a_path_edit() {
        let src = "---\nauthors:\n  - name: Ada\n    org: X\n  - name: Bo\nz: 1\n---\n";
        assert!(set_field_path(src, &["authors", "name"], &s("v")).is_err());
        assert!(remove_field_path(src, &["authors", "name"]).is_err());
        assert!(rename_field_path(src, &["authors", "name"], "who").is_err());
        let out = set_field(src, "z", &s("2")).apply(src);
        assert_eq!(out, src.replace("z: 1", "z: 2"));
    }

    /// Every writer that touches front matter leaves a capped entry's bytes
    /// as they were when it edits something else.
    #[test]
    fn every_writer_keeps_capped_entries_byte_identical() {
        use crate::linter::{Cx, Options, lint};
        for src in [flow_note(100), block_note(100)] {
            let capped_lines: Vec<&str> = src.lines().filter(|l| l.starts_with('k') || l.starts_with(' ')).collect();
            let keeps = |out: &str, what: &str| {
                for l in &capped_lines {
                    assert!(out.lines().any(|o| o == *l), "{what} changed a capped line: {}", &l[..l.len().min(40)]);
                }
                assert!(out.ends_with("body\n"), "{what}");
            };
            // Property edits (the panel, MCP, CLI, templates and tasks all use these).
            keeps(&set_field(&src, "z", &s("2")).apply(&src), "set_field");
            keeps(&set_field(&src, "new", &s("v")).apply(&src), "set_field (new key)");
            keeps(&set_field_path(&src, &["z"], &s("2")).unwrap().apply(&src), "set_field_path");
            keeps(&remove_field(&src, "z").unwrap().apply(&src), "remove_field");
            keeps(&rename_field_path(&src, &["z"], "zz").unwrap().unwrap().apply(&src), "rename_field_path");
            keeps(&append_list_item(&src, "tags", "t").unwrap().apply(&src), "append_list_item");
            let with_list = src.replacen("z: 1\n", "z: 1\ntags: [a, b]\n", 1);
            keeps(
                &edit_list_items(&with_list, &["tags"], |_| ItemChange::Replace("c".into())).unwrap().apply(&with_list),
                "edit_list_items",
            );
            // Tidy and the linter, with every front-matter rule on.
            let opts =
                Options { front_matter: true, tags_format: crate::linter::TagsFormat::Keep, date_modified: true, ..Options::default() };
            let out = lint(&src, &opts, &Cx { stamp: "2026-11-10T09:30", edited: true });
            keeps(&out.text, "the linter");
            let opts = Options { front_matter: true, front_matter_sort: crate::linter::Sort::Alpha, ..Options::default() };
            let out = lint(&src, &opts, &Cx::default());
            assert!(
                out.text == src || out.text.lines().filter(|l| capped_lines.contains(l)).count() == capped_lines.len(),
                "sorting must not rewrite a capped entry"
            );
        }
    }

    /// A hostile note can't push the property editor off a normal stack.
    #[test]
    fn deep_edits_run_on_a_small_stack() {
        let notes = [flow_note(20_000), block_note(1000)];
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                for src in notes {
                    assert!(set_field_path(&src, &["k", "a", "b"], &s("v")).is_err() || src.contains("k0"));
                    let _ = remove_field_path(&src, &["k", "a"]);
                    let _ = rename_field_path(&src, &["k", "a"], "b");
                    let _ = set_field(&src, "z", &s("2")).apply(&src);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    // ---- yaml-rust2 and nesting (S-L1)

    #[test]
    fn yaml_depth_is_measured_without_recursion() {
        assert!(yaml_depth_ok("a: 1\nb:\n  - x\n  - y: [1, 2]\n    z: {a: b}\n"));
        assert!(yaml_depth_ok("tags:\n  - a\n"));
        assert!(yaml_depth_ok(&format!("tags:\n  {}x\n", "- ".repeat(60))));
        assert!(!yaml_depth_ok(&format!("tags:\n  {}x\n", "- ".repeat(100))));
        assert!(!yaml_depth_ok(&format!("tags:\n  {}x\n", "- ".repeat(100_000))));
        assert!(!yaml_depth_ok(&format!("{}x\n", "? ".repeat(100))));
        let mut maps = String::new();
        for d in 0..100 {
            maps.push_str(&" ".repeat(d));
            maps.push_str("k:\n");
        }
        assert!(!yaml_depth_ok(&maps));
        // A comment or a blank line is no level; a document break starts over.
        assert!(yaml_depth_ok(&format!("{}\n# c\n\n---\n{}", "k:\n".repeat(50), "k:\n".repeat(50))));
    }

    /// Lists, maps, flow, and a mix, 100,000 deep, on a normal-size stack: a
    /// stack overflow aborts the whole test run, so passing means every
    /// YAML entry point returned.
    #[test]
    fn deep_yaml_reaches_no_recursive_parser() {
        use crate::linter::{Cx, Options, lint};
        let n = 100_000;
        // Block maps can't be 100,000 deep (the indentation alone would be 5 GB): 1,000 is 0.5 MB.
        let maps: String = (0..1000).map(|d| " ".repeat(d) + "k:\n").collect();
        let bodies = [
            maps,
            format!("tags:\n  {}x\n", "- ".repeat(n)),
            format!("tags: {}x{}\n", "[".repeat(n), "]".repeat(n)),
            format!("k: {}x{}\n", "{a: ".repeat(n), "}".repeat(n)),
            format!("tags:\n  - {}\n  - x\n", "[".repeat(n)),
            format!("a:\n{}", "- ".repeat(n)),
            format!("tags:\n  {}x\n", "- [".repeat(n)),
            format!("{}x\n", "? ".repeat(n)),
            format!("{}x\n", "- {a: ".repeat(n)),
        ];
        let notes: Vec<(String, String)> = bodies.into_iter().map(|b| (format!("---\n{b}---\nbody\n"), b)).collect();
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                // Only the front-matter rules: the other rules scan the whole text, and a
                // line of 100,000 `[` is slow for them (a separate matter from the YAML parser).
                let opts = Options {
                    heading_increment: false,
                    heading_style: crate::linter::HeadingStyle::Keep,
                    trailing_spaces: false,
                    list_marker: crate::linter::ListMarker::Keep,
                    blank_lines: false,
                    final_newline: false,
                    front_matter: true,
                    link_spacing: false,
                    ..Options::default()
                };
                for (note, body) in notes {
                    let _ = yaml_valid(&note);
                    let _ = parse(&note);
                    let out = lint(&note, &opts, &Cx { stamp: "2026-11-10T09:30", edited: true });
                    assert_eq!(out.text, note, "a note too deep to check is left as it was");
                    assert!(load_yaml(&body).is_err(), "too deep to read, so refused or errored, never overflowed");
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
