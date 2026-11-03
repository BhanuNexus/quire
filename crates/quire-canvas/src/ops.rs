// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Changes to a canvas as data: the operations agents send (`canvas_apply`,
//! design §G.1) and a node-level description of what changed between two
//! versions (for review and tool results).

use serde_json::{Map, Value, json};

use crate::geom::Side;
use crate::model::{Canvas, Color, Edge, Node, NodeKind};

/// Why an operation was refused. `index` is its position in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub index: usize,
    pub message: String,
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation {}: {}", self.index + 1, self.message)
    }
}

/// Keys a node or edge update may not set (the id is the identity; the type
/// changes what the node is).
const FIXED: [&str; 2] = ["id", "type"];

fn err(index: usize, message: impl Into<String>) -> OpError {
    OpError { index, message: message.into() }
}

/// Default size of a new node by type, as Obsidian makes them.
pub const fn default_size(kind: NodeKind) -> (i64, i64) {
    match kind {
        NodeKind::Text => (250, 60),
        NodeKind::File => (400, 400),
        NodeKind::Link => (400, 300),
        NodeKind::Group | NodeKind::Other => (500, 400),
    }
}

/// Apply `ops` in order, all or nothing.
///
/// Each op is `{"op": "add_node", "node": {…}}`, `{"op": "update_node", "id", "set": {…}}`,
/// `{"op": "remove_node", "id"}`, `{"op": "add_edge", "edge": {…}}`,
/// `{"op": "update_edge", "id", "set": {…}}` or `{"op": "remove_edge", "id"}`.
/// New nodes and edges without an id get one; the ids are returned in order.
pub fn apply(c: &mut Canvas, ops: &[Value], seed: &mut u64) -> Result<Vec<String>, OpError> {
    let mut work = c.clone();
    let mut ids = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let name = op.get("op").and_then(Value::as_str).ok_or_else(|| err(i, "each operation needs \"op\""))?;
        match name {
            "add_node" => ids.push(add_node(&mut work, i, op.get("node").unwrap_or(&Value::Null), seed)?),
            "update_node" => update(&mut work, i, op, true)?,
            "remove_node" => {
                let id = op_id(i, op)?;
                work.remove_node(&id).ok_or_else(|| err(i, format!("no node {id}")))?;
            }
            "add_edge" => ids.push(add_edge(&mut work, i, op.get("edge").unwrap_or(&Value::Null), seed)?),
            "update_edge" => update(&mut work, i, op, false)?,
            "remove_edge" => {
                let id = op_id(i, op)?;
                let at = work.edges.iter().position(|e| e.id == id).ok_or_else(|| err(i, format!("no edge {id}")))?;
                work.edges.remove(at);
            }
            other => {
                return Err(err(
                    i,
                    format!("unknown op {other:?}; use add_node, update_node, remove_node, add_edge, update_edge or remove_edge"),
                ));
            }
        }
    }
    *c = work;
    Ok(ids)
}

fn op_id(i: usize, op: &Value) -> Result<String, OpError> {
    op.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| err(i, "\"id\" is required"))
}

fn add_node(c: &mut Canvas, i: usize, v: &Value, seed: &mut u64) -> Result<String, OpError> {
    let Value::Object(fields) = v else { return Err(err(i, "add_node needs \"node\": {…}")) };
    let mut fields = fields.clone();
    let kind = fields.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    let k = match kind.as_str() {
        "text" => NodeKind::Text,
        "file" => NodeKind::File,
        "link" => NodeKind::Link,
        "group" => NodeKind::Group,
        _ => return Err(err(i, "node type must be text, file, link or group")),
    };
    let id = match fields.get("id").and_then(Value::as_str) {
        Some(id) if c.has_id(id) => return Err(err(i, format!("id {id} is already used"))),
        Some(id) if !id.trim().is_empty() => id.to_string(),
        _ => c.new_id(seed),
    };
    fields.insert("id".into(), id.clone().into());
    let (w, h) = default_size(k);
    for (key, d) in [("x", 0), ("y", 0), ("width", w), ("height", h)] {
        fields.entry(key).or_insert_with(|| d.into());
    }
    let node: Node = serde_json::from_value(Value::Object(fields)).map_err(|e| err(i, format!("bad node: {e}")))?;
    check_node(i, &node)?;
    c.nodes.push(node);
    Ok(id)
}

fn check_node(i: usize, n: &Node) -> Result<(), OpError> {
    if n.width <= 0 || n.height <= 0 {
        return Err(err(i, "width and height must be positive"));
    }
    if let Some(col) = &n.color
        && Color::parse(col).is_none()
    {
        return Err(err(i, format!("colour {col:?} must be \"1\"–\"6\" or #RRGGBB")));
    }
    let missing = match n.node_kind() {
        NodeKind::Text => n.text.is_none().then_some("text"),
        NodeKind::File => n.file.as_deref().is_none_or(str::is_empty).then_some("file"),
        NodeKind::Link => n.url.as_deref().is_none_or(str::is_empty).then_some("url"),
        NodeKind::Group | NodeKind::Other => None,
    };
    match missing {
        Some(field) => Err(err(i, format!("a {} node needs \"{field}\"", n.kind))),
        None => Ok(()),
    }
}

fn add_edge(c: &mut Canvas, i: usize, v: &Value, seed: &mut u64) -> Result<String, OpError> {
    let Value::Object(fields) = v else { return Err(err(i, "add_edge needs \"edge\": {…}")) };
    let mut fields = fields.clone();
    let id = match fields.get("id").and_then(Value::as_str) {
        Some(id) if c.has_id(id) => return Err(err(i, format!("id {id} is already used"))),
        Some(id) if !id.trim().is_empty() => id.to_string(),
        _ => c.new_id(seed),
    };
    fields.insert("id".into(), id.clone().into());
    let edge: Edge = serde_json::from_value(Value::Object(fields)).map_err(|e| err(i, format!("bad edge: {e}")))?;
    check_edge(c, i, &edge)?;
    c.edges.push(edge);
    Ok(id)
}

fn check_edge(c: &Canvas, i: usize, e: &Edge) -> Result<(), OpError> {
    for n in [&e.from_node, &e.to_node] {
        if c.node(n).is_none() {
            return Err(err(i, format!("no node {n} to connect")));
        }
    }
    for s in [&e.from_side, &e.to_side].into_iter().flatten() {
        if Side::parse(s).is_none() {
            return Err(err(i, format!("side {s:?} must be top, right, bottom or left")));
        }
    }
    for end in [&e.from_end, &e.to_end].into_iter().flatten() {
        if !matches!(end.as_str(), "none" | "arrow") {
            return Err(err(i, format!("end {end:?} must be none or arrow")));
        }
    }
    if let Some(col) = &e.color
        && Color::parse(col).is_none()
    {
        return Err(err(i, format!("colour {col:?} must be \"1\"–\"6\" or #RRGGBB")));
    }
    Ok(())
}

/// `update_node` / `update_edge`: merge `set` into the object; `null` removes a key.
fn update(c: &mut Canvas, i: usize, op: &Value, node: bool) -> Result<(), OpError> {
    let id = op_id(i, op)?;
    let Some(Value::Object(set)) = op.get("set") else { return Err(err(i, "\"set\": {…} is required")) };
    if let Some(k) = FIXED.iter().find(|k| set.contains_key(**k)) {
        return Err(err(i, format!("\"{k}\" can't be changed; remove the node and add a new one")));
    }
    let merge = |old: Value| -> Value {
        let mut m: Map<String, Value> = match old {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        for (k, v) in set {
            if v.is_null() {
                m.remove(k);
            } else {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    };
    if node {
        let at = c.node_index(&id).ok_or_else(|| err(i, format!("no node {id}")))?;
        let merged = merge(serde_json::to_value(&c.nodes[at]).unwrap_or(Value::Null));
        let n: Node = serde_json::from_value(merged).map_err(|e| err(i, format!("bad node: {e}")))?;
        check_node(i, &n)?;
        c.nodes[at] = n;
    } else {
        let at = c.edges.iter().position(|e| e.id == id).ok_or_else(|| err(i, format!("no edge {id}")))?;
        let merged = merge(serde_json::to_value(&c.edges[at]).unwrap_or(Value::Null));
        let e: Edge = serde_json::from_value(merged).map_err(|e| err(i, format!("bad edge: {e}")))?;
        check_edge(c, i, &e)?;
        c.edges[at] = e;
    }
    Ok(())
}

/// One node-level change between two versions of a canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    AddNode {
        id: String,
        kind: String,
        title: String,
    },
    RemoveNode {
        id: String,
        kind: String,
        title: String,
    },
    /// Position or size only.
    MoveNode {
        id: String,
        title: String,
    },
    /// Content, colour or other keys (and maybe position).
    EditNode {
        id: String,
        title: String,
        fields: Vec<String>,
    },
    AddEdge {
        id: String,
        from: String,
        to: String,
        label: Option<String>,
    },
    RemoveEdge {
        id: String,
        from: String,
        to: String,
    },
    EditEdge {
        id: String,
        from: String,
        to: String,
        fields: Vec<String>,
    },
    /// Nodes changed order (front/back) without other changes.
    Reorder,
}

impl Change {
    /// In plain words: `Add text node “Ropes”`.
    pub fn describe(&self) -> String {
        match self {
            Self::AddNode { kind, title, .. } => format!("Add {kind} node “{title}”"),
            Self::RemoveNode { kind, title, .. } => format!("Remove {kind} node “{title}”"),
            Self::MoveNode { title, .. } => format!("Move or resize “{title}”"),
            Self::EditNode { title, fields, .. } => format!("Change {} of “{title}”", fields.join(", ")),
            Self::AddEdge { from, to, label, .. } => match label {
                Some(l) => format!("Connect “{from}” → “{to}” ({l})"),
                None => format!("Connect “{from}” → “{to}”"),
            },
            Self::RemoveEdge { from, to, .. } => format!("Disconnect “{from}” → “{to}”"),
            Self::EditEdge { from, to, fields, .. } => format!("Change {} of the connection “{from}” → “{to}”", fields.join(", ")),
            Self::Reorder => "Bring nodes forward or back".into(),
        }
    }

    /// The node or edge it is about (empty for `Reorder`).
    pub fn id(&self) -> &str {
        match self {
            Self::AddNode { id, .. }
            | Self::RemoveNode { id, .. }
            | Self::MoveNode { id, .. }
            | Self::EditNode { id, .. }
            | Self::AddEdge { id, .. }
            | Self::RemoveEdge { id, .. }
            | Self::EditEdge { id, .. } => id,
            Self::Reorder => "",
        }
    }
}

/// Which keys differ between two JSON objects (`x`/`y`/`width`/`height` as
/// "position"/"size").
fn changed_fields(a: &Value, b: &Value) -> Vec<String> {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else { return Vec::new() };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if a.get(k) == b.get(k) {
            continue;
        }
        let name = match k.as_str() {
            "x" | "y" => "position",
            "width" | "height" => "size",
            other => other,
        };
        if !out.iter().any(|o| o == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// What changed from `old` to `new`, node by node, in a stable order.
pub fn diff(old: &Canvas, new: &Canvas) -> Vec<Change> {
    let mut out = Vec::new();
    let title = |c: &Canvas, id: &str| c.node(id).map_or_else(|| id.to_string(), Node::title);
    for n in &new.nodes {
        match old.node(&n.id) {
            None => out.push(Change::AddNode { id: n.id.clone(), kind: n.kind.clone(), title: n.title() }),
            Some(o) if o != n => {
                let fields = changed_fields(&json!(o), &json!(n));
                if fields.iter().all(|f| f == "position" || f == "size") {
                    out.push(Change::MoveNode { id: n.id.clone(), title: n.title() });
                } else {
                    out.push(Change::EditNode { id: n.id.clone(), title: n.title(), fields });
                }
            }
            Some(_) => {}
        }
    }
    for o in &old.nodes {
        if new.node(&o.id).is_none() {
            out.push(Change::RemoveNode { id: o.id.clone(), kind: o.kind.clone(), title: o.title() });
        }
    }
    for e in &new.edges {
        match old.edge(&e.id) {
            None => out.push(Change::AddEdge {
                id: e.id.clone(),
                from: title(new, &e.from_node),
                to: title(new, &e.to_node),
                label: e.label.clone(),
            }),
            Some(o) if o != e => out.push(Change::EditEdge {
                id: e.id.clone(),
                from: title(new, &e.from_node),
                to: title(new, &e.to_node),
                fields: changed_fields(&json!(o), &json!(e)),
            }),
            Some(_) => {}
        }
    }
    for o in &old.edges {
        if new.edge(&o.id).is_none() {
            out.push(Change::RemoveEdge { id: o.id.clone(), from: title(old, &o.from_node), to: title(old, &o.to_node) });
        }
    }
    if out.is_empty() {
        let order = |c: &Canvas| c.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        if order(old) != order(new) {
            out.push(Change::Reorder);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse;

    fn base() -> Canvas {
        parse(
            r##"{"nodes":[
{"id":"a","type":"text","x":0,"y":0,"width":250,"height":60,"text":"# Ropes"},
{"id":"b","type":"file","x":300,"y":0,"width":400,"height":400,"file":"Rope Notes.md","keep":1}
],"edges":[
{"id":"e","fromNode":"a","toNode":"b"}
]}"##,
        )
        .unwrap()
    }

    #[test]
    fn applies_operations_in_order() {
        let mut c = base();
        let mut seed = 1;
        let ops = vec![
            json!({"op": "add_node", "node": {"type": "text", "text": "Undo is cheap", "x": 0, "y": 100}}),
            json!({"op": "update_node", "id": "b", "set": {"color": "4", "subpath": "#Undo"}}),
            json!({"op": "add_edge", "edge": {"id": "e2", "fromNode": "a", "toNode": "b", "fromSide": "bottom", "label": "see"}}),
            json!({"op": "remove_edge", "id": "e"}),
            json!({"op": "update_edge", "id": "e2", "set": {"label": null, "color": "#ff0000"}}),
        ];
        let ids = apply(&mut c, &ops, &mut seed).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[1], "e2");
        let n = c.node(&ids[0]).unwrap();
        assert_eq!((n.width, n.height, n.text.as_deref()), (250, 60, Some("Undo is cheap")));
        let b = c.node("b").unwrap();
        assert_eq!(b.subpath.as_deref(), Some("#Undo"));
        assert_eq!(b.other.get("keep"), Some(&json!(1)), "unknown keys survive an update");
        assert_eq!(c.edges.len(), 1);
        assert_eq!((c.edges[0].label.as_deref(), c.edges[0].color.as_deref()), (None, Some("#ff0000")));
        let d = diff(&base(), &c);
        let words: Vec<String> = d.iter().map(Change::describe).collect();
        assert_eq!(
            words,
            [
                "Change color, subpath of “Rope Notes.md#Undo”",
                "Add text node “Undo is cheap”",
                "Connect “Ropes” → “Rope Notes.md#Undo”",
                "Disconnect “Ropes” → “Rope Notes.md”",
            ]
        );
    }

    #[test]
    fn refuses_bad_operations_without_changing_anything() {
        let before = base();
        let mut seed = 1;
        for (op, msg) in [
            (json!({"op": "add_node", "node": {"type": "text"}}), "needs \"text\""),
            (json!({"op": "add_node", "node": {"type": "blob", "text": "x"}}), "type must be"),
            (json!({"op": "add_node", "node": {"id": "a", "type": "text", "text": "x"}}), "already used"),
            (json!({"op": "update_node", "id": "a", "set": {"type": "file"}}), "can't be changed"),
            (json!({"op": "update_node", "id": "a", "set": {"color": "9"}}), "colour"),
            (json!({"op": "add_edge", "edge": {"fromNode": "a", "toNode": "zz"}}), "no node zz"),
            (json!({"op": "add_edge", "edge": {"fromNode": "a", "toNode": "b", "toSide": "middle"}}), "side"),
            (json!({"op": "remove_node", "id": "zz"}), "no node zz"),
            (json!({"op": "fly"}), "unknown op"),
        ] {
            let mut c = before.clone();
            let ops = vec![json!({"op": "remove_edge", "id": "e"}), op];
            let e = apply(&mut c, &ops, &mut seed).unwrap_err();
            assert_eq!(e.index, 1);
            assert!(e.message.contains(msg), "{e}");
            assert_eq!(c, before, "all or nothing");
        }
    }

    #[test]
    fn diff_names_moves_and_removals() {
        let mut c = base();
        c.nodes[0].x = 50;
        assert_eq!(diff(&base(), &c), vec![Change::MoveNode { id: "a".into(), title: "Ropes".into() }]);
        let mut seed = 3;
        apply(&mut c, &[json!({"op": "remove_node", "id": "b"})], &mut seed).unwrap();
        let d = diff(&base(), &c);
        assert!(d.contains(&Change::RemoveNode { id: "b".into(), kind: "file".into(), title: "Rope Notes.md".into() }));
        assert!(d.iter().any(|x| matches!(x, Change::RemoveEdge { .. })), "its edge goes too");
        let mut r = base();
        r.nodes.swap(0, 1);
        assert_eq!(diff(&base(), &r), vec![Change::Reorder]);
    }
}
