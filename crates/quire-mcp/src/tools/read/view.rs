// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `view`: a note with line numbers, or part of one.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "view",
        "View a note",
        "Read a note with line numbers (`  12│ text`), or just a section or a line range. Returns the note's version: pass it as expected_version when you edit, so you never overwrite changes you haven't seen. Examples: {\"path\": \"Rope Notes.md\"}; {\"path\": \"Rope Notes.md\", \"section\": \"## Undo\"}; {\"path\": \"Rope Notes.md\", \"lines\": [10, 40]}. Long notes are cut at max_tokens; pass next_cursor back as cursor to continue. include_pending shows the note with your own pending suggestion applied.",
        schema!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Vault path, e.g. \"Projects/Plan.md\" (\".md\" may be left out)."},
                "section": {"type": "string", "description": "A heading (\"## Undo\" or \"Undo\") or a block id (\"^abc\")."},
                "lines": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2, "description": "[first, last], 1-based, inclusive."},
                "include_pending": {"type": "boolean", "default": false},
                "max_tokens": {"type": "integer", "default": 4000},
                "cursor": {"type": "string"}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {
                "path": {"type": "string"}, "version": {"type": "string"}, "lines": {"type": "array", "items": {"type": "integer"}},
                "total_lines": {"type": "integer"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]},
                "title": {"type": "string"}, "properties": {"type": "object"}, "trust": {"type": "string"}, "modified": {"type": ["string", "null"]},
                "last_actor": {"type": "string"}, "pending_proposals": {"type": "array"}}, "required": ["path", "version", "lines", "total_lines"]}),
        r,
        Access::Read,
        view,
    )
}

fn view(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Ok(file) = cx.vault.read(&path) else { return ToolOutput::error("not_found", format!("Couldn't read {path}.")) };
    let pending = cx.agent.proposals(&ProposalFilter { status: Some(ProposalStatus::Pending), actor: None, path: Some(path.clone()) });
    let text = if flag(a, "include_pending", false) { own_pending(cx, &path).map_or(file, |(_, t)| t) } else { file };
    let version = ext.served.store(&text);
    let w = match window(&text, &version, a) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let (untrusted, origin) = trust(cx, &path, &text);
    let header = format!("# File: {path} (lines {}–{} of {}, version {version})\n", w.first, w.shown_to, w.total);
    let mut out = header;
    if untrusted {
        out.push_str(&fence_untrusted(&path, origin.as_deref(), &w.body));
        out.push('\n');
    } else {
        out.push_str(&w.body);
    }
    if let Some(c) = &w.next {
        write!(out, "… cut at line {} of {}; continue with view {{\"path\": \"{path}\", \"cursor\": \"{c}\"}}", w.shown_to, w.last)
            .expect("a String takes any text");
    }
    let ix = cx.index.read();
    let title = ix.get(&path).map_or_else(|| path.stem().to_string(), |r| r.title.clone());
    let props = frontmatter::parse(&text).map_or_else(|| json!({}), |f| serde_json::to_value(&f.fields).unwrap_or_default());
    let st = json!({
        "path": path, "version": version, "lines": [w.first, w.shown_to], "total_lines": w.total, "truncated": w.next.is_some(),
        "next_cursor": w.next, "title": title, "properties": props, "trust": if untrusted { "untrusted" } else { "trusted" },
        "modified": modified(cx, &path), "last_actor": last_actor(cx, &path),
        "pending_proposals": pending.iter().map(|p| json!({"id": p.id, "title": p.title, "actor": p.actor.id})).collect::<Vec<_>>(),
    });
    ToolOutput::ok(out, st).with_links(vec![note_link(&path)])
}
