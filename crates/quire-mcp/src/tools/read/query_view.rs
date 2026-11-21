// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `query_view`: runs a saved view.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "query_view",
        "Run a view",
        "Run a quire-view query over note properties and get a table. The spec is the text of a quire-view block. Example: \"from: #project\\nwhere: status != done\\nsort: due asc\\nfields: [title, status, due]\".",
        schema!({"type": "object", "properties": {"spec": {"type": "string"}, "max_rows": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}, "required": ["spec"]}),
        schema!({"type": "object", "properties": {"columns": {"type": "array", "items": {"type": "string"}},
                "rows": {"type": "array", "items": {"type": "object"}}, "groups": {"type": "array", "items": {"type": "string"}}, "total": {"type": "integer"},
                "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["columns", "rows"]}),
        r,
        Access::Read,
        query_view,
    )
}

fn query_view(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let spec = match req(a, "spec").map(|s| views::parse(&s)) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return ToolOutput::error("invalid_args", format!("The view spec is invalid: {e}")),
        Err(e) => return e,
    };
    let max = int(a, "max_rows", 100, 1, 500);
    let ix = cx.index.read();
    let vis = cx.visibility_in(&ix);
    // Only notes this client may see count: rows, groups and the total alike.
    let res = views::run_over(&spec, &ix, &chrono::Local::now().format("%Y-%m-%d").to_string(), None, &|p| vis.allows(p));
    drop(ix);
    let mut columns = vec!["path".to_string()];
    columns.extend(spec.fields.iter().cloned());
    let render = |(_, r): &(usize, &views::ViewRow)| {
        let mut o = serde_json::Map::new();
        o.insert("path".into(), json!(r.path));
        o.insert("title".into(), json!(r.title));
        for (f, c) in spec.fields.iter().zip(&r.cells) {
            o.insert(f.clone(), json!(c));
        }
        if !r.group.is_empty() {
            o.insert("group".into(), json!(r.group));
        }
        Value::Object(o)
    };
    // The view's own order (its `sort:`), which its rows do not carry as a key: the cursor holds the
    // position. A grouped view (a board) is one answer, cut by size, with no cursor.
    let rows: Vec<(usize, &views::ViewRow)> = res.rows.iter().enumerate().collect();
    let grouped = spec.group.is_some();
    let fingerprint = bounds::fingerprint("v", a);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let args = if grouped { json!({}) } else { a.clone() };
    let Some(mut page) = bounds::keyset(&rows, |(i, _)| bounds::num_key(*i as u64), render, &args, &fingerprint, max, budget) else {
        return stale(&fingerprint);
    };
    if grouped {
        page.next_cursor = None;
    }
    // Rows the view itself left out (its `limit:`) count as cut too.
    let truncated = page.truncated || res.total > res.rows.len();
    let mut text = format!("| {} |\n|{}|\n", columns.join(" | "), " --- |".repeat(columns.len()));
    for (_, r) in &rows[page.range.clone()] {
        writeln!(text, "| {} | {} |", r.path, r.cells.iter().map(|c| c.replace('|', "\\|")).collect::<Vec<_>>().join(" | "))
            .expect("a String takes any text");
    }
    write!(text, "{} of {} rows", page.range.end - page.range.start, res.total).expect("a String takes any text");
    more_line(&mut text, "query_view", &page);
    ToolOutput::ok(
        text,
        json!({"columns": columns, "rows": page.items, "groups": res.groups, "total": res.total, "truncated": truncated, "next_cursor": page.next_cursor}),
    )
}
