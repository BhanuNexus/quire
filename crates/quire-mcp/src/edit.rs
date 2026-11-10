// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The edits agents make, as pure functions of a note's text (design §4.2).
//!
//! Most are *content-addressed* (they find their place by text: a string,
//! a heading, the end of a section), so they can be re-applied to a newer
//! version of the note when the agent's version is stale (§B.2). `insert`
//! names a line number and can't be.

use quire_app::markdown::{Value, frontmatter, outline};

/// One edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOp {
    /// `str_replace`: `old` must occur exactly once.
    Replace { old: String, new: String },
    /// `insert`: new lines after line `line` (0 = top).
    Insert { line: usize, text: String },
    /// `edit_section`: the section under a heading (`## Undo`, `Undo`) or a
    /// block (`^id`).
    Section { target: String, content: String, mode: SectionMode },
    /// `append`: at the end of the note, or of a section.
    Append { text: String, under: Option<String> },
    /// `set_properties`.
    Properties { set: Vec<(String, Value)>, remove: Vec<String> },
    /// `add_link` (0.12): a typed link to `target` in field `field`, in
    /// the style the note already uses (front matter or inline field).
    TypedLink { field: String, target: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionMode {
    Replace,
    Append,
    Prepend,
}

/// Why an edit doesn't fit the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    NotUnique {
        matches: usize,
    },
    NoMatch,
    /// No heading or block like this; the headings that exist.
    NoSection {
        target: String,
        available: Vec<String>,
    },
    BadLine {
        line: usize,
        total: usize,
    },
    Invalid(String),
}

impl EditError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotUnique { .. } => "not_unique",
            Self::NoMatch | Self::NoSection { .. } => "no_match",
            Self::BadLine { .. } | Self::Invalid(_) => "invalid_args",
        }
    }

    pub fn message(&self, path: &str) -> String {
        match self {
            Self::NotUnique { matches } => {
                format!("old_str occurs {matches} times in {path}; include more surrounding text so it matches exactly once.")
            }
            Self::NoMatch => format!("old_str isn't in {path}. `view` the note and copy the text exactly (whitespace included)."),
            Self::NoSection { target, available } => {
                let list = if available.is_empty() { "none".to_string() } else { available.join(", ") };
                format!("No section “{target}” in {path}. Headings: {list}.")
            }
            Self::BadLine { line, total } => format!("line {line} is past the end of {path} ({total} lines)."),
            Self::Invalid(m) => m.clone(),
        }
    }
}

impl EditOp {
    /// Whether the edit finds its place by content, so it can move to a newer version.
    pub const fn content_addressed(&self) -> bool {
        !matches!(self, Self::Insert { .. })
    }

    pub fn apply(&self, text: &str) -> Result<String, EditError> {
        match self {
            Self::Replace { old, new } => {
                if old.is_empty() {
                    return Err(EditError::Invalid("old_str is empty; use insert or append to add text.".into()));
                }
                let matches = text.matches(old.as_str()).count();
                match matches {
                    0 => Err(EditError::NoMatch),
                    1 => Ok(text.replacen(old.as_str(), new, 1)),
                    n => Err(EditError::NotUnique { matches: n }),
                }
            }
            Self::Insert { line, text: add } => {
                let starts = line_starts(text);
                let total = line_count(text);
                if *line > total {
                    return Err(EditError::BadLine { line: *line, total });
                }
                let at = if *line == 0 { 0 } else { starts.get(*line + 1).copied().unwrap_or(text.len()) };
                let mut out = String::with_capacity(text.len() + add.len() + 2);
                out.push_str(&text[..at]);
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(add);
                if !add.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&text[at..]);
                Ok(out)
            }
            Self::Section { target, content, mode } => edit_section(text, target, content, *mode),
            Self::Append { text: add, under } => match under {
                None => Ok(append_at(text, text.len(), add)),
                Some(h) => {
                    let s = find_section(text, h)?;
                    Ok(append_at(text, trim_blank_tail(text, s.body, s.end), add))
                }
            },
            Self::TypedLink { field, target } => {
                if !quire_app::markdown::fields::is_field_name(field) {
                    return Err(EditError::Invalid(format!("“{field}” isn't a link type: use letters, digits, spaces, “-” or “_”.")));
                }
                let style = quire_app::markdown::fields::preferred_style(text);
                // Already linked this way: no change.
                match quire_app::markdown::fields::try_add_link(text, field, target, style) {
                    Ok(edit) => Ok(edit.map_or_else(|| text.to_string(), |e| e.apply(text))),
                    Err(why) => Err(EditError::Invalid(format!(
                        "Can't add a “{field}” link without rewriting what the note has there ({why}); change that property by hand."
                    ))),
                }
            }
            Self::Properties { set, remove } => {
                let mut out = text.to_string();
                for (k, v) in set {
                    if k.trim().is_empty() || k.contains(':') || k.contains('\n') {
                        return Err(EditError::Invalid(format!("“{k}” isn't a valid property name.")));
                    }
                    out = frontmatter::set_field(&out, k, v).apply(&out);
                }
                for k in remove {
                    if let Some(e) = frontmatter::remove_field(&out, k) {
                        out = e.apply(&out);
                    }
                }
                Ok(out)
            }
        }
    }
}

/// Byte offset of the start of each line (line 1 is index 1; index 0 is 0).
fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0, 0];
    v.extend(text.match_indices('\n').map(|(i, _)| i + 1).filter(|&i| i < text.len()));
    v
}

/// Lines in `text` (a final newline doesn't start another line).
pub fn line_count(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.lines().count() }
}

/// Insert `add` as whole lines at byte `at` (a line boundary or the end).
fn append_at(text: &str, at: usize, add: &str) -> String {
    let (head, tail) = text.split_at(at);
    let mut out = String::with_capacity(text.len() + add.len() + 2);
    out.push_str(head);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(add);
    if !add.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(tail);
    out
}

/// A section: the heading line (or block) and where its body ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    /// Start of the heading line (or of the block).
    start: usize,
    /// Start of the body (after the heading line); equals `start` for blocks.
    body: usize,
    /// End of the section (start of the next same-or-higher heading, or EOF).
    end: usize,
}

fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i + 1)
}

/// The headings of `text` as `## Text`.
fn heading_list(text: &str) -> Vec<String> {
    outline(text).iter().map(|h| format!("{} {}", "#".repeat(usize::from(h.level)), h.text)).collect()
}

fn find_section(text: &str, target: &str) -> Result<Span, EditError> {
    let t = target.trim();
    if let Some(id) = t.strip_prefix('^') {
        return find_block(text, id).ok_or_else(|| EditError::NoSection { target: t.into(), available: heading_list(text) });
    }
    let (level, name) = match t.find(|c| c != '#') {
        Some(n) if n > 0 => (Some(n), t[n..].trim()),
        _ => (None, t),
    };
    let heads = outline(text);
    let found: Vec<usize> = heads
        .iter()
        .enumerate()
        .filter(|(_, h)| h.text.trim().eq_ignore_ascii_case(name) && level.is_none_or(|l| usize::from(h.level) == l))
        .map(|(i, _)| i)
        .collect();
    let i = match found.as_slice() {
        [i] => *i,
        [] => return Err(EditError::NoSection { target: t.into(), available: heading_list(text) }),
        many => return Err(EditError::NotUnique { matches: many.len() }),
    };
    let h = &heads[i];
    let start = text[..h.offset].rfind('\n').map_or(0, |n| n + 1);
    let end = heads[i + 1..].iter().find(|n| n.level <= h.level).map_or(text.len(), |n| text[..n.offset].rfind('\n').map_or(0, |x| x + 1));
    Ok(Span { start, body: line_end(text, start), end })
}

/// The paragraph (or list item line) ending with ` ^id`.
fn find_block(text: &str, id: &str) -> Option<Span> {
    let marker = format!("^{id}");
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.ends_with(&marker) && trimmed[..trimmed.len() - marker.len()].ends_with([' ', '\t']) {
            let end = offset + line.len();
            // A list item or a heading is its own block; a paragraph starts after the blank line above.
            let lead = trimmed.trim_start();
            let start = if lead.starts_with(['-', '*', '+', '#', '>']) || lead.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                offset
            } else {
                let mut s = offset;
                while s > 0 {
                    let prev_start = text[..s - 1].rfind('\n').map_or(0, |n| n + 1);
                    let prev = text[prev_start..s].trim();
                    // A paragraph starts after a blank line, a heading, a list item or a fence.
                    if prev.is_empty() || prev.starts_with(['#', '-', '*', '+', '>']) || prev.starts_with("```") || prev.starts_with("~~~")
                    {
                        break;
                    }
                    s = prev_start;
                }
                s
            };
            return Some(Span { start, body: start, end });
        }
        offset += line.len();
    }
    None
}

/// The 1-based inclusive lines of a section (`## Undo`, `Undo`, `^id`).
pub fn section_lines(text: &str, target: &str) -> Result<(usize, usize), EditError> {
    let s = find_section(text, target)?;
    let line_of = |b: usize| text[..b].matches('\n').count() + 1;
    let start = line_of(s.start);
    let end = if s.end == 0 { start } else { line_of(s.end - 1) };
    Ok((start, end.max(start)))
}

fn edit_section(text: &str, target: &str, content: &str, mode: SectionMode) -> Result<String, EditError> {
    let s = find_section(text, target)?;
    let block = target.trim().starts_with('^');
    let mut body = content.to_string();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    if block && mode == SectionMode::Replace {
        // Keep the block id so links to it still resolve.
        let id = target.trim();
        if !body.trim_end().ends_with(id) {
            body = format!("{} {id}\n", body.trim_end());
        }
    }
    let mut out = String::with_capacity(text.len() + body.len());
    match mode {
        SectionMode::Replace => {
            out.push_str(&text[..s.body]);
            out.push_str(&body);
            // Keep a blank line before the next heading if there was one.
            if !block && s.end < text.len() && !body.is_empty() && !body.ends_with("\n\n") && text[s.body..s.end].ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(&text[s.end..]);
        }
        SectionMode::Append => return Ok(append_at(text, trim_blank_tail(text, s.body, s.end), content)),
        SectionMode::Prepend => {
            out.push_str(&text[..s.body]);
            out.push_str(&body);
            out.push_str(&text[s.body..]);
        }
    }
    Ok(out)
}

/// The end of a section's text, before trailing blank lines.
fn trim_blank_tail(text: &str, body: usize, end: usize) -> usize {
    let mut e = end;
    while e > body {
        let prev = text[..e - 1].rfind('\n').map_or(0, |n| n + 1).max(body);
        if text[prev..e].trim().is_empty() {
            e = prev;
        } else {
            break;
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "# Rope\nintro\n\n## Undo\nold undo\n\n## Speed\nfast ^fast\n\nA para\nover two lines ^p2\n";

    #[expect(clippy::needless_pass_by_value, reason = "each test builds the op inline")]
    fn ok(op: EditOp, text: &str) -> String {
        op.apply(text).unwrap()
    }

    #[test]
    fn replace_needs_exactly_one_match() {
        let r = |o: &str, n: &str| EditOp::Replace { old: o.into(), new: n.into() };
        assert_eq!(ok(r("old undo", "new undo"), NOTE).lines().nth(4), Some("new undo"));
        assert_eq!(r("nope", "x").apply(NOTE), Err(EditError::NoMatch));
        assert_eq!(r("o", "x").apply(NOTE).unwrap_err().code(), "not_unique");
        assert_eq!(r("", "x").apply(NOTE).unwrap_err().code(), "invalid_args");
    }

    #[test]
    fn insert_by_line() {
        let i = |line, t: &str| EditOp::Insert { line, text: t.into() };
        assert!(ok(i(0, "top"), NOTE).starts_with("top\n# Rope\n"));
        assert!(ok(i(2, "after intro"), NOTE).starts_with("# Rope\nintro\nafter intro\n\n## Undo"));
        assert!(ok(i(11, "end"), NOTE).ends_with("^p2\nend\n"));
        assert_eq!(i(12, "x").apply(NOTE), Err(EditError::BadLine { line: 12, total: 11 }));
        assert_eq!(ok(i(1, "b"), "a"), "a\nb\n");
        assert!(!i(0, "x").content_addressed());
    }

    #[test]
    fn sections_by_heading_and_block() {
        let s = |t: &str, c: &str, mode| EditOp::Section { target: t.into(), content: c.into(), mode };
        let out = ok(s("## Undo", "ropes share subtrees", SectionMode::Replace), NOTE);
        assert!(out.contains("## Undo\nropes share subtrees\n\n## Speed"), "{out}");
        let out = ok(s("undo", "more", SectionMode::Append), NOTE);
        assert!(out.contains("old undo\nmore\n\n## Speed"), "{out}");
        let out = ok(s("Undo", "first", SectionMode::Prepend), NOTE);
        assert!(out.contains("## Undo\nfirst\nold undo"), "{out}");
        let out = ok(s("# Rope", "only", SectionMode::Replace), NOTE);
        assert_eq!(out, "# Rope\nonly\n", "a top heading's section runs to the end");
        let out = ok(s("^fast", "very fast", SectionMode::Replace), NOTE);
        assert!(out.contains("## Speed\nvery fast ^fast\n\nA para"), "{out}");
        let out = ok(s("^p2", "one line", SectionMode::Replace), NOTE);
        assert!(out.ends_with("\n\none line ^p2\n"), "{out}");
        let e = s("## Nope", "x", SectionMode::Replace).apply(NOTE).unwrap_err();
        assert!(e.message("R.md").contains("## Undo"), "lists the headings");
        let twice = "# A\n## X\none\n## X\ntwo\n";
        assert_eq!(s("X", "y", SectionMode::Replace).apply(twice).unwrap_err().code(), "not_unique");
        assert!(ok(s("## X", "y", SectionMode::Append), "# A\n## X\n").ends_with("## X\ny\n"));
    }

    #[test]
    fn append_and_properties() {
        let a = |t: &str, u: Option<&str>| EditOp::Append { text: t.into(), under: u.map(Into::into) };
        assert!(ok(a("- end", None), "x").ends_with("x\n- end\n"));
        assert!(ok(a("- more", Some("Undo")), NOTE).contains("old undo\n- more\n\n## Speed"));
        let p = EditOp::Properties { set: vec![("status".into(), Value::Str("done".into()))], remove: vec![] };
        let out = ok(p, "# T\n");
        assert!(out.starts_with("---\nstatus: done\n---\n"), "{out}");
        let p = EditOp::Properties { set: vec![], remove: vec!["status".into()] };
        assert_eq!(ok(p, "---\nstatus: done\nx: 1\n---\n# T\n"), "---\nx: 1\n---\n# T\n");
        let bad = EditOp::Properties { set: vec![("a:b".into(), Value::Str("x".into()))], remove: vec![] };
        assert_eq!(bad.apply("x").unwrap_err().code(), "invalid_args");
    }
}
