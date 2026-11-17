// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `append`, which appends to a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{
    Access, Annotations, CallExt, EXPECTED, EditOp, Tool, ToolCx, ToolOutput, Value, edit, json, opt, outcome_schema, props, req, tool,
};

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "append",
        "Append",
        "Add text at the end of a note, or at the end of a section with under_heading. Example: {\"path\": \"wiki/log.md\", \"text\": \"- 2026-11-02: ingested paper X\"}.",
        json!({"type": "object", "properties": props(json!({
                "path": {"type": "string"}, "text": {"type": "string"}, "under_heading": {"type": "string"},
                "expected_version": {"type": "string", "description": EXPECTED}})), "required": ["path", "text"]}),
        o(),
        Annotations::WRITE,
        Access::Write,
        append,
    )
}

fn append(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    match req(a, "text") {
        Ok(text) => edit(cx, ext, a, "append", &EditOp::Append { text, under: opt(a, "under_heading") }),
        Err(e) => e,
    }
}
