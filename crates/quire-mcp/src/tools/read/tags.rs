// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `tags`: the vault's tags and their counts.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{Access, Annotations, BTreeMap, CallExt, Tool, ToolCx, ToolOutput, Value, json, s, tool};

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "tags",
        "Tags",
        "Every tag with how many notes use it; nested tags appear as written (project/alpha). Example: {\"prefix\": \"project\"}.",
        schema!({"type": "object", "properties": {"prefix": {"type": "string"}}}),
        schema!({"type": "object", "properties": {"tags": {"type": "array", "items": {"type": "object", "properties": {"tag": {"type": "string"}, "count": {"type": "integer"}}}}}, "required": ["tags"]}),
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
            if t.to_lowercase().starts_with(&prefix) {
                *counts.entry(t.clone()).or_default() += 1;
            }
        }
    }
    let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
    rows.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    let text = if rows.is_empty() {
        "No tags.".to_string()
    } else {
        rows.iter().map(|(t, n)| format!("#{t} ({n})")).collect::<Vec<_>>().join(", ")
    };
    ToolOutput::ok(text, json!({"tags": rows.iter().map(|(t, n)| json!({"tag": t, "count": n})).collect::<Vec<_>>()}))
}
