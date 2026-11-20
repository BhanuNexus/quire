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
        schema!({"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {"versions": {"type": "array", "items": {"type": "object", "properties": {
                "id": {"type": "string"}, "at": {"type": "string"}, "actor": {"type": "string"}, "version": {"type": "string"},
                "added": {"type": "integer"}, "removed": {"type": "integer"}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["versions"]}),
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
    let mut snaps = cx.vault.history(&path);
    // Newest first, by time and then file (the cursor's key), so a version saved between pages
    // makes none repeat.
    let key =
        |s: &quire_app::vault::Snapshot| format!("{}\u{0}{}", bounds::num_key(u64::MAX - u64::try_from(s.at_ms).unwrap_or(0)), s.file);
    snaps.sort_by_cached_key(key);
    // Only the versions shown are read, with the one before each for its counts.
    let render = |snap: &quire_app::vault::Snapshot| {
        let i = snaps.iter().position(|s| s.file == snap.file).unwrap_or(0);
        let Ok(t) = cx.vault.read_snapshot(snap) else { return Value::Null };
        let prev = snaps.get(i + 1).and_then(|s| cx.vault.read_snapshot(s).ok()).unwrap_or_default();
        let (added, removed) = quire_app::history_ops::diff_stats(&prev, &t);
        let actor = cx
            .vault
            .read_provenance(snap)
            .and_then(|p| p.get("actor").and_then(|a| a.get("id")).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "human".into());
        json!({"id": snap.at_ms.to_string(), "at": iso(snap.at_ms), "actor": actor, "version": ContentHash::of(&t), "added": added, "removed": removed})
    };
    let fingerprint = bounds::fingerprint("h", a);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let Some(page) = bounds::keyset(&snaps, key, render, a, &fingerprint, limit, budget) else { return stale(&fingerprint) };
    // A version that can't be read is left out, as before.
    let rows: Vec<Value> = page.items.iter().filter(|r| !r.is_null()).cloned().collect();
    let mut text = format!("Versions of {path}, newest first:\n");
    for r in &rows {
        writeln!(
            text,
            "- {} by {}: {} (+{} −{})",
            r["at"].as_str().unwrap_or_default(),
            r["actor"].as_str().unwrap_or_default(),
            r["version"].as_str().unwrap_or_default(),
            r["added"],
            r["removed"]
        )
        .expect("a String takes any text");
    }
    if rows.is_empty() {
        text.push_str("(no saved versions)");
    }
    more_line(&mut text, "history", &page);
    let st = json!({"versions": rows, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor});
    ToolOutput::ok(text.trim_end().to_string(), st).with_links(vec![note_link(&path)])
}
