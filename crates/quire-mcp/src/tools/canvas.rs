// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `canvas_read` and `canvas_apply` (design §G.1, §4.2). Owned by WS-G;
//! registered after WS-B's read tools in `ToolRegistry::standard`.
//!
//! A canvas is a JSON Canvas file. Agents read its nodes and edges (text
//! cut to what fits in each node and to the token budget) and change it with
//! operations by id. The result is written in Quire's one-node-per-line
//! form through `AgentStore::submit`, so outside agent zones it is a
//! proposal the person reviews node by node.

use quire_agent::{Change, ChangeSet, Mode, RunId, SubmitOutcome, WriteError, canonical_path};
use quire_app::canvas_state::canvas::{self as qc, Canvas, NodeKind, ops};
use quire_app::vault::{ContentHash, NotePath};
use serde_json::{Value, json};

use super::common::{int, note_link, note_uri, opt, outcome, outcome_schema, req, tool, version_arg, visible, write_error};
use super::{Access, Annotations, CallExt, Tool, ToolCx, ToolOutput};

/// The canvas tools, in their fixed order.
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        tool(
            "canvas_read",
            "Read a canvas",
            "Read a JSON Canvas file (.canvas): its nodes (text, file, link, group) with positions and sizes, and its edges. Long text is cut to what fits in its node and to max_tokens. Use the ids with canvas_apply. Example: {\"path\": \"Boards/Ropes.canvas\"}.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Vault path of the .canvas file."},
                "max_tokens": {"type": "integer", "minimum": 200, "maximum": 20000, "default": 4000}}, "required": ["path"]}),
            json!({"type": "object", "properties": {
                "path": {"type": "string"}, "version": {"type": "string"}, "uri": {"type": "string"},
                "nodes": {"type": "array", "items": {"type": "object"}}, "edges": {"type": "array", "items": {"type": "object"}},
                "truncated": {"type": "boolean"}}, "required": ["path", "version", "nodes", "edges"]}),
            Annotations::READ,
            Access::Read,
            canvas_read,
        ),
        tool(
            "canvas_apply",
            "Change a canvas",
            "Change a .canvas file with operations, applied in order, all or nothing: add_node {node: {type, text|file|url|label, x, y, width, height, color}}, update_node {id, set: {…}} (null removes a key), remove_node {id} (its edges go too), add_edge {edge: {fromNode, toNode, fromSide, toSide, toEnd, label}}, update_edge {id, set}, remove_edge {id}. Colours are \"1\"–\"6\" or #RRGGBB; sides top/right/bottom/left. New ids are returned. A missing file is created. Outside agent zones the change is a suggestion the person reviews node by node. Example: {\"path\": \"Boards/Ropes.canvas\", \"ops\": [{\"op\": \"add_node\", \"node\": {\"type\": \"text\", \"text\": \"Undo is cheap\", \"x\": 0, \"y\": 200}}], \"expected_version\": \"sha256:…\"}.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "ops": {"type": "array", "items": {"type": "object", "properties": {"op": {"type": "string", "enum": ["add_node", "update_node", "remove_node", "add_edge", "update_edge", "remove_edge"]}}, "required": ["op"]}},
                "expected_version": {"type": "string", "description": "The version from canvas_read. If the canvas changed since, the operations are applied to the new version when they still fit."},
                "run": {"type": "string", "description": "Optional run id from begin_run."}}, "required": ["path", "ops"]}),
            {
                let mut s = outcome_schema();
                s["properties"]["ids"] = json!({"type": "array", "items": {"type": "string"}});
                s["properties"]["changes"] = json!({"type": "array", "items": {"type": "string"}});
                s
            },
            Annotations::WRITE,
            Access::Write,
            canvas_apply,
        ),
    ]
}

/// The canvas path an agent named (`.canvas` added when missing), checked.
fn canvas_path(a: &Value) -> Result<NotePath, ToolOutput> {
    let raw = req(a, "path")?;
    let mut p = raw.trim().trim_start_matches("./").to_string();
    if !p.to_lowercase().ends_with(".canvas") {
        p.push_str(".canvas");
    }
    canonical_path(&NotePath::new(p)).map_err(|e| write_error(&e))
}

fn not_found(p: &NotePath) -> ToolOutput {
    ToolOutput::Err {
        code: "not_found",
        message: format!("No canvas at {p}. Use `list` or `search` to find it, or canvas_apply to make a new one."),
        structured: json!({"error": "not_found", "path": p}),
    }
}

fn unreadable(p: &NotePath, why: &str) -> ToolOutput {
    ToolOutput::Err {
        code: "invalid_canvas",
        message: format!("{p} isn't a JSON Canvas file Quire can read ({why}); it was left unchanged."),
        structured: json!({"error": "invalid_canvas", "path": p, "reason": why}),
    }
}

/// About how much text fits in a node of this size at the default font.
fn fits(w: i64, h: i64) -> usize {
    let lines = (h.max(20) / 24).max(1);
    let cols = (w.max(40) / 8).max(5);
    usize::try_from(lines * cols).unwrap_or(usize::MAX)
}

fn cut(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    (t, true)
}

fn canvas_read(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match canvas_path(a) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !cx.vault.exists(&path) || !visible(cx, &path) {
        return not_found(&path);
    }
    let Ok(text) = cx.vault.read(&path) else { return not_found(&path) };
    let c = match qc::parse(&text) {
        Ok(c) => c,
        Err(e) => return unreadable(&path, &e),
    };
    let version = ContentHash::of(&text);
    // About four characters per token.
    let mut budget = int(a, "max_tokens", 4000, 200, 20_000) * 4;
    let mut truncated = false;
    let mut nodes = Vec::new();
    let mut lines = vec![format!("# Canvas: {path} ({} nodes, {} edges, version {version})", c.nodes.len(), c.edges.len())];
    for n in &c.nodes {
        if budget < 80 {
            truncated = true;
            break;
        }
        let mut v = json!({"id": n.id, "type": n.kind, "x": n.x, "y": n.y, "width": n.width, "height": n.height});
        if let Some(col) = &n.color {
            v["color"] = col.clone().into();
        }
        let shown = match n.node_kind() {
            NodeKind::Text => {
                let (t, cut_off) = cut(n.text.as_deref().unwrap_or_default(), fits(n.width, n.height).min(budget));
                truncated |= cut_off;
                v["text"] = t.clone().into();
                if cut_off {
                    v["text_truncated"] = true.into();
                }
                t
            }
            NodeKind::File => {
                v["file"] = n.file.clone().unwrap_or_default().into();
                if let Some(s) = &n.subpath {
                    v["subpath"] = s.clone().into();
                }
                format!("[[{}{}]]", n.file.as_deref().unwrap_or_default(), n.subpath.as_deref().unwrap_or_default())
            }
            NodeKind::Link => {
                v["url"] = n.url.clone().unwrap_or_default().into();
                n.url.clone().unwrap_or_default()
            }
            NodeKind::Group => {
                v["label"] = n.label.clone().unwrap_or_default().into();
                n.label.clone().unwrap_or_default()
            }
            NodeKind::Other => String::new(),
        };
        let line = format!("- {} {} at ({}, {}) {}×{}: {}", n.kind, n.id, n.x, n.y, n.width, n.height, shown.replace('\n', " ⏎ "));
        budget = budget.saturating_sub(line.len());
        lines.push(line);
        nodes.push(v);
    }
    let mut edges = Vec::new();
    for e in &c.edges {
        if budget < 40 {
            truncated = true;
            break;
        }
        let v = serde_json::to_value(e).unwrap_or(Value::Null);
        let line = format!(
            "- edge {}: {} → {}{}",
            e.id,
            e.from_node,
            e.to_node,
            e.label.as_deref().map(|l| format!(" ({l})")).unwrap_or_default()
        );
        budget = budget.saturating_sub(line.len());
        lines.push(line);
        edges.push(v);
    }
    if truncated {
        lines.push("(Cut to fit; ask with a larger max_tokens to see more.)".into());
    }
    let structured =
        json!({"path": path, "version": version, "uri": note_uri(&path), "nodes": nodes, "edges": edges, "truncated": truncated});
    ToolOutput::ok(lines.join("\n"), structured).with_links(vec![note_link(&path)])
}

fn canvas_apply(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match canvas_path(a) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(op_list) = a.get("ops").and_then(Value::as_array).filter(|o| !o.is_empty()) else {
        return ToolOutput::error("invalid_args", "`ops` must be a non-empty list of operations.");
    };
    let expected = match version_arg(a, "expected_version") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let run = match opt(a, "run").map(|r| RunId::parse(&r).ok_or(r)) {
        None => None,
        Some(Ok(r)) => Some(r),
        Some(Err(r)) => return ToolOutput::error("invalid_args", format!("`run` must be an id from begin_run (r_…), got {r}")),
    };
    if !visible(cx, &path) {
        return write_error(&WriteError::PermissionDenied { path, zone: "private".into(), mode: Mode::Deny });
    }
    let exists = cx.vault.exists(&path);
    let current = if exists {
        match cx.vault.read(&path) {
            Ok(t) => t,
            Err(_) => return not_found(&path),
        }
    } else {
        String::new()
    };
    let current_v = ContentHash::of(&current);
    let before = match qc::parse(&current) {
        Ok(c) => c,
        Err(e) => return unreadable(&path, &e),
    };
    let mut after: Canvas = before.clone();
    let mut seed = u64::try_from(cx.agent.now_ms()).unwrap_or(1) ^ 0x9e37_79b9_7f4a_7c15;
    let ids = match ops::apply(&mut after, op_list, &mut seed) {
        Ok(ids) => ids,
        Err(e) => {
            let stale = expected.as_ref().is_some_and(|v| *v != current_v);
            if stale {
                // Made against an older version and no longer fits.
                return write_error(&WriteError::VersionConflict {
                    path,
                    expected: expected.unwrap_or_else(|| current_v.clone()),
                    current: current_v,
                    diff_since: String::new(),
                    merged_preview: None,
                });
            }
            return ToolOutput::Err {
                code: "invalid_args",
                message: format!("{e}. Nothing was changed."),
                structured: json!({"error": "invalid_args", "op_index": e.index, "reason": e.message}),
            };
        }
    };
    let changes: Vec<String> = ops::diff(&before, &after).iter().map(ops::Change::describe).collect();
    let new_text = qc::to_canonical(&after);
    let mut extra = json!({"ids": ids, "changes": changes});
    if expected.as_ref().is_some_and(|v| *v != current_v) {
        extra["rebased"] = true.into();
    }
    if exists && new_text == current {
        let msg = "No change: the canvas already looks like this.";
        let mut st =
            json!({"outcome": "applied", "versions": [{"path": path, "version": current_v}], "uri": note_uri(&path), "message": msg});
        if let (Some(o), Value::Object(e)) = (st.as_object_mut(), extra) {
            o.extend(e);
        }
        return ToolOutput::ok(msg, st);
    }
    let change = if exists { Change::Edit { path: path.clone(), new_text } } else { Change::Create { path: path.clone(), text: new_text } };
    let title = match changes.as_slice() {
        [one] => format!("{one} on {}", path.stem()),
        _ => format!("Change canvas {} ({} changes)", path.stem(), changes.len()),
    };
    let cs = ChangeSet {
        changes: vec![change],
        run,
        title,
        expected: if exists { vec![(path.clone(), current_v)] } else { Vec::new() },
        ..ChangeSet::default()
    };
    match cx.agent.submit(cx.actor, cs) {
        Ok(out) => {
            let mut o = outcome(&path, &out, extra);
            if let (ToolOutput::Ok { text, .. }, SubmitOutcome::Applied { .. } | SubmitOutcome::Proposed { .. }) = (&mut o, &out) {
                for c in &changes {
                    text.push_str("\n- ");
                    text.push_str(c);
                }
            }
            o
        }
        Err(e) => write_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use quire_agent::{Actor, AgentStore, DeviceInfo, FixedClock, ProposalFilter, Via};
    use quire_app::index::IndexService;
    use quire_app::intel::chat::CancelFlag;
    use quire_app::vault::Vault;

    use super::*;

    const BOARD: &str = r##"{"nodes":[
{"id":"a","type":"text","x":0,"y":0,"width":250,"height":60,"text":"# Ropes\nBalanced trees of strings, with a very long explanation that goes on and on and on and on and on and on and on and on and on."},
{"id":"b","type":"file","x":300,"y":0,"width":400,"height":400,"file":"Rope Notes.md"}
],"edges":[
{"id":"e","fromNode":"a","toNode":"b","label":"see"}
]}
"##;

    fn store(vault: &Vault) -> AgentStore {
        AgentStore::open(vault, DeviceInfo { id: "d".into(), name: "test".into() }, FixedClock::new(1_793_577_600_000))
    }

    fn call(vault: &Vault, name: &str, args: &Value) -> ToolOutput {
        let index = IndexService::spawn();
        let agent = store(vault);
        let actor = Actor::agent("mcp:test", "Test agent", Via::McpStdio, "d");
        agent.set_cap(&actor.id, Mode::Direct);
        let cancel = CancelFlag::default();
        let cx = ToolCx { vault, index: &index, agent: &agent, actor: &actor, cancel: &cancel, allow_write: true };
        let t = tools().into_iter().find(|t| t.spec().name == name).unwrap();
        t.call(&cx, args)
    }

    fn structured(o: &ToolOutput) -> &Value {
        match o {
            ToolOutput::Ok { structured, .. } => structured,
            ToolOutput::Err { structured, message, .. } => panic!("error: {message} {structured}"),
            ToolOutput::InputRequired { .. } => panic!("input required"),
        }
    }

    #[test]
    fn reads_nodes_with_text_cut_to_fit() {
        let v = Vault::in_memory(&[("Boards/Ropes.canvas", BOARD)]);
        let o = call(&v, "canvas_read", &json!({"path": "Boards/Ropes"}));
        let s = structured(&o);
        assert_eq!(s["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(s["nodes"][1]["file"], "Rope Notes.md");
        assert_eq!(s["edges"][0]["label"], "see");
        assert_eq!(s["truncated"], true, "the long text doesn't fit in 250×60");
        assert!(s["nodes"][0]["text"].as_str().unwrap().ends_with('…'));
        assert_eq!(s["version"], ContentHash::of(BOARD).to_string());
        let ToolOutput::Ok { text, .. } = &o else { unreachable!() };
        assert!(text.contains("- edge e: a → b (see)"), "{text}");
        let missing = call(&v, "canvas_read", &json!({"path": "Nope.canvas"}));
        assert!(matches!(missing, ToolOutput::Err { code: "not_found", .. }));
        let bad = Vault::in_memory(&[("Bad.canvas", "{nodes")]);
        assert!(matches!(call(&bad, "canvas_read", &json!({"path": "Bad.canvas"})), ToolOutput::Err { code: "invalid_canvas", .. }));
    }

    #[test]
    fn apply_outside_agent_zones_is_a_node_level_proposal() {
        let v = Vault::in_memory(&[("Boards/Ropes.canvas", BOARD)]);
        let version = ContentHash::of(BOARD).to_string();
        let o = call(
            &v,
            "canvas_apply",
            &json!({"path": "Boards/Ropes.canvas", "expected_version": version, "ops": [
                {"op": "add_node", "node": {"type": "text", "text": "Undo is cheap", "x": 0, "y": 200}},
                {"op": "update_node", "id": "b", "set": {"color": "4"}}]}),
        );
        let s = structured(&o);
        assert_eq!(s["outcome"], "proposed", "{s}");
        assert_eq!(s["changes"], json!(["Change color of “Rope Notes.md”", "Add text node “Undo is cheap”"]));
        assert_eq!(v.read(&"Boards/Ropes.canvas".into()).unwrap(), BOARD, "nothing written until the person accepts");
        let agent = store(&v);
        let list = agent.proposals(&ProposalFilter::pending());
        assert_eq!(list.len(), 1);
        let p = agent.proposal(&list[0].id).unwrap();
        let new_text = p.changes.iter().find_map(|c| c.new_text.clone()).expect("an edit");
        // One node per line: the proposal's hunks are whole nodes.
        let diff = similar::TextDiff::from_lines(BOARD, &new_text);
        let changed: Vec<String> =
            diff.iter_all_changes().filter(|c| c.tag() != similar::ChangeTag::Equal).map(|c| c.value().trim().to_string()).collect();
        assert_eq!(changed.len(), 3, "{changed:?}");
        assert!(changed.iter().all(|l| l.starts_with("{\"id\":")), "{changed:?}");
    }

    #[test]
    fn apply_in_an_agent_zone_writes_and_creates() {
        let v = Vault::in_memory(&[]);
        let o = call(
            &v,
            "canvas_apply",
            &json!({"path": "wiki/Map", "ops": [{"op": "add_node", "node": {"id": "n1", "type": "group", "label": "Map"}}]}),
        );
        let s = structured(&o);
        assert_eq!(s["outcome"], "applied", "{s}");
        assert_eq!(s["ids"], json!(["n1"]));
        let text = v.read(&"wiki/Map.canvas".into()).unwrap();
        assert_eq!(qc::parse(&text).unwrap().nodes[0].label.as_deref(), Some("Map"));
        let o = call(&v, "canvas_apply", &json!({"path": "wiki/Map.canvas", "ops": [{"op": "remove_node", "id": "zz"}]}));
        assert!(matches!(o, ToolOutput::Err { code: "invalid_args", .. }));
        let o = call(
            &v,
            "canvas_apply",
            &json!({"path": "wiki/Map.canvas", "expected_version": ContentHash::of("old").to_string(), "ops": [{"op": "remove_node", "id": "zz"}]}),
        );
        assert!(matches!(o, ToolOutput::Err { code: "version_conflict", .. }));
        let o = call(
            &v,
            "canvas_apply",
            &json!({"path": "wiki/Map.canvas", "expected_version": ContentHash::of("old").to_string(), "ops": [{"op": "update_node", "id": "n1", "set": {"label": "World"}}]}),
        );
        assert_eq!(structured(&o)["rebased"], true, "still fits the new version");
        let o = call(&v, "canvas_apply", &json!({"path": ".quire/x.canvas", "ops": [{"op": "add_node", "node": {"type": "group"}}]}));
        assert!(o.is_error());
    }
}
