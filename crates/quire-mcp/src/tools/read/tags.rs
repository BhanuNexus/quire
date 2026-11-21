// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `tags`: the vault's tags and their counts.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{Access, Annotations, BTreeMap, CallExt, Tool, ToolCx, ToolOutput, Value, bounds, int, json, more_line, s, stale, tool};

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "tags",
        "Tags",
        "Every tag with how many notes use it; nested tags appear as written (project/alpha). Example: {\"prefix\": \"project\"}.",
        schema!({"type": "object", "properties": {"prefix": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}}),
        schema!({"type": "object", "properties": {"tags": {"type": "array", "items": {"type": "object", "properties": {"tag": {"type": "string"}, "count": {"type": "integer"}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["tags"]}),
        r,
        Access::Read,
        tags,
    )
}

fn tags(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let prefix = s(a, "prefix").trim_start_matches('#').to_lowercase();
    let ix = cx.index.read();
    let vis = cx.visibility_in(&ix);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in ix.records().filter(|r| vis.allows_with_tags(&r.path, &r.tags)) {
        let mut mine: Vec<&String> = r.tags.iter().collect();
        mine.sort();
        mine.dedup();
        for t in mine {
            if !prefix.is_empty() && !t.to_lowercase().starts_with(&prefix) {
                continue;
            }
            // A tag seen before is counted without copying it.
            match counts.get_mut(t.as_str()) {
                Some(n) => *n += 1,
                None => {
                    counts.insert(t.clone(), 1);
                }
            }
        }
    }
    // By name (the cursor's key); a count is data, so one that changes between pages repeats no row.
    let rows: Vec<(String, usize)> = counts.into_iter().collect();
    drop(ix);
    let fingerprint = bounds::fingerprint("g", a);
    let limit = int(a, "limit", 200, 1, 1000);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let render = |(t, n): &(String, usize)| json!({"tag": t, "count": n});
    let Some(page) = bounds::keyset(&rows, |(t, _)| t.clone(), render, a, &fingerprint, limit, budget) else { return stale(&fingerprint) };
    let mut text = if page.items.is_empty() {
        "No tags.".to_string()
    } else {
        rows[page.range.clone()].iter().map(|(t, n)| format!("#{t} ({n})")).collect::<Vec<_>>().join(", ")
    };
    more_line(&mut text, "tags", &page);
    ToolOutput::ok(text, json!({"tags": page.items, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor}))
}
