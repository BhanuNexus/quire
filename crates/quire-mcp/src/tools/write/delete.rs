// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `delete`, which deletes a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "delete",
        "Delete a note",
        "Move a note to the vault's trash (it can be restored). Example: {\"path\": \"wiki/Old.md\"}.",
        json!({"type": "object", "properties": props(json!({"path": {"type": "string"}})), "required": ["path"]}),
        o(),
        Annotations::WRITE,
        Access::Write,
        delete,
    )
}

fn delete(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let (path, run) = match (existing(cx, a, "path"), run_arg(a)) {
        (Ok(p), Ok(r)) => (p, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let base = ContentHash::of(&cx.vault.read(&path).unwrap_or_default());
    let cs =
        ChangeSet { changes: vec![Change::Delete { path: path.clone() }], run, title: title_for("delete", &path), ..ChangeSet::default() };
    submit(cx, ext, a, "delete", &path, cs, &format!("Move {path} to the trash"), &base, json!({}))
}
