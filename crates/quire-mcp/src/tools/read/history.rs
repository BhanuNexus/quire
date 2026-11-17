// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `history`: a note's versions.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "history",
        "Version history",
        "The saved versions of a note, newest first, with who made each and how many lines changed. Read one with read_version; compare with diff. Example: {\"path\": \"Rope Notes.md\"}.",
        schema!({"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {"versions": {"type": "array", "items": {"type": "object", "properties": {
                "id": {"type": "string"}, "at": {"type": "string"}, "actor": {"type": "string"}, "version": {"type": "string"},
                "added": {"type": "integer"}, "removed": {"type": "integer"}}}}}, "required": ["versions"]}),
        r,
        Access::Read,
        history,
    )
}

fn history(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let limit = int(a, "limit", 20, 1, 200);
    let snaps = cx.vault.history(&path);
    let texts: Vec<Option<String>> = snaps.iter().take(limit + 1).map(|s| cx.vault.read_snapshot(s).ok()).collect();
    let mut rows = Vec::new();
    let mut text = format!("Versions of {path}, newest first:\n");
    for (i, snap) in snaps.iter().take(limit).enumerate() {
        let Some(t) = &texts[i] else { continue };
        let prev = texts.get(i + 1).and_then(Option::as_deref).unwrap_or("");
        let (added, removed) = quire_app::history_ops::diff_stats(prev, t);
        let actor = cx
            .vault
            .read_provenance(snap)
            .and_then(|p| p.get("actor").and_then(|a| a.get("id")).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "human".into());
        let version = ContentHash::of(t);
        let _ = writeln!(text, "- {} by {actor}: {version} (+{added} −{removed})", iso(snap.at_ms));
        rows.push(json!({"id": snap.at_ms.to_string(), "at": iso(snap.at_ms), "actor": actor, "version": version, "added": added, "removed": removed}));
    }
    if rows.is_empty() {
        text.push_str("(no saved versions)");
    }
    ToolOutput::ok(text.trim_end().to_string(), json!({"versions": rows})).with_links(vec![note_link(&path)])
}
