// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `read_version`: a note as it was at an earlier version.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "read_version",
        "Read an old version",
        "The text of a note as it was: at a version (sha256:… from view or history) or as of a time. Example: {\"path\": \"Rope Notes.md\", \"as_of\": \"2026-11-01\"}.",
        schema!({"type": "object", "properties": {
                "path": {"type": "string"}, "version": {"type": "string"},
                "as_of": {"type": "string", "description": "RFC 3339, YYYY-MM-DD (a UTC day), or a time back from now: 30m, 24h, 7d, 2w"},
                "max_tokens": {"type": "integer", "default": 4000}, "cursor": {"type": "string"}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {"text": {"type": "string"}, "version": {"type": "string"}, "at": {"type": ["string", "null"]},
                "actor": {"type": "string"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["text", "version"]}),
        r,
        Access::Read,
        read_version,
    )
}

fn read_version(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (text, at, actor) = match (version_arg(a, "version"), opt(a, "as_of")) {
        (Err(e), _) => return e,
        (Ok(Some(v)), _) => match ext.served.get(&path, &v) {
            Some(t) => (t, None, String::new()),
            None => return ToolOutput::error("not_found", format!("No version {v} of {path} is known. Use `history` to list versions.")),
        },
        (Ok(None), Some(t)) => {
            let when = match parse_time(&t, cx.agent.now_ms()) {
                Ok(w) => w,
                Err(why) => return ToolOutput::error("invalid_args", format!("`as_of`: {why}")),
            };
            let Some(snap) = cx.vault.history(&path).into_iter().find(|s| s.at_ms <= when) else {
                return ToolOutput::error("not_found", format!("No saved version of {path} is that old."));
            };
            let actor = cx
                .vault
                .read_provenance(&snap)
                .and_then(|p| p.get("actor").and_then(|a| a.get("id")).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "human".into());
            match cx.vault.read_snapshot(&snap) {
                Ok(t) => (t, Some(iso(snap.at_ms)), actor),
                Err(e) => return ToolOutput::error("io", e.to_string()),
            }
        }
        (Ok(None), None) => return ToolOutput::error("invalid_args", "Give `version` or `as_of`."),
    };
    let version = ext.served.store(&text);
    let w = match window(&text, &version, a) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let mut out = format!("# {path} at version {version} (lines {}–{} of {})\n{}", w.first, w.shown_to, w.total, w.body);
    if let Some(c) = &w.next {
        write!(out, "… cut; continue with cursor=\"{c}\"").expect("a String takes any text");
    }
    let st = json!({"text": text.lines().skip(w.first - 1).take(w.shown_to + 1 - w.first.min(w.shown_to + 1)).collect::<Vec<_>>().join("\n"),
        "version": version, "at": at, "actor": actor, "truncated": w.next.is_some(), "next_cursor": w.next});
    ToolOutput::ok(out, st)
}
