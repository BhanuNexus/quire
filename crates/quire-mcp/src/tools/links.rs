// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.12 typed links for agents.
//!
//! `links` (a note's links and backlinks with their types), `hierarchy`
//! (ancestors, descendants and siblings, depth limited) and `add_link` (a
//! typed link through the normal write path: applied in agent zones, a
//! suggestion the person reviews elsewhere).
//!
//! Every note these tools name passes the policy guard (`visible`): a note
//! the client may not see is never listed, and walks never pass through it,
//! so a hidden note can't show up as a gap in a chain either.

use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;

use quire_app::index::{Hier, Index, Relations};
use quire_app::vault::NotePath;
use serde_json::{Value, json};

use super::common::{existing, int, list, note_link, outcome_schema, req, s, tool, visible};
use super::{Access, Annotations, CallExt, Tool, ToolCx, ToolOutput};
use crate::edit::EditOp;

pub fn tools() -> Vec<Box<dyn Tool>> {
    let r = Annotations::READ;
    vec![
        tool(
            "links",
            "Typed links of a note",
            "A note's links and backlinks with their types. A typed link is a link in a field: `parent:: [[X]]`, `[supports:: [[Y]]]` or a property `up: \"[[X]]\"`. Hierarchy types (up, down, same, next, prev, and the names the vault maps to them such as parent and child) imply their reverse: if A is `up` B, B lists A as `down` with implied: true. Example: {\"path\": \"Claims/Ropes are fast.md\", \"type\": [\"supports\", \"contradicts\"], \"direction\": \"in\"}. Use type \"untyped\" for plain links.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "type": {"type": "array", "items": {"type": "string"}, "description": "Only these types (a string is one type); \"untyped\" for plain links."},
                "direction": {"type": "string", "enum": ["in", "out", "both"], "default": "both"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100}}, "required": ["path"]}),
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "links": {"type": "array", "items": {"type": "object", "properties": {
                    "direction": {"type": "string", "enum": ["in", "out"]}, "type": {"type": ["string", "null"]},
                    "hierarchy": {"type": ["string", "null"], "enum": ["up", "down", "same", "next", "prev", null]},
                    "implied": {"type": "boolean"}, "path": {"type": "string"}, "title": {"type": "string"}}}},
                "types": {"type": "array", "items": {"type": "string"}, "description": "Every link type used in the vault (that you can see)."},
                "truncated": {"type": "boolean"}}, "required": ["path", "links"]}),
            r,
            Access::Read,
            links,
        ),
        tool(
            "hierarchy",
            "Hierarchy around a note",
            "Where a note sits in the vault's hierarchy (from up/down/same/next/prev links and the names the vault maps to them): its ancestors (parents, grandparents…), descendants and siblings, each with its distance, plus the paths from the top down to the note. Cycles are followed once. Example: {\"path\": \"Maths/Limits.md\", \"depth\": 3, \"include\": [\"ancestors\", \"siblings\"]}.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 10, "default": 3},
                "include": {"type": "array", "items": {"type": "string", "enum": ["ancestors", "descendants", "siblings", "trails"]},
                    "description": "Default: all four."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 200}}, "required": ["path"]}),
            json!({"type": "object", "properties": {
                "path": {"type": "string"}, "title": {"type": "string"},
                "ancestors": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "title": {"type": "string"}, "depth": {"type": "integer"}, "via": {"type": "string"}}}},
                "descendants": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "title": {"type": "string"}, "depth": {"type": "integer"}, "via": {"type": "string"}}}},
                "siblings": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "title": {"type": "string"}}}},
                "previous": {"type": ["string", "null"]}, "next": {"type": ["string", "null"]},
                "trails": {"type": "array", "items": {"type": "array", "items": {"type": "string"}}},
                "truncated": {"type": "boolean"}}, "required": ["path"]}),
            r,
            Access::Read,
            hierarchy,
        ),
        tool(
            "add_link",
            "Add a typed link",
            "Add a typed link from a note to another, written the way the note already writes them: an inline field (`supports:: [[Target]]`) or a property (`supports: \"[[Target]]\"`). Like every write it applies in your zones and becomes a suggestion the person reviews elsewhere; nothing changes when the link is already there. Example: {\"path\": \"Claims/Ropes are fast.md\", \"type\": \"supports\", \"target\": \"Benchmarks 2026\", \"expected_version\": \"…\"}.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "The note that gets the link."},
                "type": {"type": "string", "description": "up, down, same, next, prev, a name the vault maps to them (parent, child…), or any other type (supports, contradicts, source…)."},
                "target": {"type": "string", "description": "The note to link to, by path or name."},
                "expected_version": {"type": "string"},
                "run": {"type": "string"}}, "required": ["path", "type", "target"]}),
            outcome_schema(),
            Annotations::WRITE_IDEMPOTENT,
            Access::Write,
            add_link,
        ),
    ]
}

fn title(ix: &Index, p: &NotePath) -> String {
    ix.get(p).map_or_else(|| p.stem().to_string(), |r| r.title.clone())
}

/// Types the caller asked for (`None`: all). `"untyped"` stands for plain links.
fn wanted(a: &Value) -> Option<HashSet<String>> {
    let v: HashSet<String> =
        list(a, "type").iter().map(|t| quire_app::markdown::fields::normalize_key(t)).filter(|t| !t.is_empty()).collect();
    (!v.is_empty()).then_some(v)
}

fn links(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let p = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let limit = int(a, "limit", 100, 1, 500);
    let dir = s(a, "direction");
    let (want_in, want_out) = match dir.as_str() {
        "in" => (true, false),
        "out" => (false, true),
        _ => (true, true),
    };
    let types = wanted(a);
    let keep = |t: Option<&str>| types.as_ref().is_none_or(|w| w.contains(t.unwrap_or("untyped")));
    let ix = cx.index.read();
    let rel = ix.relations();
    let mut rows: Vec<Value> = Vec::new();
    let mut seen: HashSet<(bool, Option<String>, NotePath)> = HashSet::new();
    let mut push = |rows: &mut Vec<Value>, incoming: bool, field: Option<&str>, hier: Option<Hier>, implied: bool, other: &NotePath| {
        if !keep(field) || !visible(cx, other) || !seen.insert((incoming, field.map(str::to_string), other.clone())) {
            return;
        }
        rows.push(json!({
            "direction": if incoming { "in" } else { "out" }, "type": field, "hierarchy": hier.map(Hier::key),
            "implied": implied, "path": other, "title": title(&ix, other)}));
    };
    if want_out {
        for e in rel.of(&p).iter().filter(|e| e.visible(&|q| visible(cx, q))) {
            push(&mut rows, false, Some(&e.field), e.hier, e.implied, &e.to);
        }
        // Plain links out.
        for o in ix.graph().outgoing(&p) {
            let plain = ix.backlinks(o).iter().any(|b| b.from == p && b.link.field.is_none());
            if plain {
                push(&mut rows, false, None, None, false, o);
            }
        }
    }
    if want_in {
        for b in ix.backlinks(&p) {
            let hier = b.link.field.as_deref().and_then(|f| ix.link_config().kind(f));
            push(&mut rows, true, b.link.field.as_deref(), hier, false, &b.from);
        }
    }
    let total = rows.len();
    rows.truncate(limit);
    // Types in notes the caller may see (a hidden note's field names stay hidden too).
    let mut all_types: Vec<String> = Vec::new();
    for r in ix.records().filter(|r| visible(cx, &r.path)) {
        for f in r.links.iter().filter_map(|l| l.field.as_ref()) {
            if !all_types.contains(f) {
                all_types.push(f.clone());
            }
        }
    }
    all_types.sort();
    let mut text = format!("Links of {p} ({total}):\n");
    for r in &rows {
        let arrow = if r["direction"] == "in" { "←" } else { "→" };
        let t = r["type"].as_str().unwrap_or("link");
        let implied = if r["implied"] == true { " (implied)" } else { "" };
        let _ = writeln!(text, "- {arrow} {t}{implied}: {}", r["path"].as_str().unwrap_or_default());
    }
    let notes = rows.iter().filter_map(|r| r["path"].as_str()).map(|q| note_link(&NotePath::new(q))).collect();
    ToolOutput::ok(text.trim_end().to_string(), json!({"path": p, "links": rows, "types": all_types, "truncated": total > limit}))
        .with_links(notes)
}

/// Notes reached from `start` along `h`, through visible notes only.
fn walk(cx: &ToolCx<'_>, ix: &Index, start: &NotePath, h: Hier, depth: usize, limit: usize) -> (Vec<Value>, bool) {
    let rel = ix.relations();
    let mut seen: HashSet<NotePath> = HashSet::from([start.clone()]);
    let mut queue = VecDeque::from([(start.clone(), 0usize)]);
    let mut out = Vec::new();
    let mut cut = false;
    while let Some((p, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        for n in rel.related_where(&p, h, &|q| visible(cx, q)) {
            if !visible(cx, n) || !seen.insert(n.clone()) {
                continue;
            }
            if out.len() >= limit {
                cut = true;
                break;
            }
            out.push(json!({"path": n, "title": title(ix, n), "depth": d + 1, "via": p}));
            queue.push_back((n.clone(), d + 1));
        }
    }
    (out, cut)
}

/// Siblings of `p` the caller may know about: its explicit `same` links, then the
/// other children of its *visible* parents. Two notes that share only a parent the
/// caller can't see aren't reported as siblings (that would reveal the parent).
fn visible_siblings(cx: &ToolCx<'_>, rel: &Relations, p: &NotePath) -> Vec<NotePath> {
    let mut v: Vec<NotePath> = rel.related_where(p, Hier::Same, &|q| visible(cx, q)).into_iter().cloned().collect();
    for parent in rel.related_where(p, Hier::Up, &|q| visible(cx, q)) {
        let mut kids: Vec<&NotePath> = rel.related_where(parent, Hier::Down, &|q| visible(cx, q));
        kids.sort();
        for k in kids {
            if k != p && visible(cx, k) && !v.contains(k) {
                v.push(k.clone());
            }
        }
    }
    v
}

/// The note before or after `p`: its explicit `prev`/`next` link, else its neighbour
/// by name under its first visible parent (hidden notes aren't counted).
fn visible_step(cx: &ToolCx<'_>, rel: &Relations, p: &NotePath, forward: bool) -> Option<NotePath> {
    let h = if forward { Hier::Next } else { Hier::Prev };
    if let Some(n) = rel.related_where(p, h, &|q| visible(cx, q)).first() {
        return Some((*n).clone()).filter(|q| visible(cx, q));
    }
    let parent = *rel.related_where(p, Hier::Up, &|q| visible(cx, q)).first()?;
    let mut kids: Vec<&NotePath> = rel.related_where(parent, Hier::Down, &|q| visible(cx, q));
    kids.sort_by_key(|k| k.stem().to_lowercase());
    let i = kids.iter().position(|k| *k == p)?;
    let j = if forward { i + 1 } else { i.checked_sub(1)? };
    kids.get(j).map(|k| (*k).clone())
}

fn hierarchy(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let p = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let depth = int(a, "depth", 3, 1, 10);
    let limit = int(a, "limit", 200, 1, 500);
    let include = list(a, "include");
    let want = |k: &str| include.is_empty() || include.iter().any(|i| i == k);
    let ix = cx.index.read();
    let rel = ix.relations();
    let mut st = json!({"path": p, "title": title(&ix, &p)});
    let mut truncated = false;
    let mut text = format!("{p} in the hierarchy:\n");
    for (key, h) in [("ancestors", Hier::Up), ("descendants", Hier::Down)] {
        if want(key) {
            let (rows, cut) = walk(cx, &ix, &p, h, depth, limit);
            truncated |= cut;
            let names: Vec<&str> = rows.iter().filter_map(|r| r["path"].as_str()).collect();
            let _ = writeln!(text, "{key}: {}", if names.is_empty() { "none".to_string() } else { names.join(", ") });
            st[key] = rows.into();
        }
    }
    if want("siblings") {
        let sibs: Vec<Value> =
            visible_siblings(cx, rel, &p).iter().take(limit).map(|q| json!({"path": q, "title": title(&ix, q)})).collect();
        let names: Vec<&str> = sibs.iter().filter_map(|r| r["path"].as_str()).collect();
        let _ = writeln!(text, "siblings: {}", if names.is_empty() { "none".to_string() } else { names.join(", ") });
        st["siblings"] = sibs.into();
        let step = |forward: bool| visible_step(cx, rel, &p, forward);
        st["previous"] = json!(step(false));
        st["next"] = json!(step(true));
    }
    if want("trails") {
        // Only paths every note of which the caller may see.
        let trails: Vec<Vec<NotePath>> = rel.trails_where(&p, 8, 32, &|q| visible(cx, q));
        for t in &trails {
            let _ = writeln!(text, "trail: {}", t.iter().map(NotePath::stem).collect::<Vec<_>>().join(" › "));
        }
        st["trails"] = json!(trails);
    }
    st["truncated"] = truncated.into();
    ToolOutput::ok(text.trim_end().to_string(), st)
}

fn add_link(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let (field, target) = match (req(a, "type"), req(a, "target")) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let from = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    // The target must be a note the caller may see (a hidden one reads as missing).
    let to = match existing(cx, &json!({"target": target}), "target") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let name = {
        let ix = cx.index.read();
        if ix.resolve(to.stem(), Some(&from)).as_ref() == Some(&to) { to.stem().to_string() } else { to.without_ext().to_string() }
    };
    let mut args = a.clone();
    args["path"] = json!(from);
    super::write::edit(cx, ext, &args, "add_link", &EditOp::TypedLink { field, target: name })
}
