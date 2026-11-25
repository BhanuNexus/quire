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
        "List notes, optionally in a folder, with tags, changed in a time range, or matching a property filter. Example: {\"folder\": \"Projects/\", \"where\": \"status != done\", \"since\": \"7d\", \"sort\": \"modified\"}. Pass next_cursor back as cursor for more.",
        schema!({"type": "object", "properties": {
                "folder": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}, "description": "Every tag must be on the note."},
                "since": {"type": "string", "description": "RFC 3339, YYYY-MM-DD (a UTC day), or a time back from now: 30m, 24h, 7d, 2w"}, "until": {"type": "string", "description": "As since; a date takes in that whole UTC day"},
                "where": {"type": "string", "description": "e.g. \"status = active and due < 2026-12-01\""},
                "sort": {"type": "string", "enum": ["path", "modified", "title"], "default": "path"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100},
                "max_tokens": {"type": "integer"}, "cursor": {"type": "string"}}}),
        schema!({"type": "object", "properties": {
                "notes": {"type": "array", "items": {"type": "object", "properties": {
                    "uri": {"type": "string"}, "path": {"type": "string"}, "title": {"type": "string"}, "modified": {"type": ["string", "null"]},
                    "tags": {"type": "array", "items": {"type": "string"}}, "words": {"type": "integer"}}}},
                "total": {"type": "integer"}, "truncated": {"type": "boolean"},
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
    // `tag` (one) is the 0.9 spelling of `tags`.
    let tags: Vec<String> = list_any(a, &["tags", "tag"]).into_iter().map(|t| t.trim_start_matches('#').to_string()).collect();
    let now = cx.agent.now_ms();
    let (since, until) = match (time_any(a, &["since"], now), until_any(a, &["until"], now)) {
        (Ok(s), Ok(u)) => (s, u),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let limit = int(a, "limit", 100, 1, 500);
    let allowed: Option<BTreeSet<NotePath>> = match opt(a, "where") {
        None => None,
        // Only notes the caller may see are run through the condition.
        Some(w) => match views::parse_where(&w) {
            Ok(spec) => Some(
                views::run_over(&spec, &ix, &quire_base::dates::today_ymd(), None, &|p| vis.allows(p))
                    .rows
                    .into_iter()
                    .map(|r| r.path)
                    .collect(),
            ),
            Err(e) => return ToolOutput::error("invalid_args", format!("`where`: {e}")),
        },
    };
    let notes = ix
        .records()
        .filter(|r| folder.is_empty() || r.path.as_str().to_lowercase().starts_with(&under))
        .filter(|r| tags.iter().all(|t| r.has_tag(t)))
        // The index knows each note's time: a range reads no file.
        .filter(|r| {
            (since.is_none() && until.is_none())
                || modified_ms(cx, ix.modified(&r.path), &r.path)
                    .is_some_and(|ms| since.is_none_or(|s| ms >= s) && until.is_none_or(|u| ms <= u))
        })
        .filter(|r| allowed.as_ref().is_none_or(|set| set.contains(&r.path)))
        .filter(|r| vis.allows_with_tags(&r.path, &r.tags));
    // Each note's place in the order, as text that sorts the same way: the cursor carries the
    // last one, so the next page starts after it whatever was added or removed meanwhile.
    let sort = s(a, "sort");
    let sort_key = |r: &&quire_app::index::NoteRecord| -> String {
        let lead = match sort.as_str() {
            // Newest first; the index knows each note's time, so this reads no file.
            "modified" => {
                bounds::num_key(u64::MAX - modified_ms(cx, ix.modified(&r.path), &r.path).map_or(0, |ms| u64::try_from(ms).unwrap_or(0)))
            }
            "title" => r.title.to_lowercase(),
            _ => r.path.as_str().to_lowercase(),
        };
        let mut key = lead;
        key.push('\u{0}');
        key.push_str(r.path.as_str());
        key
    };
    let mut keyed: Vec<(String, &quire_app::index::NoteRecord)> = notes.map(|r| (sort_key(&r), r)).collect();
    keyed.sort_unstable_by(|x, y| x.0.cmp(&y.0));
    let fingerprint = bounds::fingerprint_canonical("l", a, &["tag"], &[("tags", json!(tags))]);
    let render = |(_, r): &(String, &quire_app::index::NoteRecord)| {
        let modified = modified_ms(cx, ix.modified(&r.path), &r.path).map(iso);
        json!({"uri": note_uri(&r.path), "path": r.path, "title": r.title, "modified": modified, "tags": r.tags, "words": r.stats.words})
    };
    let budget = bounds::max_tokens(a, bounds::DEFAULT_MAX_TOKENS);
    let Some(page) = bounds::keyset(&keyed, |(k, _)| k.clone(), render, a, &fingerprint, limit, budget) else {
        return stale(&fingerprint);
    };
    let mut text = if page.items.is_empty() {
        "No notes.\n".to_string()
    } else {
        keyed[page.range.clone()].iter().fold(String::new(), |mut s, (_, r)| {
            writeln!(s, "- {} — {}", r.path, r.title).expect("a String takes any text");
            s
        })
    };
    more_line(&mut text, "list", &page);
    ToolOutput::ok(
        text.trim_end().to_string(),
        json!({"notes": page.items, "total": page.total, "truncated": page.truncated, "next_cursor": page.next_cursor}),
    )
}
