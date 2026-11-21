// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `str_replace`, which replaces text in a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{
    Access, Annotations, CallExt, EXPECTED, EditOp, Tool, ToolCx, ToolOutput, Value, edit, flag, json, outcome_schema, props, req, tool,
};

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "str_replace",
        "Replace text",
        "Replace one exact occurrence of old_str with new_str. old_str must match exactly once (copy it from view, without the line numbers); include surrounding text if needed, or pass all: true to replace every occurrence (the result says how many). Fails with not_unique or no_match. Example: {\"path\": \"Rope Notes.md\", \"old_str\": \"undo is cheap\", \"new_str\": \"undo is cheap because subtrees are shared\", \"expected_version\": \"sha256:…\"}.",
        json!({"type": "object", "properties": props(json!({
                "path": {"type": "string"}, "old_str": {"type": "string"}, "new_str": {"type": "string"},
                "all": {"type": "boolean", "default": false, "description": "Replace every occurrence."},
                "expected_version": {"type": "string", "description": EXPECTED}})), "required": ["path", "old_str", "new_str"]}),
        o(),
        Annotations::WRITE,
        Access::Write,
        str_replace,
    )
}

fn str_replace(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    match (req(a, "old_str"), req(a, "new_str")) {
        (Ok(old), Ok(new)) if flag(a, "all", false) => edit(cx, ext, a, "str_replace", &EditOp::ReplaceAll { old, new }),
        (Ok(old), Ok(new)) => edit(cx, ext, a, "str_replace", &EditOp::Replace { old, new }),
        (Err(e), _) | (_, Err(e)) => e,
    }
}
