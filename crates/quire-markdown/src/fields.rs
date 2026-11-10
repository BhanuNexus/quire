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

/// The YAML quote (`'` or `"`) still open at the end of `value`, if any.
///
/// `value` is the text of a property's value up to some point. In single
/// quotes `''` is a quote; in double quotes `\` escapes the next character.
pub fn yaml_quote_at(value: &str) -> Option<char> {
    let mut open: Option<char> = None;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match (open, c) {
            (None, '\'' | '"') => open = Some(c),
            (Some('\''), '\'') => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    open = None;
                }
            }
            (Some('"'), '\\') => {
                chars.next();
            }
            (Some('"'), '"') => open = None,
            _ => {}
        }
    }
    open
}

/// The links in front-matter values, each typed by its property's name
/// (the nearest key above it, for list items and nested maps). `end` is
/// where the front-matter block ends.
pub(crate) fn front_matter_links(src: &str, end: usize) -> Vec<Link> {
    let mut out = Vec::new();
    let block = &src[..end.min(src.len())];
    if !block.contains("[[") {
        return out;
    }
    let mut key: Option<String> = None;
    let mut at = 0;
    for (n, raw) in block.split_inclusive('\n').enumerate() {
        let start = at;
        at += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if n == 0 || line.trim_end() == "---" || line.trim_end() == "..." {
            continue;
        }
        let value_from = match fm_key(line) {
            Some((k, v)) => {
                key = Some(normalize_key(&line[k]));
                v
            }
            None => 0,
        };
        let Some(name) = key.as_ref().filter(|k| !k.is_empty()) else { continue };
        let mut i = value_from;
        while let Some(off) = line[i.min(line.len())..].find("[[") {
            let open = i + off;
            let embed = open > 0 && line.as_bytes()[open - 1] == b'!';
            match wikilink_at(src, start + open) {
                Some((Inline::WikiLink { range, link }, end)) => {
                    let range = if embed { range.start - 1..range.end } else { range };
                    // In a single-quoted YAML value, `''` is one `'`.
                    let quoted = yaml_quote_at(&line[value_from..open]) == Some('\'');
                    let unq = |s: &str| if quoted { s.replace("''", "'") } else { s.to_string() };
                    out.push(Link {
                        kind: if embed { LinkKind::Embed } else { LinkKind::Wiki },
                        display: unq(link.display()),
                        target: unq(&link.target),
                        heading: link.heading.as_deref().map(unq),
                        range,
                        field: Some(name.clone()),
                    });
                    i = end - start;
                }
                _ => i = open + 2,
            }
        }
    }
    out
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

/// The `]` closing a one-line flow list `key: [ … ]`, optionally followed
/// by a comment, as a byte offset in `line`.
fn flow_close(line: &str) -> Option<usize> {
    let (_, value_at) = fm_key(line.trim_end_matches(['\n', '\r']))?;
    let value = &line[value_at..];
    // `[[X]]` unquoted is a link YAML reads as nested lists, not a list to extend.
    if !value.trim_start().starts_with('[') || value.trim_start().starts_with("[[") {
        return None;
    }
    let mut close = None;
    for (i, c) in value.char_indices() {
        if c == ']' && yaml_quote_at(&value[..i]).is_none() {
            close = Some(i);
        }
        if c == '#' && yaml_quote_at(&value[..i]).is_none() && value[..i].ends_with([' ', '\t']) {
            break;
        }
    }
    let close = close?;
    let after = value[close + 1..].trim_start();
    (after.is_empty() || after.starts_with('#')).then_some(value_at + close)
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

/// The edit that adds a link to `target` in field `field` of a note.
///
/// `target` is a note name (written `[[target]]`), `field` a name as
/// [`is_field_name`] allows, `style` [`FieldStyle::Line`] or
/// [`FieldStyle::FrontMatter`]. `None` when the note already has that link
/// in that field. Only the field's own line(s) change.
///
/// * Front matter: a list field gets another item (in its own style: `[…,
///   "[[X]]"]` or `- "[[X]]"`); a single link becomes a list of two; a
///   missing field is added as `field: "[[X]]"`.
/// * Inline: an existing `field:: …` line gets `, [[X]]`; else a new line
///   `field:: [[X]]` goes after the last line field, or after the title
///   heading, or at the top of the body.
pub fn add_link(src: &str, field: &str, target: &str, style: FieldStyle) -> Option<crate::frontmatter::Edit> {
    use crate::frontmatter::{self, Edit, Value};
    let name = normalize_key(field);
    let link = format!("[[{}]]", target.trim());
    let ex = crate::extract(src);
    let already = ex.links.iter().any(|l| l.field.as_deref() == Some(name.as_str()) && l.target.eq_ignore_ascii_case(target.trim()));
    if already || !is_key(field) || target.trim().is_empty() || target.contains(['[', ']', '|', '\n']) {
        return None;
    }
    let nl = if src.contains("\r\n") { "\r\n" } else { "\n" };
    if style == FieldStyle::FrontMatter {
        let fm = ex.front_matter.as_ref();
        let key = fm.and_then(|f| f.fields.keys().find(|k| normalize_key(k) == name).cloned()).unwrap_or_else(|| field.trim().to_string());
        // `key: [a, b]` on one line (maybe with a `# comment`): add before its `]`.
        if let Some(text) = frontmatter::field_text(src, &key).filter(|t| t.trim_end().lines().count() == 1)
            && let Some(close) = flow_close(text)
        {
            // `text` is a slice of `src`: its offset there.
            let at = text.as_ptr() as usize - src.as_ptr() as usize;
            let open = text.find('[').unwrap_or(close);
            let sep = if text[open + 1..close].trim().is_empty() { "" } else { ", " };
            return Some(Edit { range: at + close..at + close, text: format!("{sep}\"{link}\"") });
        }
        let fm_end = fm.map_or(0, |f| f.range.end);
        let written: Vec<String> = ex
            .links
            .iter()
            .filter(|l| l.range.end <= fm_end && l.field.as_deref() == Some(name.as_str()))
            .map(|l| src[l.range.clone()].to_string())
            .collect();
        let block = frontmatter::field_text(src, &key).is_some_and(|t| t.trim_end().lines().count() > 1);
        return Some(match fm.and_then(|f| f.fields.get(&key)) {
            Some(Value::List(_)) if block => frontmatter::append_list_item(src, &key, &format!("\"{link}\""))?,
            _ if !written.is_empty() => frontmatter::set_field(src, &key, &Value::List(written.into_iter().chain([link]).collect())),
            Some(Value::Str(old)) if !old.trim().is_empty() => frontmatter::set_field(src, &key, &Value::List(vec![old.clone(), link])),
            _ => frontmatter::set_field(src, &key, &Value::Str(link)),
        });
    }
    let fields = inline_fields(src);
    let line_fields: Vec<&InlineField> = fields.iter().filter(|f| f.style == FieldStyle::Line).collect();
    if let Some(f) = line_fields.iter().find(|f| f.name == name) {
        let sep = if f.value_range.is_empty() { "" } else { ", " };
        let at = f.value_range.end.max(f.key_range.end + 2);
        let lead = if f.value_range.is_empty() && !src[..at].ends_with(' ') { " " } else { "" };
        return Some(Edit { range: at..at, text: format!("{lead}{sep}{link}") });
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
    Some(Edit { range: at..at, text: format!("{pre}{key}:: {link}{nl}") })
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
        assert_eq!(yaml_quote_at(" 'a', \"b\\\"c"), Some('"'));
        assert_eq!(yaml_quote_at(" 'a' "), None);
        let links = extract("---\nup: [\"[[A's]]\", '[[B''s|x]]']\n---\n").links;
        assert_eq!(links.iter().map(|l| l.target.as_str()).collect::<Vec<_>>(), ["A's", "B's"]);
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
