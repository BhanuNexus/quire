// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `insert`, which inserts lines into a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "insert",
        "Insert lines",
        "Insert text as new lines after line `line` (0 = at the top), using view's line numbers. Because it addresses a line number, a stale expected_version is a conflict. Example: {\"path\": \"Rope Notes.md\", \"line\": 3, \"text\": \"- a new point\", \"expected_version\": \"sha256:…\"}.",
        json!({"type": "object", "properties": props(json!({
                "path": {"type": "string"}, "line": {"type": "integer", "minimum": 0}, "text": {"type": "string"},
                "expected_version": {"type": "string", "description": EXPECTED}})), "required": ["path", "line", "text"]}),
        o(),
        Annotations::WRITE,
        Access::Write,
        insert,
    )
}

fn insert(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let Some(line) = a.get("line").and_then(Value::as_u64) else {
        return ToolOutput::error("invalid_args", "`line` is required (0 = top).");
    };
    match req(a, "text") {
        Ok(text) => edit(cx, ext, a, "insert", &EditOp::Insert { line: usize::try_from(line).unwrap_or(usize::MAX), text }),
        Err(e) => e,
    }
}
