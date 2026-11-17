// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `edit_section`, which rewrites one section of a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "edit_section",
        "Edit a section",
        "Replace, append to or prepend to the body under a heading (\"## Undo\", or just \"Undo\") or a block (\"^blockid\"). The heading line itself stays. Example: {\"path\": \"Rope Notes.md\", \"heading\": \"## Undo\", \"content\": \"Ropes share subtrees, so…\", \"mode\": \"replace\"}.",
        json!({"type": "object", "properties": props(json!({
                "path": {"type": "string"}, "heading": {"type": "string"}, "content": {"type": "string"},
                "mode": {"type": "string", "enum": ["replace", "append", "prepend"], "default": "replace"},
                "expected_version": {"type": "string", "description": EXPECTED}})), "required": ["path", "heading", "content"]}),
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
    match (req(a, "heading"), req(a, "content")) {
        (Ok(target), Ok(content)) => edit(cx, ext, a, "edit_section", &EditOp::Section { target, content, mode }),
        (Err(e), _) | (_, Err(e)) => e,
    }
}
