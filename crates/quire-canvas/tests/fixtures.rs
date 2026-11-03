// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canvases as Obsidian writes them open, save in Quire's form, and read back equal.

use quire_canvas::{NodeKind, parse, to_canonical};

#[test]
fn obsidian_fixtures_round_trip() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("canvas") {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path).unwrap();
        let c = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let out = to_canonical(&c);
        let back = parse(&out).unwrap();
        assert_eq!(back, c, "{} parses back equal", path.display());
        assert_eq!(to_canonical(&back), out, "{} is stable", path.display());
        // Every node and edge on its own line.
        assert_eq!(out.lines().count(), c.nodes.len() + c.edges.len() + 3, "{out}");
        // Nothing was dropped: every key of the input is in the output.
        let a: serde_json::Value = serde_json::from_str(&text).unwrap();
        let b: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(normalize(a), normalize(b), "{}", path.display());
    }
    assert_eq!(seen, 3);
}

/// Missing `nodes`/`edges` read as empty lists.
fn normalize(mut v: serde_json::Value) -> serde_json::Value {
    for k in ["nodes", "edges"] {
        v.as_object_mut().expect("a canvas is an object").entry(k).or_insert_with(|| serde_json::json!([]));
    }
    v
}

#[test]
fn research_canvas_reads_every_kind() {
    let text = include_str!("fixtures/obsidian-research.canvas");
    let c = parse(text).unwrap();
    let kinds: Vec<NodeKind> = c.nodes.iter().map(quire_canvas::Node::node_kind).collect();
    assert_eq!(kinds[..5], [NodeKind::Group, NodeKind::Text, NodeKind::File, NodeKind::File, NodeKind::Link]);
    assert_eq!(kinds[7], NodeKind::Other, "an unknown node type is kept");
    assert_eq!(c.nodes[7].other.get("emoji").and_then(|v| v.as_str()), Some("⭐"));
    assert!(c.edges[2].other.contains_key("styleAttributes"));
    assert!(c.other.contains_key("metadata"));
    assert!(c.edges[1].arrow_at_from() && !c.edges[1].arrow_at_to());
    assert!(c.edges[0].arrow_at_to() && !c.edges[0].arrow_at_from());
}
