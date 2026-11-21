// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `recent_activity`: what changed lately, and who changed it.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "recent_activity",
        "Recent changes",
        "What changed in the vault since a time: agent writes, suggestions and decisions from Quire's ledger, and notes the person edited. Example: {\"since\": \"24h\"} or {\"since\": \"2026-11-01\", \"actor\": \"mcp:claude-code\"}.",
        schema!({"type": "object", "properties": {
                "since": {"type": "string", "description": "\"24h\", \"7d\", YYYY-MM-DD or RFC 3339."},
                "actor": {"type": "string"},
                "kinds": {"type": "array", "items": {"type": "string", "enum": ["edit", "create", "rename", "delete", "proposal", "decision"]}},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}, "required": ["since"]}),
        schema!({"type": "object", "properties": {
                "events": {"type": "array", "items": {"type": "object", "properties": {
                    "at": {"type": "string"}, "actor": {"type": "string"}, "op": {"type": "string"}, "path": {"type": "string"},
                    "to": {"type": "string"}, "proposal": {"type": "string"}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["events"]}),
        r,
        Access::Read,
        recent_activity,
    )
}

fn op_kind(op: Op) -> &'static str {
    match op {
        Op::Write => "edit",
        Op::Create => "create",
        Op::Rename => "rename",
        Op::Delete => "delete",
        Op::Propose => "proposal",
        Op::Accept | Op::Reject => "decision",
        Op::Revert => "revert",
        Op::Read => "read",
        Op::Batch => "batch",
    }
}

fn recent_activity(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let now = cx.agent.now_ms();
    let Some(since) = opt(a, "since").and_then(|t| parse_time(&t, now)) else {
        return ToolOutput::error("invalid_args", "`since` must be like \"24h\", \"7d\", \"2026-11-01\" or an RFC 3339 time.");
    };
    let actor = opt(a, "actor");
    let kinds = list(a, "kinds");
    let limit = int(a, "limit", 100, 1, 500);
    let mut events: Vec<(i64, Value)> = cx
        .agent
        .log(&LedgerQuery { since: Some(since), actor: actor.clone(), run: None })
        .into_iter()
        .filter(|e| !matches!(e.op, Op::Read | Op::Batch))
        // Both paths of an event: a rename into a hidden place names it in `to`.
        .filter(|e| e.path.as_ref().is_none_or(|p| visible(cx, p)) && e.to.as_ref().is_none_or(|p| visible(cx, p)))
        .map(|e| {
            let v = json!({"at": iso(e.at), "actor": e.actor.id, "op": op_kind(e.op), "path": e.path, "to": e.to, "proposal": e.proposal, "after": e.after});
            (e.at, v)
        })
        .collect();
    if actor.as_deref().is_none_or(|x| x == "human") {
        let ix = cx.index.read();
        let vis = cx.visibility_in(&ix);
        // The index knows each note's time: no file is read for a note older than `since`.
        for r in ix.records() {
            let Some(ms) = modified_ms(cx, &ix, &r.path) else { continue };
            if ms >= since && vis.allows_with_tags(&r.path, &r.tags) && last_actor(cx, &r.path) == "human" {
                events.push((ms, json!({"at": iso(ms), "actor": "human", "op": "edit", "path": r.path})));
            }
        }
    }
    events.retain(|(_, v)| kinds.is_empty() || kinds.iter().any(|k| v["op"] == k.as_str()));
    // Newest first, then by who, what and where (the cursor's key), so an event logged between
    // pages makes none repeat.
    let key = |(at, v): &(i64, Value)| {
        let part = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let newest_first = bounds::num_key(u64::MAX - u64::try_from(*at).unwrap_or(0));
        [newest_first, part("actor"), part("op"), part("path"), part("to"), part("proposal"), part("after")].join("\u{0}")
    };
    events.sort_by_cached_key(key);
    events.dedup_by(|x, y| key(x) == key(y));
    let render = |(_, v): &(i64, Value)| {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("after");
        }
        v
    };
    let fingerprint = bounds::fingerprint("r", a);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let Some(page) = bounds::keyset(&events, key, render, a, &fingerprint, limit, budget) else { return stale(&fingerprint) };
    let n = page.items.len();
    let mut text = format!("{n} change{} since {}:\n", if n == 1 { "" } else { "s" }, iso(since));
    for e in &page.items {
        writeln!(
            text,
            "- {} {} {} {}",
            e["at"].as_str().unwrap_or(""),
            e["actor"].as_str().unwrap_or(""),
            e["op"].as_str().unwrap_or(""),
            e["path"].as_str().unwrap_or("")
        )
        .expect("a String takes any text");
    }
    more_line(&mut text, "recent_activity", &page);
    let st = json!({"events": page.items, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor});
    ToolOutput::ok(text.trim_end().to_string(), st)
}
