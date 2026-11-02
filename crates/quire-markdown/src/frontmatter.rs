// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A deliberately small YAML-subset front-matter reader.
//!
//! Supports what note apps actually write: `key: value`, quoted strings,
//! inline lists `key: [a, b]`, and block lists (`key:` then `- item` lines).
//! Anything fancier is kept as a raw string rather than rejected.

use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    List(Vec<String>),
    /// A nested map (`author:` then indented `name: Ada`). The parser doesn't
    /// produce it yet (nested blocks are skipped); writers and views accept it.
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
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        s[1..s.len() - 1].replace("''", "'")
    } else {
        s.to_string()
    }
}

/// Parse front matter at the very start of `src`, if present.
pub fn parse(src: &str) -> Option<FrontMatter> {
    let body_start = if src.starts_with("---\n") {
        4
    } else if src.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let mut pos = body_start;
    let mut end = None;
    for line in src[body_start..].split_inclusive('\n') {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            end = Some((pos, pos + line.len()));
            break;
        }
        pos += line.len();
    }
    let (body_end, block_end) = end?;
    let body = &src[body_start..body_end];

    let mut fields = BTreeMap::new();
    let mut pending_list: Option<(String, Vec<String>)> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some((_, items)) = pending_list.as_mut()
        {
            items.push(unquote(item));
            continue;
        }
        if let Some((k, items)) = pending_list.take() {
            fields.insert(k, Value::List(items));
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with([' ', '\t']) {
            continue; // nested maps are out of scope
        }
        let key = key.trim().to_string();
        let val = val.trim();
        if val.is_empty() {
            pending_list = Some((key, Vec::new()));
        } else if val.starts_with('[') && val.ends_with(']') {
            let items = val[1..val.len() - 1].split(',').map(unquote).filter(|s| !s.is_empty()).collect();
            fields.insert(key, Value::List(items));
        } else {
            fields.insert(key, Value::Str(unquote(val)));
        }
    }
    if let Some((k, items)) = pending_list {
        fields.insert(k, Value::List(items));
    }
    Some(FrontMatter { fields, range: 0..block_end })
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
}

impl Value {
    pub fn kind(&self) -> Kind {
        match self {
            Self::List(_) => Kind::List,
            Self::Str(s) => kind_of(s),
            // Shown as text (its flow form) until the properties panel has a tree.
            Self::Map(_) => Kind::Text,
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
pub fn yaml_scalar(s: &str) -> String {
    let s = s.trim();
    let risky = s.is_empty()
        || s.contains(": ")
        || s.contains(" #")
        || s.starts_with(['#', '[', '{', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`', '-', '?'])
        || s.ends_with(':')
        || matches!(s.to_ascii_lowercase().as_str(), "yes" | "no" | "null" | "~");
    if risky { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) } else { s.to_string() }
}

/// `key: value` line(s) for a field, with trailing newline.
pub fn format_field(key: &str, value: &Value) -> String {
    match value {
        Value::Str(s) => format!("{key}: {}\n", yaml_scalar(s)),
        Value::List(items) => format!("{key}: [{}]\n", flow_items(items)),
        Value::Map(_) => format!("{key}: {}\n", value.flow()),
    }
}

/// List items for a flow list, quoting any that contain a comma.
fn flow_items(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|i| {
            let q = yaml_scalar(i);
            if q.contains(',') && !q.starts_with('"') { format!("\"{q}\"") } else { q }
        })
        .collect();
    quoted.join(", ")
}

/// Byte range of a top-level field's line(s), including block-list items.
fn field_lines(src: &str, fm: &FrontMatter, key: &str) -> Option<Range<usize>> {
    let body_start = if src.starts_with("---\r\n") { 5 } else { 4 };
    let mut pos = body_start;
    let mut found: Option<Range<usize>> = None;
    for line in src[body_start..fm.range.end].split_inclusive('\n') {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            break;
        }
        match &mut found {
            Some(r) => {
                let cont = line.starts_with([' ', '\t']) || t.trim_start().starts_with("- ");
                if cont && !t.is_empty() {
                    r.end = pos + line.len();
                } else {
                    break;
                }
            }
            None => {
                if !line.starts_with([' ', '\t'])
                    && let Some((k, _)) = line.split_once(':')
                    && k.trim() == key
                {
                    found = Some(pos..pos + line.len());
                }
            }
        }
        pos += line.len();
    }
    found
}

/// Set (add or replace) a field, preserving everything else in the block.
pub fn set_field(src: &str, key: &str, value: &Value) -> Edit {
    let key = key.trim();
    let line = format_field(key, value);
    match parse(src) {
        None => Edit { range: 0..0, text: format!("---\n{line}---\n") },
        Some(fm) => {
            if let Some(r) = field_lines(src, &fm, key) {
                // Keep the file's line ending for the last replaced line.
                let text = if src[r.clone()].ends_with('\n') { line } else { line.trim_end_matches('\n').to_string() };
                Edit { range: r, text }
            } else {
                // Insert before the closing fence.
                let close = src[..fm.range.end].trim_end_matches(['\n', '\r']);
                let fence_start = close.rfind('\n').map_or(0, |i| i + 1);
                Edit { range: fence_start..fence_start, text: line }
            }
        }
    }
}

/// Remove a field; `None` when it isn't there. Removing the last field
/// removes the whole block.
pub fn remove_field(src: &str, key: &str) -> Option<Edit> {
    let fm = parse(src)?;
    let r = field_lines(src, &fm, key.trim())?;
    if fm.fields.len() == 1 && fm.fields.contains_key(key.trim()) {
        return Some(Edit { range: fm.range, text: String::new() });
    }
    Some(Edit { range: r, text: String::new() })
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
    fn kinds() {
        assert_eq!(Value::Str("2026-11-01".into()).kind(), Kind::Date);
        assert_eq!(Value::Str("3.5".into()).kind(), Kind::Number);
        assert_eq!(Value::Str("true".into()).kind(), Kind::Bool);
        assert_eq!(Value::Str("hello".into()).kind(), Kind::Text);
        assert_eq!(Value::List(vec![]).kind(), Kind::List);
    }

    #[test]
    fn nested_maps_write_in_flow_form() {
        let author = Value::Map(BTreeMap::from([
            ("name".to_string(), Value::Str("Ada".into())),
            ("tags".to_string(), Value::List(vec!["x".into(), "y, z".into()])),
        ]));
        assert_eq!(format_field("author", &author), "author: {name: Ada, tags: [x, \"y, z\"]}\n");
        assert_eq!(author.at_path(&["name"]).and_then(Value::as_str), Some("Ada"));
        assert_eq!(author.at_path(&[]), Some(&author));
        assert!(author.at_path(&["nope"]).is_none());
        assert!(author.as_list().is_empty() && author.as_str().is_none());
        // Today's parser skips nested blocks (WS-E adds them); the rest is unchanged.
        let fm = parse("---\nauthor:\n  name: Ada\ntitle: T\n---\n").unwrap();
        assert_eq!(fm.title(), Some("T"));
    }
}
