// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `create`, which creates a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{
    Access, Annotations, CallExt, Change, ChangeSet, ContentHash, Mode, Tool, ToolCx, ToolOutput, Value, WriteError, json, outcome_schema,
    path_arg, props, req, run_arg, submit, title_for, tool, unified, visible, write_error,
};

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "create",
        "Create a note",
        "Create a new note at a path. Fails with already_exists rather than overwrite. In agent zones (wiki/) it is written at once; elsewhere it becomes a suggestion the person reviews. Example: {\"path\": \"wiki/Ropes.md\", \"content\": \"# Ropes\\n…\"}.",
        json!({"type": "object", "properties": props(schema!({
                "path": {"type": "string", "description": "Vault path ending in .md."},
                "content": {"type": "string", "description": "Markdown."}})), "required": ["path", "content"]}),
        o(),
        Annotations::CREATE,
        Access::Write,
        create,
    )
}

fn create(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let (path, content, run) = match (path_arg(a, "path"), req(a, "content"), run_arg(a)) {
        (Ok(p), Ok(c), Ok(r)) => (p, c, r),
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return e,
    };
    if !visible(cx, &path) {
        return write_error(&WriteError::PermissionDenied { path, zone: "private".into(), mode: Mode::Deny });
    }
    let diff = unified("", &content, path.as_str());
    let cs = ChangeSet {
        changes: vec![Change::Create { path: path.clone(), text: content }],
        run,
        title: title_for("create", &path),
        ..ChangeSet::default()
    };
    submit(cx, ext, a, "create", &path, cs, &diff, &ContentHash::of(""), json!({}))
}
