// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `neighbors`: links in and out of a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "neighbors",
        "Linked notes",
        "Notes linked to or from a note, up to 3 links away, with the line that links them and the link's type (the field it is in: `up:: [[X]]`, `supports: \"[[Y]]\"`; null for a plain link). Example: {\"path\": \"Rope Notes.md\", \"depth\": 2, \"direction\": \"in\"}; only some types: {\"path\": \"Claim.md\", \"type\": [\"supports\", \"contradicts\"]}.",
        schema!({"type": "object", "properties": {
                "path": {"type": "string"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1},
                "direction": {"type": "string", "enum": ["in", "out", "both"], "default": "both"},
                "type": {"type": "array", "items": {"type": "string"}, "description": "Follow only links of these types (\"untyped\" for plain links)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {
                "nodes": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "title": {"type": "string"}, "depth": {"type": "integer"}}}},
                "edges": {"type": "array", "items": {"type": "object", "properties": {"from": {"type": "string"}, "to": {"type": "string"}, "context": {"type": "string"}, "type": {"type": ["string", "null"]}}}}},
                "required": ["nodes", "edges"]}),
        r,
        Access::Read,
        neighbors,
    )
}

fn neighbors(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let start = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let depth = int(a, "depth", 1, 1, 3);
    let limit = int(a, "limit", 50, 1, 200);
    let dir = s(a, "direction");
    let (want_in, want_out) = match dir.as_str() {
        "in" => (true, false),
        "out" => (false, true),
        _ => (true, true),
    };
    // 0.12: only links of these types (`untyped` for plain ones).
    let types: Option<BTreeSet<String>> = {
        let t: BTreeSet<String> = list(a, "type").iter().map(|x| quire_app::markdown::fields::normalize_key(x)).collect();
        (!t.is_empty()).then_some(t)
    };
    let keep = |field: Option<&String>| types.as_ref().is_none_or(|t| t.contains(field.map_or("untyped", String::as_str)));
    let ix = cx.index.read();
    let mut seen: HashMap<NotePath, usize> = HashMap::from([(start.clone(), 0)]);
    let mut queue = VecDeque::from([start.clone()]);
    let mut edges: Vec<Value> = Vec::new();
    let mut nodes: Vec<Value> = Vec::new();
    while let Some(p) = queue.pop_front() {
        let d = seen[&p];
        if d >= depth || nodes.len() >= limit {
            continue;
        }
        let mut next: Vec<(NotePath, Value)> = Vec::new();
        if want_in {
            for b in ix.backlinks(&p).iter().filter(|b| keep(b.link.field.as_ref())) {
                next.push((b.from.clone(), json!({"from": b.from, "to": p, "context": b.context, "type": b.link.field})));
            }
        }
        if want_out {
            for o in ix.graph().outgoing(&p) {
                let Some(b) = ix.backlinks(o).iter().find(|b| b.from == p && keep(b.link.field.as_ref())) else { continue };
                next.push((o.clone(), json!({"from": p, "to": o, "context": b.context, "type": b.link.field})));
            }
        }
        for (n, edge) in next {
            if !visible(cx, &n) {
                continue;
            }
            edges.push(edge);
            if !seen.contains_key(&n) && nodes.len() < limit {
                seen.insert(n.clone(), d + 1);
                let title = ix.get(&n).map_or_else(|| n.stem().to_string(), |r| r.title.clone());
                nodes.push(json!({"path": n, "title": title, "depth": d + 1}));
                queue.push_back(n);
            }
        }
    }
    let mut text = format!("Notes linked with {start} ({} found):\n", nodes.len());
    for n in &nodes {
        let _ =
            writeln!(text, "- {} ({} link{} away)", n["path"].as_str().unwrap_or(""), n["depth"], if n["depth"] == 1 { "" } else { "s" });
    }
    let links = nodes.iter().filter_map(|n| n["path"].as_str()).map(|p| note_link(&NotePath::new(p))).collect();
    ToolOutput::ok(text.trim_end().to_string(), json!({"nodes": nodes, "edges": edges})).with_links(links)
}
