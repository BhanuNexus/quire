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
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
                "max_tokens": {"type": "integer"}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {
                "nodes": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "title": {"type": "string"}, "depth": {"type": "integer"}}}},
                "edges": {"type": "array", "items": {"type": "object", "properties": {"from": {"type": "string"}, "to": {"type": "string"}, "context": {"type": "string"}, "type": {"type": ["string", "null"]}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"}},
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
    let vis = cx.visibility_in(&ix);
    let mut seen: HashMap<NotePath, usize> = HashMap::from([(start.clone(), 0)]);
    let mut queue = VecDeque::from([start.clone()]);
    let mut edges: Vec<Value> = Vec::new();
    let mut nodes: Vec<Value> = Vec::new();
    // Every note within `depth`, for `total`; the first `limit` of them are shown.
    while let Some(p) = queue.pop_front() {
        let d = seen[&p];
        if d >= depth {
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
            if !vis.allows(&n) {
                continue;
            }
            edges.push(edge);
            if !seen.contains_key(&n) {
                seen.insert(n.clone(), d + 1);
                let title = ix.get(&n).map_or_else(|| n.stem().to_string(), |r| r.title.clone());
                nodes.push(json!({"path": n, "title": title, "depth": d + 1}));
                queue.push_back(n);
            }
        }
    }
    let total = nodes.len();
    nodes.truncate(limit);
    // Within `max_tokens`: the nodes first, then the edges between notes still shown.
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let node_texts: Vec<String> = nodes.iter().map(Value::to_string).collect();
    let kept = bounds::fit(node_texts.iter().map(String::as_str), budget);
    nodes.truncate(kept);
    let used: usize = node_texts.iter().take(kept).map(|t| bounds::est_tokens(t) + 1).sum();
    let shown: HashSet<&str> = nodes.iter().filter_map(|n| n["path"].as_str()).chain([start.as_str()]).collect();
    edges.retain(|e| shown.contains(e["from"].as_str().unwrap_or_default()) && shown.contains(e["to"].as_str().unwrap_or_default()));
    let between = edges.len();
    let mut left = budget.saturating_sub(used);
    edges.retain(|e| {
        let t = bounds::est_tokens(&e.to_string()) + 1;
        let fits = t <= left;
        left = left.saturating_sub(t);
        fits
    });
    let truncated = total > nodes.len() || edges.len() < between;
    let mut text = format!("Notes linked with {start} ({} found):\n", nodes.len());
    for n in &nodes {
        writeln!(text, "- {} ({} link{} away)", n["path"].as_str().unwrap_or(""), n["depth"], if n["depth"] == 1 { "" } else { "s" })
            .expect("a String takes any text");
    }
    let links = nodes.iter().filter_map(|n| n["path"].as_str()).map(|p| note_link(&NotePath::new(p))).collect();
    if truncated {
        write!(text, "\n… cut at limit or max_tokens: {total} notes found").expect("a String takes any text");
    }
    ToolOutput::ok(text.trim_end().to_string(), json!({"nodes": nodes, "edges": edges, "total": total, "truncated": truncated}))
        .with_links(links)
}
