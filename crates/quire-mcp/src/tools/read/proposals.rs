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
        "Your suggestions (proposals) and whether the person accepted them. Example: {} for your pending ones, {\"status\": \"applied\"}, or {\"actor\": \"all\"} for everyone's.",
        schema!({"type": "object", "properties": {
                "status": {"type": "string", "enum": ["pending", "applied", "partially_applied", "rejected", "expired", "superseded", "all"], "default": "pending"},
                "actor": {"type": "string", "default": "me", "description": "Whose: an actor id, \"me\" (you) or \"all\"."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}}),
        schema!({"type": "object", "properties": {"proposals": {"type": "array", "items": {"type": "object", "properties": {
                "id": {"type": "string"}, "status": {"type": "string"}, "title": {"type": "string"}, "files": {"type": "array", "items": {"type": "string"}},
                "created": {"type": "string"}, "decided_hunks": {"type": "integer"}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["proposals"]}),
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
    // `mine: true/false` is the 0.9 spelling of `actor: "me"/"all"`; `actor` wins.
    let actor = match (opt(a, "actor"), a.get("mine").and_then(Value::as_bool)) {
        (Some(x), _) if x == "me" => Some(cx.actor.id.clone()),
        (Some(x), _) if x == "all" => None,
        (Some(x), _) => Some(x),
        (None, Some(false)) => None,
        (None, _) => Some(cx.actor.id.clone()),
    };
    let fingerprint = bounds::fingerprint_canonical("s", a, &["mine"], &[("actor", json!(actor))]);
    let filter = ProposalFilter { status, actor, path: None };
    let mut list = cx.agent.proposals(&filter);
    list.retain(|p| p.files.iter().all(|f| visible(cx, f)));
    // Newest first by when each was made, then by id (the cursor's key).
    let key = |p: &ProposalSummary| format!("{}\u{0}{}", bounds::num_key(u64::MAX - u64::try_from(p.created).unwrap_or(0)), p.id);
    list.sort_by_cached_key(key);
    let render = |p: &ProposalSummary| json!({"id": p.id, "status": p.status, "title": p.title, "files": p.files, "created": iso(p.created), "decided_hunks": p.decided_hunks, "uri": format!("quire://proposal/{}", p.id)});
    let limit = int(a, "limit", 50, 1, 200);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let Some(page) = bounds::keyset(&list, key, render, a, &fingerprint, limit, budget) else { return stale(&fingerprint) };
    let mut text = String::new();
    for p in &list[page.range.clone()] {
        let st = serde_json::to_value(p.status).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        writeln!(text, "- {} [{st}] {} ({})", p.id, p.title, p.files.iter().map(NotePath::as_str).collect::<Vec<_>>().join(", "))
            .expect("a String takes any text");
    }
    if list.is_empty() {
        text.push_str("No suggestions.");
    }
    more_line(&mut text, "proposals", &page);
    let st = json!({"proposals": page.items, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor});
    ToolOutput::ok(text.trim_end().to_string(), st)
}
