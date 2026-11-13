// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Edges derived from naming schemes (0.13), as the Breadcrumbs plugin
//! makes them: tag notes, folder notes, list notes and Dendron-style names.
//!
//! They are built in the same pass as the written typed links
//! (`LinkGraph::build`), from data the records already hold (tags, paths,
//! front matter, and the source text of the few notes that are list notes).
//! A vault with none of these notes does one cheap range lookup per note.
//!
//! The front-matter keys are Breadcrumbs v4's, case as written:
//!
//! | Key | Meaning |
//! |---|---|
//! | `BC-tag-note-tag` | the tag (`#` optional); the note gets an edge to every note tagged with it or a nested tag |
//! | `BC-tag-note-field` | the edge field (default: the first `down` name) |
//! | `BC-tag-note-exact` | `true`: only notes with exactly that tag |
//! | `BC-folder-note-field` | makes the note a folder note: an edge to the other notes of its folder |
//! | `BC-folder-note-recurse` | `true`: and every folder below |
//! | `BC-list-note-field` | makes the note a list note: an edge to each link in its lists |
//! | `BC-list-note-exclude-index` | `true`: no edges from the note to its top-level items |
//! | `BC-list-note-neighbour-field` | an edge between consecutive items of one list |

use std::collections::{BTreeMap, HashMap};

use quire_markdown::fields::normalize_key;
use quire_vault::NotePath;

use crate::graph::Resolver;
use crate::record::NoteRecord;
use crate::relations::LinkConfig;

type Edge = crate::relations::DerivedEdge;

fn flag(rec: &NoteRecord, key: &str) -> bool {
    rec.properties.get(key).and_then(|v| v.as_str()).is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
}

fn text<'a>(rec: &'a NoteRecord, key: &str) -> Option<&'a str> {
    rec.properties.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

/// Whether the note has any `BC-…` key. `BTreeMap` order makes this one range lookup.
fn has_bc_keys(rec: &NoteRecord) -> bool {
    rec.properties
        .range::<str, _>((std::ops::Bound::Included("BC-"), std::ops::Bound::Unbounded))
        .next()
        .is_some_and(|(k, _)| k.starts_with("BC-"))
}

/// Every derived edge of the vault, in a fixed order.
pub(crate) fn derive<'a>(
    records: impl Iterator<Item = &'a NoteRecord> + Clone,
    source: &dyn Fn(&NotePath) -> Option<&'a str>,
    resolver: &Resolver,
    cfg: &LinkConfig,
) -> Vec<Edge> {
    let mut out: Vec<Edge> = Vec::new();
    let notes: Vec<&NoteRecord> = records.clone().filter(|r| has_bc_keys(r)).collect();
    let default_field = cfg.field_for(crate::Hier::Down);
    let mut tag_index: Option<BTreeMap<String, Vec<&NoteRecord>>> = None;
    let mut folder_index: Option<BTreeMap<&str, Vec<&NoteRecord>>> = None;
    for rec in &notes {
        // Tag notes.
        if let Some(tag) = text(rec, "BC-tag-note-tag") {
            let tag = tag.trim_start_matches('#').to_lowercase();
            let field = text(rec, "BC-tag-note-field").map_or_else(|| default_field.clone(), normalize_key);
            let exact = flag(rec, "BC-tag-note-exact");
            let by_tag = tag_index.get_or_insert_with(|| {
                // Each tag and its parents (`a/b` is under `a`) to the notes that have it.
                let mut m: BTreeMap<String, Vec<&NoteRecord>> = BTreeMap::new();
                for r in records.clone() {
                    for t in &r.tags {
                        let t = t.to_lowercase();
                        let mut end = t.len();
                        loop {
                            m.entry(t[..end].to_string()).or_default().push(r);
                            match t[..end].rfind('/') {
                                Some(i) => end = i,
                                None => break,
                            }
                        }
                    }
                }
                m
            });
            let hits = by_tag.get(&tag).into_iter().flatten();
            for r in hits {
                if r.path == rec.path || (exact && !r.tags.iter().any(|t| t.eq_ignore_ascii_case(&tag))) {
                    continue;
                }
                out.push((rec.path.clone(), field.clone(), r.path.clone(), rec.path.clone()));
            }
        }
        // Folder notes.
        if let Some(field) = text(rec, "BC-folder-note-field") {
            let field = normalize_key(field);
            let recurse = flag(rec, "BC-folder-note-recurse");
            let by_folder = folder_index.get_or_insert_with(|| {
                let mut m: BTreeMap<&str, Vec<&NoteRecord>> = BTreeMap::new();
                for r in records.clone() {
                    m.entry(r.path.folder()).or_default().push(r);
                }
                m
            });
            let folder = rec.path.folder();
            let below = |f: &str| {
                f == folder || (recurse && (folder.is_empty() || f.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/'))))
            };
            for (f, members) in &*by_folder {
                if below(f) {
                    out.extend(
                        members
                            .iter()
                            .filter(|r| r.path != rec.path)
                            .map(|r| (rec.path.clone(), field.clone(), r.path.clone(), rec.path.clone())),
                    );
                }
            }
        }
        // List notes.
        if let Some(field) = text(rec, "BC-list-note-field")
            && let Some(src) = source(&rec.path)
        {
            let neighbour = text(rec, "BC-list-note-neighbour-field").map(normalize_key);
            list_edges(rec, src, &normalize_key(field), neighbour.as_deref(), flag(rec, "BC-list-note-exclude-index"), resolver, &mut out);
        }
    }
    // Dendron names: `a.b.c` is under `a.b`.
    let d = &cfg.dendron;
    if d.enabled && !d.delimiter.is_empty() {
        let field = normalize_key(&d.field);
        let mut by_stem: HashMap<String, &NotePath> = HashMap::new();
        for r in records.clone() {
            let slot = by_stem.entry(r.path.stem().to_lowercase()).or_insert(&r.path);
            if r.path < **slot {
                *slot = &r.path;
            }
        }
        for r in records {
            let stem = r.path.stem();
            if let Some((parent, _)) = stem.rsplit_once(d.delimiter.as_str())
                && !parent.is_empty()
                && let Some(p) = by_stem.get(&parent.to_lowercase())
            {
                out.push((r.path.clone(), field.clone(), (*p).clone(), r.path.clone()));
            }
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    out
}

/// The wikilink target of the first `[[…]]` in `line`, and the text before it.
fn first_wikilink(line: &str) -> Option<(&str, &str)> {
    let open = line.find("[[")?;
    let rest = &line[open + 2..];
    let close = rest.find("]]")?;
    let inner = &rest[..close];
    let target = inner.split(['|', '#']).next().unwrap_or(inner).trim();
    (!target.is_empty()).then_some((&line[..open], target))
}

/// `- `, `* `, `+ ` or `1. ` at the start of a (trimmed) line; the rest.
fn list_item(trimmed: &str) -> Option<&str> {
    let t = trimmed.strip_prefix(['-', '*', '+']).or_else(|| {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0).then(|| trimmed[digits..].strip_prefix(['.', ')'])).flatten()
    })?;
    t.strip_prefix(' ').or_else(|| t.strip_prefix('\t'))
}

/// The field named before a link (`child [[B]]`, `child:: [[B]]`), if it is one word.
fn field_prefix(before: &str) -> Option<String> {
    let w = before.trim().trim_end_matches(':').trim();
    (!w.is_empty() && !w.contains(char::is_whitespace) && w.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_')))
        .then(|| normalize_key(w))
}

/// Where the body starts (after the front matter).
fn body_start(src: &str) -> usize {
    if !(src.starts_with("---\n") || src.starts_with("---\r\n")) {
        return 0;
    }
    let mut pos = src.find('\n').map_or(src.len(), |i| i + 1);
    for line in src[pos..].split_inclusive('\n') {
        pos += line.len();
        if matches!(line.trim_end(), "---" | "...") {
            return pos;
        }
    }
    0
}

/// Most list items read from one note (a runaway list stops here).
const LIST_CAP: usize = 5000;

fn list_edges(
    rec: &NoteRecord,
    src: &str,
    field: &str,
    neighbour: Option<&str>,
    exclude_index: bool,
    resolver: &Resolver,
    out: &mut Vec<Edge>,
) {
    // Open items by indent: (indent, note, field of its own edge); and the last note seen per indent, for neighbours.
    let mut stack: Vec<(usize, NotePath)> = Vec::new();
    let mut last_at: HashMap<usize, NotePath> = HashMap::new();
    let mut fence: Option<char> = None;
    let mut items = 0;
    for line in src[body_start(src)..].lines() {
        let trimmed = line.trim_start();
        if let Some(c) = trimmed.strip_prefix("```").map(|_| '`').or_else(|| trimmed.strip_prefix("~~~").map(|_| '~')) {
            fence = if fence == Some(c) { None } else { fence.or(Some(c)) };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let indent = line.chars().take_while(|c| matches!(c, ' ' | '\t')).map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
        let Some(rest) = list_item(trimmed) else {
            if trimmed.is_empty() {
                continue;
            }
            // Text that isn't a list item ends the lists above it.
            stack.clear();
            last_at.clear();
            continue;
        };
        let Some((before, target)) = first_wikilink(rest) else { continue };
        let Some(to) = resolver.resolve(target, Some(&rec.path)) else { continue };
        items += 1;
        if items > LIST_CAP {
            break;
        }
        let own = field_prefix(before).unwrap_or_else(|| field.to_string());
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        last_at.retain(|i, _| *i <= indent);
        match stack.last() {
            Some((_, parent)) => out.push((parent.clone(), own, to.clone(), rec.path.clone())),
            None if !exclude_index => out.push((rec.path.clone(), own, to.clone(), rec.path.clone())),
            None => {}
        }
        if let (Some(n), Some(prev)) = (neighbour, last_at.get(&indent)) {
            out.push((prev.clone(), n.to_string(), to.clone(), rec.path.clone()));
        }
        last_at.insert(indent, to.clone());
        stack.push((indent, to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recs(notes: &[(&str, &str)]) -> Vec<(NoteRecord, String)> {
        notes.iter().map(|(p, s)| (NoteRecord::from_source(NotePath::new(*p), s), (*s).to_string())).collect()
    }

    fn run(notes: &[(&str, &str)], cfg: &LinkConfig) -> Vec<(String, String, String)> {
        let rs = recs(notes);
        let resolver = Resolver::build(rs.iter().map(|(r, _)| r));
        let src: HashMap<NotePath, &str> = rs.iter().map(|(r, s)| (r.path.clone(), s.as_str())).collect();
        derive(rs.iter().map(|(r, _)| r), &|p| src.get(p).copied(), &resolver, cfg)
            .into_iter()
            .map(|(a, f, b, _)| (a.as_str().to_string(), f, b.as_str().to_string()))
            .collect()
    }

    fn e(a: &str, f: &str, b: &str) -> (String, String, String) {
        (a.to_string(), f.to_string(), b.to_string())
    }

    #[test]
    fn tag_notes() {
        let n = [
            ("Index.md", "---\nBC-tag-note-tag: '#topic'\nBC-tag-note-field: down\n---\n"),
            ("A.md", "#topic\n"),
            ("B.md", "#topic/sub\n"),
            ("C.md", "#other\n"),
        ];
        assert_eq!(run(&n, &LinkConfig::default()), [e("Index.md", "down", "A.md"), e("Index.md", "down", "B.md")]);
        let exact =
            [("Index.md", "---\nBC-tag-note-tag: topic\nBC-tag-note-exact: true\n---\n"), ("A.md", "#topic\n"), ("B.md", "#topic/sub\n")];
        assert_eq!(run(&exact, &LinkConfig::default()), [e("Index.md", "down", "A.md")], "field defaults to down, exact skips nested tags");
    }

    #[test]
    fn folder_notes() {
        let n = [("Proj/Proj.md", "---\nBC-folder-note-field: down\n---\n"), ("Proj/A.md", ""), ("Proj/Deep/B.md", ""), ("Other/C.md", "")];
        assert_eq!(run(&n, &LinkConfig::default()), [e("Proj/Proj.md", "down", "Proj/A.md")]);
        let r = [
            ("Proj/Proj.md", "---\nBC-folder-note-field: down\nBC-folder-note-recurse: true\n---\n"),
            ("Proj/A.md", ""),
            ("Proj/Deep/B.md", ""),
            ("Projects/C.md", ""),
        ];
        assert_eq!(run(&r, &LinkConfig::default()), [e("Proj/Proj.md", "down", "Proj/A.md"), e("Proj/Proj.md", "down", "Proj/Deep/B.md")]);
    }

    #[test]
    fn list_notes() {
        let src = "---\nBC-list-note-field: down\nBC-list-note-neighbour-field: next\n---\n# Plan\n\n- [[A]]\n  - [[A1]]\n  - child [[A2]]\n- [[B]]\n\n```\n- [[Hidden]]\n```\n";
        let n = [("Plan.md", src), ("A.md", ""), ("A1.md", ""), ("A2.md", ""), ("B.md", ""), ("Hidden.md", "")];
        let got = run(&n, &LinkConfig::default());
        assert_eq!(
            got,
            [
                e("A.md", "child", "A2.md"),
                e("A.md", "down", "A1.md"),
                e("A.md", "next", "B.md"),
                e("A1.md", "next", "A2.md"),
                e("Plan.md", "down", "A.md"),
                e("Plan.md", "down", "B.md"),
            ]
        );
        let ex = [
            ("Plan.md", "---\nBC-list-note-field: down\nBC-list-note-exclude-index: true\n---\n- [[A]]\n  - [[B]]\n"),
            ("A.md", ""),
            ("B.md", ""),
        ];
        assert_eq!(run(&ex, &LinkConfig::default()), [e("A.md", "down", "B.md")]);
    }

    #[test]
    fn dendron_names() {
        let cfg = LinkConfig { dendron: crate::relations::Dendron { enabled: true, ..Default::default() }, ..LinkConfig::default() };
        let n = [("a.md", ""), ("a.b.md", ""), ("a.b.c.md", ""), ("x.y.md", "")];
        assert_eq!(run(&n, &cfg), [e("a.b.c.md", "up", "a.b.md"), e("a.b.md", "up", "a.md")], "a missing parent gives no edge");
        assert!(run(&n, &LinkConfig::default()).is_empty(), "off by default");
    }
}
