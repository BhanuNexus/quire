// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `proposals`: the caller's suggestions.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "proposals",
        "Suggestions",
        "Your suggestions (proposals) and whether the person accepted them. Example: {} for your pending ones, {\"status\": \"applied\"}, or {\"mine\": false} for everyone's.",
        schema!({"type": "object", "properties": {
                "status": {"type": "string", "enum": ["pending", "applied", "partially_applied", "rejected", "expired", "superseded", "all"], "default": "pending"},
                "mine": {"type": "boolean", "default": true}}}),
        schema!({"type": "object", "properties": {"proposals": {"type": "array", "items": {"type": "object", "properties": {
                "id": {"type": "string"}, "status": {"type": "string"}, "title": {"type": "string"}, "files": {"type": "array", "items": {"type": "string"}},
                "created": {"type": "string"}, "decided_hunks": {"type": "integer"}}}}}, "required": ["proposals"]}),
        r,
        Access::Read,
        proposals,
    )
}

fn proposals(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let status = match s(a, "status").as_str() {
        "" | "pending" => Some(ProposalStatus::Pending),
        "all" => None,
        other => match serde_json::from_value::<ProposalStatus>(json!(other)) {
            Ok(st) => Some(st),
            Err(_) => return ToolOutput::error("invalid_args", format!("Unknown status {other}.")),
        },
    };
    let mine = flag(a, "mine", true);
    let filter = ProposalFilter { status, actor: mine.then(|| cx.actor.id.clone()), path: None };
    let mut list = cx.agent.proposals(&filter);
    list.retain(|p| p.files.iter().all(|f| visible(cx, f)));
    list.sort_by_key(|p| std::cmp::Reverse(p.updated));
    let mut text = String::new();
    for p in &list {
        let st = serde_json::to_value(p.status).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let _ = writeln!(text, "- {} [{st}] {} ({})", p.id, p.title, p.files.iter().map(NotePath::as_str).collect::<Vec<_>>().join(", "));
    }
    if list.is_empty() {
        text.push_str("No suggestions.");
    }
    let rows: Vec<Value> = list
        .iter()
        .map(|p| json!({"id": p.id, "status": p.status, "title": p.title, "files": p.files, "created": iso(p.created), "decided_hunks": p.decided_hunks, "uri": format!("quire://proposal/{}", p.id)}))
        .collect();
    ToolOutput::ok(text.trim_end().to_string(), json!({"proposals": rows}))
}
