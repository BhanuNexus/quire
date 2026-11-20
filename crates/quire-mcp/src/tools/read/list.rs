// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `list`: notes and folders, with filters.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "list",
        "List notes",
        "List notes, optionally in a folder, with a tag, or matching a property filter. Example: {\"folder\": \"Projects/\", \"where\": \"status != done\", \"sort\": \"modified\"}. Pass next_cursor back as cursor for more.",
        schema!({"type": "object", "properties": {
                "folder": {"type": "string"}, "tag": {"type": "string"},
                "where": {"type": "string", "description": "e.g. \"status = active and due < 2026-12-01\""},
                "sort": {"type": "string", "enum": ["path", "modified", "title"], "default": "path"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100},
                "cursor": {"type": "string"}}}),
        schema!({"type": "object", "properties": {
                "notes": {"type": "array", "items": {"type": "object", "properties": {
                    "uri": {"type": "string"}, "path": {"type": "string"}, "title": {"type": "string"}, "modified": {"type": ["string", "null"]},
                    "tags": {"type": "array", "items": {"type": "string"}}, "words": {"type": "integer"}}}},
                "next_cursor": {"type": ["string", "null"]}}, "required": ["notes"]}),
        r,
        Access::Read,
        list_tool,
    )
}

fn list_tool(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let ix = cx.index.read();
    let vis = cx.visibility_in(&ix);
    let folder = s(a, "folder").trim_matches('/').to_lowercase();
    let under = format!("{folder}/");
    let tag = s(a, "tag");
    let limit = int(a, "limit", 100, 1, 500);
    let allowed: Option<BTreeSet<NotePath>> = match opt(a, "where") {
        None => None,
        // Only notes the caller may see are run through the condition.
        Some(w) => match views::parse_where(&w) {
            Ok(spec) => Some(
                views::run_over(&spec, &ix, &chrono::Local::now().format("%Y-%m-%d").to_string(), None, &|p| vis.allows(p))
                    .rows
                    .into_iter()
                    .map(|r| r.path)
                    .collect(),
            ),
            Err(e) => return ToolOutput::error("invalid_args", format!("`where`: {e}")),
        },
    };
    let mut notes: Vec<_> = ix
        .records()
        .filter(|r| folder.is_empty() || r.path.as_str().to_lowercase().starts_with(&under))
        .filter(|r| tag.is_empty() || r.has_tag(&tag))
        .filter(|r| allowed.as_ref().is_none_or(|set| set.contains(&r.path)))
        .filter(|r| vis.allows_with_tags(&r.path, &r.tags))
        .collect();
    match s(a, "sort").as_str() {
        // Newest first; the index knows each note's time, so this reads no file.
        "modified" => notes.sort_by_cached_key(|r| (std::cmp::Reverse(modified_ms(cx, &ix, &r.path)), r.path.clone())),
        "title" => notes.sort_by_cached_key(|r| (r.title.to_lowercase(), r.path.clone())),
        _ => notes.sort_by_cached_key(|r| (r.path.as_str().to_lowercase(), r.path.clone())),
    }
    let mut key = a.clone();
    if let Some(o) = key.as_object_mut() {
        o.remove("cursor");
    }
    let fingerprint = format!("l:{}", ContentHash::of(&key.to_string()).hex());
    let skip = match bounds::start(a, &fingerprint) {
        Start::Beginning => 0,
        Start::At(n) => n,
        Start::Stale => return stale(&fingerprint),
    };
    let page: Vec<_> = notes.iter().skip(skip).take(limit).collect();
    let next = (skip + page.len() < notes.len()).then(|| Cursor { offset: skip + page.len(), version: fingerprint.clone() }.encode());
    let mut text = if page.is_empty() {
        "No notes.\n".to_string()
    } else {
        page.iter().fold(String::new(), |mut s, r| {
            writeln!(s, "- {} — {}", r.path, r.title).expect("a String takes any text");
            s
        })
    };
    if let Some(c) = &next {
        write!(text, "… {} more; call list again with cursor=\"{c}\"", notes.len() - skip - page.len()).expect("a String takes any text");
    }
    let rows: Vec<Value> = page
        .iter()
        .map(|r| {
            let modified = modified_ms(cx, &ix, &r.path).map(iso);
            json!({"uri": note_uri(&r.path), "path": r.path, "title": r.title, "modified": modified, "tags": r.tags, "words": r.stats.words})
        })
        .collect();
    ToolOutput::ok(text.trim_end().to_string(), json!({"notes": rows, "next_cursor": next}))
}
