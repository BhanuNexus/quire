// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `diff`: the change between two versions.
//!
//! Its definition (name, schema, description) and its handler sit together.

use std::fmt::Write as _;

use super::{
    Access, Annotations, CallExt, ContentHash, Cursor, Start, Tool, ToolCx, ToolOutput, Value, bounds, existing, json, stale, tool,
    unified, version_any,
};

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "diff",
        "Compare versions",
        "A unified diff of a note between two versions (from view, history or an edit result); `to_version` defaults to the current text. Example: {\"path\": \"Rope Notes.md\", \"from_version\": \"sha256:1a2b…\"}.",
        schema!({"type": "object", "properties": {"path": {"type": "string"}, "from_version": {"type": "string"}, "to_version": {"type": "string"},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}, "required": ["path", "from_version"]}),
        schema!({"type": "object", "properties": {"unified": {"type": "string"}, "added": {"type": "integer"}, "removed": {"type": "integer"},
                "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["unified"]}),
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
    // `from` and `to` are the 0.9 spellings.
    let get = |keys: &[&str]| -> Result<Option<String>, ToolOutput> {
        match version_any(a, keys)? {
            None => Ok(None),
            Some(v) => ext.served.get(&path, &v).map(Some).ok_or_else(|| {
                ToolOutput::error("not_found", format!("No version {v} of {path} is known. Use `history` to list versions."))
            }),
        }
    };
    let from = match get(&["from_version", "from"]) {
        Ok(Some(t)) => t,
        Ok(None) => return ToolOutput::error("invalid_args", "`from_version` is required."),
        Err(e) => return e,
    };
    let to = match get(&["to_version", "to"]) {
        Ok(Some(t)) => t,
        Ok(None) => cx.vault.read(&path).unwrap_or_default(),
        Err(e) => return e,
    };
    let whole = unified(&from, &to, path.as_str());
    let (added, removed) = quire_app::history_ops::diff_stats(&from, &to);
    // In lines, within `max_tokens`; the cursor is the next line of this diff (another diff is stale).
    let version = format!("d:{}", ContentHash::of(&whole).hex());
    let first = match bounds::start(a, &version) {
        Start::Beginning => 0,
        Start::At(n) => n,
        Start::Stale => return stale(&version),
    };
    let lines: Vec<&str> = whole.split_inclusive('\n').collect();
    let shown = bounds::fit(lines.iter().skip(first).copied(), bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS));
    let end = (first + shown).min(lines.len());
    let u: String = lines.get(first..end).unwrap_or_default().concat();
    let next = (end < lines.len()).then(|| Cursor::at(end, version.clone()).encode());
    let mut text = if whole.is_empty() { "No differences.".to_string() } else { u.clone() };
    if let Some(c) = &next {
        write!(text, "\n… {} more lines; call diff again with cursor=\"{c}\"", lines.len() - end).expect("a String takes any text");
    }
    ToolOutput::ok(text, json!({"unified": u, "added": added, "removed": removed, "truncated": next.is_some(), "next_cursor": next}))
}
