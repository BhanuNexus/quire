// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `properties`: front-matter properties across notes.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "properties",
        "Front-matter properties",
        "The front-matter keys used in the vault with counts, value types and examples, to follow the vault's conventions when you set properties. Example: {} or {\"key\": \"status\"}.",
        schema!({"type": "object", "properties": {"key": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}}),
        schema!({"type": "object", "properties": {"keys": {"type": "array", "items": {"type": "object", "properties": {
                "key": {"type": "string"}, "count": {"type": "integer"}, "types": {"type": "array", "items": {"type": "string"}},
                "examples": {"type": "array", "items": {"type": "string"}}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"}, "next_cursor": {"type": ["string", "null"]}}, "required": ["keys"]}),
        r,
        Access::Read,
        properties,
    )
}

fn value_type(v: &FmValue) -> &'static str {
    match v {
        FmValue::List(_) => "list",
        FmValue::Map(_) => "map",
        FmValue::Str(s) => match frontmatter::kind_of(s) {
            quire_app::markdown::Kind::Number => "number",
            quire_app::markdown::Kind::Date => "date",
            quire_app::markdown::Kind::Bool => "bool",
            _ => "text",
        },
    }
}

fn properties(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let only = opt(a, "key").map(|k| k.to_lowercase());
    let ix = cx.index.read();
    let vis = cx.visibility_in(&ix);
    let mut keys: BTreeMap<String, (usize, BTreeSet<&'static str>, Vec<String>)> = BTreeMap::new();
    // In path order, so `limit` and the examples are the same on every call.
    let mut notes: Vec<_> = ix.records().filter(|r| vis.allows_with_tags(&r.path, &r.tags)).collect();
    notes.sort_unstable_by(|x, y| x.path.cmp(&y.path));
    for r in notes {
        for (k, v) in &r.properties {
            if only.as_ref().is_some_and(|o| *o != k.to_lowercase()) {
                continue;
            }
            let e = keys.entry(k.clone()).or_default();
            e.0 += 1;
            e.1.insert(value_type(v));
            let ex = match v {
                FmValue::Str(s) => s.clone(),
                FmValue::List(l) => format!("[{}]", l.join(", ")),
                FmValue::Map(_) => "{…}".into(),
            };
            if e.2.len() < 5 && !e.2.contains(&ex) && ex.len() <= 80 {
                e.2.push(ex);
            }
        }
    }
    // By key name (the cursor's key).
    let rows: Vec<_> = keys.into_iter().collect();
    drop(ix);
    let fingerprint = bounds::fingerprint("p", a);
    let limit = int(a, "limit", 100, 1, 500);
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let render = |(k, (n, t, ex)): &(String, (usize, BTreeSet<&'static str>, Vec<String>))| json!({"key": k, "count": n, "types": t, "examples": ex});
    let Some(page) = bounds::keyset(&rows, |(k, _)| k.clone(), render, a, &fingerprint, limit, budget) else { return stale(&fingerprint) };
    let mut text = String::new();
    for (k, (n, types, ex)) in &rows[page.range.clone()] {
        writeln!(text, "- {k}: {n} notes, {} (e.g. {})", types.iter().copied().collect::<Vec<_>>().join("/"), ex.join("; "))
            .expect("a String takes any text");
    }
    if page.items.is_empty() {
        text.push_str("No properties.");
    }
    more_line(&mut text, "properties", &page);
    ToolOutput::ok(
        text.trim_end().to_string(),
        json!({"keys": page.items, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor}),
    )
}
