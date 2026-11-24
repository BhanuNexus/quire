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
        "Checklist tasks (- [ ] …) across the vault with their dates, priority and recurrence; in one note (path), a folder, or notes with tags. Example: {\"status\": \"open\", \"due_before\": \"2026-11-10\", \"folder\": \"Projects/\"}.",
        schema!({"type": "object", "properties": {
                "status": {"type": "string", "enum": ["open", "done", "all"], "default": "open"},
                "due_before": {"type": "string", "description": "YYYY-MM-DD"}, "due_after": {"type": "string"},
                "path": {"type": "string", "description": "One note."}, "folder": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Every tag must be on the note."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 200},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}}),
        schema!({"type": "object", "properties": {"tasks": {"type": "array", "items": {"type": "object", "properties": {
                "path": {"type": "string"}, "line": {"type": "integer"}, "text": {"type": "string"}, "due": {"type": ["string", "null"]},
                "scheduled": {"type": ["string", "null"]}, "start": {"type": ["string", "null"]},
                "completed": {"type": ["string", "null"]}, "reminder": {"type": ["string", "null"]},
                "time": {"type": ["string", "null"], "description": "a day-planner block, 14:00-15:30"},
                "priority": {"type": ["integer", "null"], "description": "0 highest, 1 high, 2 medium, 3 low, 4 lowest; null is normal"},
                "recurrence": {"type": ["string", "null"]},
                "done": {"type": "boolean"}}}}, "total": {"type": "integer"}, "truncated": {"type": "boolean"},
                "next_cursor": {"type": ["string", "null"]}}, "required": ["tasks"]}),
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
    let folder = opt(a, "folder").map(|f| format!("{}/", f.trim_matches('/').to_lowercase()));
    // `tag` (one) is the 0.9 spelling of `tags`.
    let tags: Vec<String> = list_any(a, &["tags", "tag"]).into_iter().map(|t| t.trim_start_matches('#').to_string()).collect();
    let limit = int(a, "limit", 200, 1, 500);
    let ix = cx.index.read();
    let vis = cx.visibility_in(&ix);
    // In path order, then the order in the note: what the cursor's key follows.
    let mut notes: Vec<_> = ix.records().filter(|r| vis.allows_with_tags(&r.path, &r.tags)).collect();
    notes.sort_unstable_by(|x, y| x.path.cmp(&y.path));
    let mut rows = Vec::new();
    for r in notes {
        if only_path.as_ref().is_some_and(|p| !r.path.as_str().eq_ignore_ascii_case(p) && !r.path.without_ext().eq_ignore_ascii_case(p)) {
            continue;
        }
        if !tags.iter().all(|t| r.has_tag(t)) || folder.as_ref().is_some_and(|f| !r.path.as_str().to_lowercase().starts_with(f.as_str())) {
            continue;
        }
        let first = rows.len();
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
            rows.push((r, t));
        }
        rows[first..].sort_by_key(|(_, t)| t.offset);
    }
    // Only for the rows returned: the line (from the index's line starts) and the row itself.
    let render = |(r, t): &(&quire_app::index::NoteRecord, &quire_app::markdown::extract::Task)| {
        let line = ix.line_of(&r.path, t.offset);
        json!({"path": r.path, "line": line, "text": t.text, "due": t.due, "scheduled": t.scheduled, "start": t.start,
            "completed": t.done_on, "reminder": t.reminder, "time": t.time_block.as_ref().map(|(a, b)| format!("{a}-{b}")),
            "priority": t.priority, "recurrence": t.recurrence, "done": t.done})
    };
    let key = |(r, t): &(&quire_app::index::NoteRecord, &quire_app::markdown::extract::Task)| {
        format!("{}\u{0}{}", r.path, bounds::num_key(t.offset as u64))
    };
    let fingerprint = bounds::fingerprint_canonical("t", a, &["tag"], &[("tags", json!(tags))]);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let Some(page) = bounds::keyset(&rows, key, render, a, &fingerprint, limit, budget) else { return stale(&fingerprint) };
    let mut text = String::new();
    for row in &page.items {
        writeln!(
            text,
            "- [{}] {} ({}:{}{})",
            if row["done"] == true { "x" } else { " " },
            row["text"].as_str().unwrap_or_default(),
            row["path"].as_str().unwrap_or_default(),
            row["line"],
            row["due"].as_str().map(|d| format!(", due {d}")).unwrap_or_default()
        )
        .expect("a String takes any text");
    }
    if page.items.is_empty() {
        text.push_str("No tasks.");
    }
    more_line(&mut text, "tasks", &page);
    ToolOutput::ok(
        text.trim_end().to_string(),
        json!({"tasks": page.items, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor}),
    )
}
