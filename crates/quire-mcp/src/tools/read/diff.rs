// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `diff`: the change between two versions.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "diff",
        "Compare versions",
        "A unified diff of a note between two versions (from view, history or an edit result); `to` defaults to the current text. Example: {\"path\": \"Rope Notes.md\", \"from\": \"sha256:1a2b…\"}.",
        schema!({"type": "object", "properties": {"path": {"type": "string"}, "from": {"type": "string"}, "to": {"type": "string"}}, "required": ["path", "from"]}),
        schema!({"type": "object", "properties": {"unified": {"type": "string"}, "added": {"type": "integer"}, "removed": {"type": "integer"}}, "required": ["unified"]}),
        r,
        Access::Read,
        diff,
    )
}

fn diff(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let get = |k: &str| -> Result<Option<String>, ToolOutput> {
        match version_arg(a, k)? {
            None => Ok(None),
            Some(v) => ext.served.get(&path, &v).map(Some).ok_or_else(|| {
                ToolOutput::error("not_found", format!("No version {v} of {path} is known. Use `history` to list versions."))
            }),
        }
    };
    let from = match get("from") {
        Ok(Some(t)) => t,
        Ok(None) => return ToolOutput::error("invalid_args", "`from` is required."),
        Err(e) => return e,
    };
    let to = match get("to") {
        Ok(Some(t)) => t,
        Ok(None) => cx.vault.read(&path).unwrap_or_default(),
        Err(e) => return e,
    };
    let u = unified(&from, &to, path.as_str());
    let (added, removed) = quire_app::history_ops::diff_stats(&from, &to);
    let text = if u.is_empty() { "No differences.".to_string() } else { u.clone() };
    ToolOutput::ok(text, json!({"unified": u, "added": added, "removed": removed}))
}
