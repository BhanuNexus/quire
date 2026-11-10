// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Front matter: spacing, key order, tags and "date modified". It is
//! worked on as text, one entry at a time, so comments and values the
//! reader doesn't know pass through byte for byte. Front matter that doesn't
//! look like plain `key: value` lines is left exactly as it is.

use crate::frontmatter::{self, Value};

use super::options::{Options, Sort, TagsCase, TagsFormat};
use super::rules::dominant_eol;

pub(super) enum Result {
    Same,
    Changed(String),
    /// Front matter is there but can't be worked on safely.
    Unreadable,
}

/// Byte range of the lines between the fences, if the note opens with front matter.
fn locate(text: &str) -> Option<(usize, usize)> {
    let first = text.split_inclusive('\n').next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut pos = first.len();
    for line in text[pos..].split_inclusive('\n') {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            return Some((first.len(), pos));
        }
        pos += line.len();
    }
    None
}

struct Entry {
    key: String,
    /// Comments and blank lines above the key.
    lead: Vec<(String, String)>,
    /// The key's line, then its continuation lines.
    lines: Vec<(String, String)>,
    block_scalar: bool,
}

fn key_of(line: &str) -> Option<(String, &str)> {
    if line.starts_with(['?', '&', '*', '!', '{', '[', '|', '>', '%', '@', '`', ',', '}', ']']) {
        return None;
    }
    if let Some(q) = line.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let close = line[1..].find(q)? + 1;
        let rest = line[close + 1..].strip_prefix(':')?;
        if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
            return None;
        }
        return Some((line[..=close].to_string(), rest));
    }
    let mut at = 0;
    while let Some(i) = line[at..].find(':') {
        let idx = at + i;
        let after = &line[idx + 1..];
        if after.is_empty() || after.starts_with([' ', '\t']) {
            let key = line[..idx].trim_end();
            return (!key.is_empty() && !key.starts_with('#')).then(|| (key.to_string(), after));
        }
        at = idx + 1;
    }
    None
}

fn is_continuation(line: &str) -> bool {
    line.starts_with([' ', '\t']) || line == "-" || line.starts_with("- ")
}

/// Split the body into entries, or `None` if it isn't plain `key: value` lines.
type Line = (String, String);

fn entries(body: &str) -> Option<(Vec<Entry>, Vec<Line>)> {
    let mut lines: Vec<(String, String)> = Vec::new();
    for l in body.split_inclusive('\n') {
        let t = l.trim_end_matches(['\n', '\r']);
        lines.push((t.to_string(), l[t.len()..].to_string()));
    }
    let mut out: Vec<Entry> = Vec::new();
    let mut lead: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (t, _) = &lines[i];
        if t.trim().is_empty() || t.starts_with('#') {
            lead.push(lines[i].clone());
            i += 1;
            continue;
        }
        if t.starts_with(['\t']) || (t.starts_with(' ') && t.trim_start().starts_with('\t')) {
            return None;
        }
        if is_continuation(t) {
            let e = out.last_mut()?;
            // Blank lines and comments just before belong to this entry.
            e.lines.append(&mut lead);
            e.lines.push(lines[i].clone());
            i += 1;
            continue;
        }
        let (key, rest) = key_of(t)?;
        let block_scalar = rest.trim_start().starts_with(['|', '>']);
        out.push(Entry { key, lead: std::mem::take(&mut lead), lines: vec![lines[i].clone()], block_scalar });
        i += 1;
    }
    Some((out, lead))
}

fn tidy_key_line(line: &str) -> String {
    let Some((key, rest)) = key_of(line) else { return line.trim_end().to_string() };
    let value = rest.trim();
    if value.is_empty() { format!("{key}:") } else { format!("{key}: {value}") }
}

fn tag_items(raw: &Value, case: TagsCase) -> Vec<String> {
    let mut seen = Vec::new();
    for item in raw.as_list() {
        let pieces: Vec<&str> = if item.split_whitespace().count() > 1 && item.split_whitespace().all(|p| p.starts_with('#')) {
            item.split_whitespace().collect()
        } else {
            vec![item.as_str()]
        };
        for p in pieces {
            let t = p.trim().trim_start_matches('#').split_whitespace().collect::<Vec<_>>().join("-");
            let t = if case == TagsCase::Lower { t.to_lowercase() } else { t };
            if !t.is_empty() && !seen.contains(&t) {
                seen.push(t);
            }
        }
    }
    seen
}

fn render_tags(key: &str, tags: &[String], flow: bool, eol: &str) -> Vec<(String, String)> {
    if tags.is_empty() {
        return vec![(format!("{key}: []"), eol.to_string())];
    }
    if flow {
        let items: Vec<String> = tags.iter().map(|t| frontmatter::yaml_scalar(t)).collect();
        return vec![(format!("{key}: [{}]", items.join(", ")), eol.to_string())];
    }
    let mut v = vec![(format!("{key}:"), eol.to_string())];
    v.extend(tags.iter().map(|t| (format!("  - {}", frontmatter::yaml_scalar(t)), eol.to_string())));
    v
}

pub(super) fn tidy(text: &str, o: &Options) -> Result {
    let Some((start, end)) = locate(text) else { return Result::Same };
    if frontmatter::parse(text).is_none() {
        return Result::Unreadable;
    }
    let Some((mut list, tail)) = entries(&text[start..end]) else { return Result::Unreadable };
    let eol = dominant_eol(text);

    if o.front_matter {
        for e in &mut list {
            if let Some(first) = e.lines.first_mut() {
                first.0 = tidy_key_line(&first.0);
            }
            if !e.block_scalar {
                for l in e.lines.iter_mut().skip(1) {
                    l.0 = l.0.trim_end().to_string();
                }
            }
            for l in &mut e.lead {
                l.0 = l.0.trim_end().to_string();
            }
        }
    }

    if o.tags_format != TagsFormat::Keep || o.tags_case != TagsCase::Keep {
        for e in &mut list {
            if e.key != "tags" || e.lines.iter().any(|(t, _)| t.trim_start().starts_with('#')) {
                continue;
            }
            let mut block = String::new();
            for (t, el) in &e.lines {
                block.push_str(t);
                block.push_str(if el.is_empty() { "\n" } else { el });
            }
            let Some(fm) = frontmatter::parse(&format!("---\n{block}---\n")) else { continue };
            let Some(raw) = fm.get("tags") else { continue };
            if matches!(raw, Value::Map(_)) {
                continue;
            }
            let was_flow = e.lines[0].0.split_once(':').is_some_and(|(_, v)| v.trim_start().starts_with('['));
            let flow = match o.tags_format {
                TagsFormat::Flow => true,
                TagsFormat::List => false,
                TagsFormat::Keep => was_flow,
            };
            e.lines = render_tags(&e.key, &tag_items(raw, o.tags_case), flow, eol);
        }
    }

    let mut dup = false;
    for (i, e) in list.iter().enumerate() {
        dup |= list[..i].iter().any(|p| p.key == e.key);
    }
    if !dup && (!o.front_matter_order.is_empty() || o.front_matter_sort == Sort::Alpha) {
        let rank = |k: &str| o.front_matter_order.iter().position(|x| x == k).unwrap_or(usize::MAX);
        let alpha = o.front_matter_sort == Sort::Alpha;
        list.sort_by(|a, b| {
            let (ra, rb) = (rank(&a.key), rank(&b.key));
            ra.cmp(&rb).then_with(|| {
                if alpha && ra == usize::MAX { a.key.to_lowercase().cmp(&b.key.to_lowercase()) } else { std::cmp::Ordering::Equal }
            })
        });
    }

    let mut body = String::new();
    let mut push = |(t, el): &(String, String)| {
        body.push_str(t);
        body.push_str(if el.is_empty() { eol } else { el });
    };
    for e in &list {
        e.lead.iter().for_each(&mut push);
        e.lines.iter().for_each(&mut push);
    }
    tail.iter().for_each(&mut push);
    if body == text[start..end] {
        return Result::Same;
    }
    // A blank first line would stop it being front matter at all.
    if body.lines().next().is_some_and(|l| l.trim().is_empty()) && !text[start..end].lines().next().is_some_and(|l| l.trim().is_empty()) {
        return Result::Same;
    }
    Result::Changed(format!("{}{body}{}", &text[..start], &text[end..]))
}

/// Set `key` to `stamp` in the front matter (adding the key, or the front
/// matter, if there is none).
pub(super) fn set_modified(text: &str, key: &str, stamp: &str, eol: &str) -> Result {
    match locate(text) {
        None => {
            let rest = if text.trim().is_empty() { String::new() } else { format!("{eol}{text}") };
            let fresh = format!("---{eol}{}: {}{eol}---{eol}{rest}", frontmatter::yaml_scalar(key), frontmatter::yaml_scalar(stamp));
            if frontmatter::parse(&fresh).is_some() { Result::Changed(fresh) } else { Result::Unreadable }
        }
        Some((start, end)) => {
            if frontmatter::parse(text).is_none() || entries(&text[start..end]).is_none() {
                return Result::Unreadable;
            }
            let edit = frontmatter::set_field(text, key, &Value::Str(stamp.to_string()));
            let new = edit.apply(text);
            if new == text { Result::Same } else { Result::Changed(new) }
        }
    }
}

fn normal_tags(fm: &frontmatter::FrontMatter) -> Vec<String> {
    let mut v: Vec<String> = fm.get("tags").map(|t| tag_items(t, TagsCase::Lower)).unwrap_or_default();
    v.sort();
    v
}

/// The front matter before and after means the same: the same keys and
/// values, and the same tags once written one way.
pub(super) fn same_meaning(before: &str, after: &str) -> bool {
    let (Some(a), Some(b)) = (frontmatter::parse(before), frontmatter::parse(after)) else { return false };
    let strip = |fm: &frontmatter::FrontMatter| {
        let mut f = fm.fields.clone();
        f.remove("tags");
        f
    };
    strip(&a) == strip(&b) && normal_tags(&a) == normal_tags(&b)
}
