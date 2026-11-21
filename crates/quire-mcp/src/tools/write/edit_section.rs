// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `edit_section`, which rewrites one section of a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{
    Access, Annotations, CallExt, EXPECTED, EditOp, SectionMode, Tool, ToolCx, ToolOutput, Value, edit, json, outcome_schema, props, req,
    req_any, s, tool,
};

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "edit_section",
        "Edit a section",
        "Replace, append to or prepend to the body under a heading (\"## Undo\", or just \"Undo\") or a block (\"^blockid\"). The heading line itself stays. Example: {\"path\": \"Rope Notes.md\", \"section\": \"## Undo\", \"content\": \"Ropes share subtrees, so…\", \"mode\": \"replace\"}.",
        json!({"type": "object", "properties": props(json!({
                "path": {"type": "string"}, "section": {"type": "string"}, "content": {"type": "string"},
                "mode": {"type": "string", "enum": ["replace", "append", "prepend"], "default": "replace"},
                "expected_version": {"type": "string", "description": EXPECTED}})), "required": ["path", "section", "content"]}),
        o(),
        Annotations::WRITE,
        Access::Write,
        edit_section,
    )
}

fn edit_section(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let mode = match s(a, "mode").as_str() {
        "" | "replace" => SectionMode::Replace,
        "append" => SectionMode::Append,
        "prepend" => SectionMode::Prepend,
        other => return ToolOutput::error("invalid_args", format!("mode must be replace, append or prepend, got {other}")),
    };
    // `heading` is the 0.9 spelling of `section`.
    match (req_any(a, &["section", "heading"]), req(a, "content")) {
        (Ok(target), Ok(content)) => edit(cx, ext, a, "edit_section", &EditOp::Section { target, content, mode }),
        (Err(e), _) | (_, Err(e)) => e,
    }
}
