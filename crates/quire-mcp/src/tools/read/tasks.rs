// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `tasks`: tasks from notes.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "tasks",
        "Tasks",
        "Checklist tasks (- [ ] …) across the vault with their dates, priority and recurrence. Example: {\"status\": \"open\", \"due_before\": \"2026-11-10\"}.",
        schema!({"type": "object", "properties": {
                "status": {"type": "string", "enum": ["open", "done", "all"], "default": "open"},
                "due_before": {"type": "string", "description": "YYYY-MM-DD"}, "due_after": {"type": "string"},
                "path": {"type": "string"}, "tag": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200}}}),
        schema!({"type": "object", "properties": {"tasks": {"type": "array", "items": {"type": "object", "properties": {
                "path": {"type": "string"}, "line": {"type": "integer"}, "text": {"type": "string"}, "due": {"type": ["string", "null"]},
                "scheduled": {"type": ["string", "null"]}, "start": {"type": ["string", "null"]},
                "completed": {"type": ["string", "null"]}, "reminder": {"type": ["string", "null"]},
                "time": {"type": ["string", "null"], "description": "a day-planner block, 14:00-15:30"},
                "priority": {"type": ["integer", "null"], "description": "0 highest, 1 high, 2 medium, 3 low, 4 lowest; null is normal"},
                "recurrence": {"type": ["string", "null"]},
                "done": {"type": "boolean"}}}}}, "required": ["tasks"]}),
        r,
        Access::Read,
        tasks,
    )
}

fn tasks(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let status = s(a, "status");
    let (want_open, want_done) = match status.as_str() {
        "done" => (false, true),
        "all" => (true, true),
        _ => (true, false),
    };
    let before = opt(a, "due_before");
    let after = opt(a, "due_after");
    let only_path = opt(a, "path");
    let tag = opt(a, "tag");
    let limit = int(a, "limit", 200, 1, 1000);
    let ix = cx.index.read();
    let mut rows = Vec::new();
    let mut text = String::new();
    for r in ix.records().filter(|r| visible(cx, &r.path)) {
        if only_path.as_ref().is_some_and(|p| !r.path.as_str().eq_ignore_ascii_case(p) && !r.path.without_ext().eq_ignore_ascii_case(p)) {
            continue;
        }
        if tag.as_ref().is_some_and(|t| !r.has_tag(t)) {
            continue;
        }
        let src = ix.source(&r.path).unwrap_or_default();
        for t in &r.tasks {
            if (t.done && !want_done) || (!t.done && !want_open) {
                continue;
            }
            if before.as_ref().is_some_and(|b| t.due.as_ref().is_none_or(|d| d.as_str() >= b.as_str())) {
                continue;
            }
            if after.as_ref().is_some_and(|b| t.due.as_ref().is_none_or(|d| d.as_str() <= b.as_str())) {
                continue;
            }
            if rows.len() >= limit {
                break;
            }
            let line = src[..t.offset.min(src.len())].matches('\n').count() + 1;
            let _ = writeln!(
                text,
                "- [{}] {} ({}:{line}{})",
                if t.done { "x" } else { " " },
                t.text,
                r.path,
                t.due.as_deref().map(|d| format!(", due {d}")).unwrap_or_default()
            );
            rows.push(json!({"path": r.path, "line": line, "text": t.text, "due": t.due, "scheduled": t.scheduled, "start": t.start,
                "completed": t.done_on, "reminder": t.reminder, "time": t.time_block.as_ref().map(|(a, b)| format!("{a}-{b}")),
                "priority": t.priority, "recurrence": t.recurrence, "done": t.done}));
        }
    }
    if rows.is_empty() {
        text.push_str("No tasks.");
    }
    ToolOutput::ok(text.trim_end().to_string(), json!({"tasks": rows}))
}
