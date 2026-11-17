// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `rename`, which renames a note.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "rename",
        "Rename or move",
        "Rename or move a note; links to it are updated unless update_links is false. Example: {\"from\": \"Inbox/Rope.md\", \"to\": \"wiki/Ropes.md\"}.",
        json!({"type": "object", "properties": props(schema!({
                "from": {"type": "string"}, "to": {"type": "string"}, "update_links": {"type": "boolean", "default": true}})), "required": ["from", "to"]}),
        o(),
        Annotations::WRITE,
        Access::Write,
        rename,
    )
}

fn rename(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let (from, to, run) = match (existing(cx, a, "from"), path_arg(a, "to"), run_arg(a)) {
        (Ok(f), Ok(t), Ok(r)) => (f, t, r),
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return e,
    };
    let update_links = flag(a, "update_links", true);
    let base = ContentHash::of(&cx.vault.read(&from).unwrap_or_default());
    let cs = ChangeSet {
        changes: vec![Change::Rename { from: from.clone(), to: to.clone(), update_links }],
        run,
        title: title_for("rename", &from),
        ..ChangeSet::default()
    };
    let diff = format!("Rename {from} → {to}{}", if update_links { " (links updated)" } else { "" });
    submit(cx, ext, a, "rename", &to, cs, &diff, &base, json!({"from": from, "to": to}))
}
