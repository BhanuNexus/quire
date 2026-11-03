// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The JSON Canvas 1.0 model (<https://jsoncanvas.org>), docs/formats/canvas.md.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::geom::Rect;

/// A node: `text`, `file`, `link` or `group` (other types are kept as they are).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(deserialize_with = "int")]
    pub x: i64,
    #[serde(deserialize_with = "int")]
    pub y: i64,
    #[serde(deserialize_with = "int")]
    pub width: i64,
    #[serde(deserialize_with = "int")]
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

/// JSON Canvas positions are integers; a file with `10.5` still opens (rounded).
fn int<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f.round() as i64))
        .ok_or_else(|| serde::de::Error::custom(format!("expected a number, found {v}")))
}

/// What a node is, for code that draws or edits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Text,
    File,
    Link,
    Group,
    /// A type this version doesn't know; kept as it is.
    Other,
}

impl NodeKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::File => "file",
            Self::Link => "link",
            Self::Group => "group",
            Self::Other => "other",
        }
    }
}

impl Node {
    pub fn node_kind(&self) -> NodeKind {
        match self.kind.as_str() {
            "text" => NodeKind::Text,
            "file" => NodeKind::File,
            "link" => NodeKind::Link,
            "group" => NodeKind::Group,
            _ => NodeKind::Other,
        }
    }

    /// A node of `kind` at a position, with no content.
    pub fn new(id: impl Into<String>, kind: NodeKind, x: i64, y: i64, width: i64, height: i64) -> Self {
        Self {
            id: id.into(),
            kind: kind.name().to_string(),
            x,
            y,
            width,
            height,
            color: None,
            text: None,
            file: None,
            subpath: None,
            url: None,
            label: None,
            background: None,
            background_style: None,
            other: Map::new(),
        }
    }

    pub fn rect(&self) -> Rect {
        Rect::new(self.x as f32, self.y as f32, self.width as f32, self.height as f32)
    }

    pub fn set_rect(&mut self, r: Rect) {
        self.x = r.x.round() as i64;
        self.y = r.y.round() as i64;
        self.width = (r.w.round() as i64).max(1);
        self.height = (r.h.round() as i64).max(1);
    }

    /// A short name for lists and review: the first line of a text node, a
    /// file's name, a link's address, a group's label.
    pub fn title(&self) -> String {
        let s = match self.node_kind() {
            NodeKind::Text => self
                .text
                .as_deref()
                .unwrap_or_default()
                .lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .find(|l| !l.is_empty())
                .unwrap_or_default()
                .to_string(),
            NodeKind::File => {
                let f = self.file.as_deref().unwrap_or_default();
                let name = f.rsplit('/').next().unwrap_or(f);
                format!("{name}{}", self.subpath.as_deref().unwrap_or_default())
            }
            NodeKind::Link => self.url.clone().unwrap_or_default(),
            NodeKind::Group => self.label.clone().unwrap_or_default(),
            NodeKind::Other => String::new(),
        };
        let mut s: String = s.chars().take(60).collect();
        if s.is_empty() {
            s = format!("{} {}", self.kind, self.id);
        }
        s
    }
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

impl Edge {
    pub fn new(id: impl Into<String>, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            from_node: from.into(),
            from_side: None,
            from_end: None,
            to_node: to.into(),
            to_side: None,
            to_end: None,
            color: None,
            label: None,
            other: Map::new(),
        }
    }

    /// Whether the end at the target has an arrow (the JSON Canvas default).
    pub fn arrow_at_to(&self) -> bool {
        self.to_end.as_deref() != Some("none")
    }

    /// Whether the end at the source has an arrow (default: none).
    pub fn arrow_at_from(&self) -> bool {
        self.from_end.as_deref() == Some("arrow")
    }
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

impl Canvas {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn node_index(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    pub fn edge(&self, id: &str) -> Option<&Edge> {
        self.edges.iter().find(|e| e.id == id)
    }

    /// Whether `id` is taken by a node or an edge.
    pub fn has_id(&self, id: &str) -> bool {
        self.nodes.iter().any(|n| n.id == id) || self.edges.iter().any(|e| e.id == id)
    }

    /// A new id, 16 hex digits as Obsidian makes them, not used yet. `seed`
    /// advances, so callers pass the same seed for several ids (tests pass a
    /// fixed one; the app seeds from the clock).
    pub fn new_id(&self, seed: &mut u64) -> String {
        loop {
            // xorshift64*: enough to avoid collisions, no dependency.
            let mut x = *seed | 1;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            *seed = x;
            let id = format!("{:016x}", x.wrapping_mul(0x2545_f491_4f6c_dd1d));
            if !self.has_id(&id) {
                return id;
            }
        }
    }

    /// Remove a node and the edges that touch it. Returns the node.
    pub fn remove_node(&mut self, id: &str) -> Option<Node> {
        let i = self.node_index(id)?;
        self.edges.retain(|e| e.from_node != id && e.to_node != id);
        Some(self.nodes.remove(i))
    }

    /// The smallest rectangle around every node.
    pub fn bounds(&self) -> Option<Rect> {
        self.nodes.iter().map(Node::rect).reduce(Rect::union)
    }
}

/// Read a `.canvas` file. An empty file is an empty canvas.
pub fn parse(text: &str) -> Result<Canvas, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
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

/// A node or edge colour: one of the six presets or `#RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// `"1"` red, `"2"` orange, `"3"` yellow, `"4"` green, `"5"` cyan, `"6"` purple.
    Preset(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Ok(n) = s.parse::<u8>() {
            return (1..=6).contains(&n).then_some(Self::Preset(n));
        }
        let hex = s.strip_prefix('#')?;
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let ch = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self::Rgb(ch(0)?, ch(2)?, ch(4)?))
    }

    /// The colour as RGB (presets as Obsidian shows them).
    pub const fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Preset(1) => (0xfb, 0x46, 0x4c),
            Self::Preset(2) => (0xe9, 0x97, 0x3f),
            Self::Preset(3) => (0xe0, 0xde, 0x71),
            Self::Preset(4) => (0x44, 0xcf, 0x6e),
            Self::Preset(5) => (0x53, 0xdf, 0xdd),
            Self::Preset(_) => (0xa8, 0x82, 0xff),
            Self::Rgb(r, g, b) => (r, g, b),
        }
    }

    pub fn to_json(self) -> String {
        match self {
            Self::Preset(n) => n.to_string(),
            Self::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    /// The preset's name in the colour menu.
    pub const fn preset_name(n: u8) -> &'static str {
        match n {
            1 => "Red",
            2 => "Orange",
            3 => "Yellow",
            4 => "Green",
            5 => "Cyan",
            _ => "Purple",
        }
    }
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
        assert_eq!(parse("\u{feff}{}").unwrap(), Canvas::default(), "a byte-order mark is fine");
    }

    #[test]
    fn fractional_positions_are_rounded_and_junk_is_refused() {
        let c = parse(r#"{"nodes":[{"id":"a","type":"text","text":"x","x":10.6,"y":-0.4,"width":250,"height":60}]}"#).unwrap();
        assert_eq!((c.nodes[0].x, c.nodes[0].y), (11, 0));
        assert!(parse(r#"{"nodes":[{"id":"a","type":"text","x":"left","y":0,"width":1,"height":1}]}"#).is_err());
        assert!(parse("{\"nodes\":[").is_err());
    }

    #[test]
    fn ids_titles_colours() {
        let mut c = parse(OBSIDIAN).unwrap();
        let mut seed = 7;
        let a = c.new_id(&mut seed);
        let b = c.new_id(&mut seed);
        assert_eq!(a.len(), 16);
        assert_ne!(a, b);
        assert_eq!(c.nodes[0].title(), "Ropes");
        assert_eq!(c.nodes[1].title(), "Rope Notes.md#Undo");
        assert_eq!(c.nodes[2].title(), "Reading");
        assert_eq!(c.remove_node("a1").map(|n| n.id), Some("a1".into()));
        assert!(c.edges.is_empty(), "edges to a removed node go too");
        assert_eq!(Color::parse("4"), Some(Color::Preset(4)));
        assert_eq!(Color::parse("#FF8000").map(Color::rgb), Some((255, 128, 0)));
        assert_eq!(Color::parse("7"), None);
        assert_eq!(Color::parse("#ff80"), None);
        assert_eq!(Color::Rgb(1, 2, 255).to_json(), "#0102ff");
        let r = c.bounds().unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (-150.0, -50.0, 800.0, 500.0));
    }
}
