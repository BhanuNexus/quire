// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The JSON Canvas 1.0 model (<https://jsoncanvas.org>), docs/formats/canvas.md.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A node: `text`, `file`, `link` or `group` (other types are kept as they are).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
    /// `"1"`–`"6"` or `#RRGGBB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_style: Option<String>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub id: String,
    pub from_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_end: Option<String>,
    pub to_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// A canvas. Nodes are in z-order (first is at the back).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Canvas {
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// Read a `.canvas` file. An empty file is an empty canvas.
pub fn parse(text: &str) -> Result<Canvas, String> {
    if text.trim().is_empty() {
        return Ok(Canvas::default());
    }
    serde_json::from_str(text).map_err(|e| e.to_string())
}

/// Quire's file form: still plain JSON, but one node or edge per line:
///
/// ```text
/// {"nodes":[
/// {"id":"a1","type":"text","x":0,"y":0,"width":240,"height":120,"text":"…"}
/// ],"edges":[
/// ]}
/// ```
pub fn to_canonical(c: &Canvas) -> String {
    fn lines<T: Serialize>(items: &[T]) -> String {
        items.iter().map(|i| serde_json::to_string(i).unwrap_or_default()).collect::<Vec<_>>().join(",\n")
    }
    let mut out = String::from("{\"nodes\":[\n");
    out.push_str(&lines(&c.nodes));
    if !c.nodes.is_empty() {
        out.push('\n');
    }
    out.push_str("],\"edges\":[\n");
    out.push_str(&lines(&c.edges));
    if !c.edges.is_empty() {
        out.push('\n');
    }
    out.push(']');
    for (k, v) in &c.other {
        out.push(',');
        out.push_str(&serde_json::to_string(k).unwrap_or_default());
        out.push(':');
        out.push_str(&serde_json::to_string(v).unwrap_or_default());
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As Obsidian writes it: pretty-printed, with a key Quire doesn't know.
    const OBSIDIAN: &str = r##"{
	"nodes":[
		{"id":"a1","type":"text","text":"# Ropes","x":-100,"y":20,"width":250,"height":60,"color":"4","styleAttributes":{}},
		{"id":"f2","type":"file","file":"Rope Notes.md","subpath":"#Undo","x":200,"y":20,"width":400,"height":400},
		{"id":"g3","type":"group","label":"Reading","x":-150,"y":-50,"width":800,"height":500,"backgroundStyle":"cover"}
	],
	"edges":[
		{"id":"e1","fromNode":"a1","fromSide":"right","toNode":"f2","toSide":"left","toEnd":"arrow","label":"see"}
	]
}"##;

    #[test]
    fn round_trips_obsidian_files_keeping_unknown_keys() {
        let c = parse(OBSIDIAN).unwrap();
        assert_eq!(c.nodes.len(), 3);
        assert_eq!(c.nodes[1].subpath.as_deref(), Some("#Undo"));
        assert_eq!(c.nodes[2].background_style.as_deref(), Some("cover"));
        assert!(c.nodes[0].other.contains_key("styleAttributes"));
        let out = to_canonical(&c);
        assert_eq!(out.lines().count(), 7, "{out}");
        assert!(out.lines().nth(1).unwrap().starts_with(r#"{"id":"a1","type":"text","x":-100"#), "{out}");
        assert_eq!(parse(&out).unwrap(), c);
        assert_eq!(to_canonical(&parse(&out).unwrap()), out, "canonical form is stable");
    }

    #[test]
    fn empty_and_extra_top_level_keys() {
        assert_eq!(to_canonical(&parse("").unwrap()), "{\"nodes\":[\n],\"edges\":[\n]}\n");
        let c = parse(r#"{"nodes":[],"edges":[],"quire":{"view":[0,0,1]}}"#).unwrap();
        let out = to_canonical(&c);
        assert_eq!(out, "{\"nodes\":[\n],\"edges\":[\n],\"quire\":{\"view\":[0,0,1]}}\n");
        assert_eq!(parse(&out).unwrap(), c);
    }
}
