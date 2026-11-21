// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `search`: keyword and meaning search over note sections.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "search",
        "Search notes",
        "Find the sections of notes that match a query, by keywords and meaning. Each hit names the note, the heading and the line range, so you can `view` just that part: view {path, lines}. Supports \"phrases\", #tags and -exclusions in the query. Example: {\"query\": \"rope undo\", \"filters\": {\"folder\": \"Projects/\", \"tags\": [\"cs\"]}}. Results are bounded by max_tokens; pass next_cursor back as cursor for more.",
        schema!({"type": "object", "properties": {
                "query": {"type": "string", "description": "What to look for."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
                "filters": {"type": "object", "properties": {
                    "folder": {"type": "string", "description": "Folder prefix, e.g. \"Projects/\"."},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "Every tag must be on the note."},
                    "where": {"type": "string", "description": "A property filter, e.g. \"status != done\"."},
                    "modified_after": {"type": "string", "description": "RFC 3339, YYYY-MM-DD or 7d."},
                    "modified_before": {"type": "string"},
                    "kind": {"type": "string", "enum": ["notes", "attachments", "all"], "default": "notes"}}},
                "max_tokens": {"type": "integer", "default": 2000},
                "cursor": {"type": "string"}}, "required": ["query"]}),
        schema!({"type": "object", "properties": {
                "hits": {"type": "array", "items": {"type": "object", "properties": {
                    "uri": {"type": "string"}, "path": {"type": "string"}, "title": {"type": "string"}, "heading": {"type": ["string", "null"]},
                    "lines": {"type": "array", "items": {"type": "integer"}}, "snippet": {"type": "string"}, "score": {"type": "number"},
                    "by_meaning": {"type": "boolean"}, "modified": {"type": ["string", "null"]}, "version": {"type": "string"},
                    "last_actor": {"type": "string"}, "trust": {"type": "string", "enum": ["trusted", "untrusted"]}}}},
                "next_cursor": {"type": ["string", "null"]}, "truncated": {"type": "boolean"},
                "indexing": {"type": "object", "description": "Present while PDFs are still being read: PDF pages may be missing.",
                    "properties": {"done": {"type": "integer"}, "total": {"type": "integer"}}}}, "required": ["hits"]}),
        r,
        Access::Read,
        search,
    )
}

fn search_filters(cx: &ToolCx<'_>, a: &Value) -> SearchFilters {
    let f = a.get("filters").cloned().unwrap_or_default();
    let now = cx.agent.now_ms();
    SearchFilters {
        folder: opt(&f, "folder"),
        tags: list(&f, "tags").into_iter().map(|t| t.trim_start_matches('#').to_string()).collect(),
        where_: opt(&f, "where"),
        modified_after: opt(&f, "modified_after").and_then(|t| parse_time(&t, now)),
        modified_before: opt(&f, "modified_before").and_then(|t| parse_time(&t, now)),
        kind: match s(&f, "kind").as_str() {
            "attachments" => SearchKind::Attachments,
            "all" => SearchKind::All,
            _ => SearchKind::Notes,
        },
    }
}

fn search(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let query = match req(a, "query") {
        Ok(q) if !q.trim().is_empty() => q,
        Ok(_) => return ToolOutput::error("invalid_args", "`query` is empty."),
        Err(e) => return e,
    };
    let limit = int(a, "limit", 10, 1, 50);
    let budget = bounds::max_tokens(a, 2_000);
    let filters = search_filters(cx, a);
    if let Err(e) = filters.validate() {
        return ToolOutput::error("invalid_args", format!("`filters.where` isn't valid: {e}"));
    }
    let fingerprint = format!("q:{}", ContentHash::of(&format!("{query}\u{1}{filters:?}\u{1}{limit}")).hex());
    let skip = match bounds::start(a, &fingerprint) {
        Start::Beginning => 0,
        Start::At(n) => n,
        Start::Stale => return stale(&fingerprint),
    };
    // Only notes the caller may see compete for the `limit` places. (Made before the
    // search: the closure runs under the index's read lock.)
    let vis = std::sync::Arc::clone(cx.visibility());
    let mut hits = cx.index.search_sections_where(&query, &filters, limit, &|p| vis.allows(p));
    // A second guard: whatever the index returns, nothing hidden leaves.
    hits.retain(|h| visible(cx, &h.path));
    let ix = cx.index.read();
    let mut rows = Vec::new();
    for h in hits.iter().skip(skip) {
        let src = ix.source(&h.path).unwrap_or_default().to_string();
        let (untrusted, origin) = trust(cx, &h.path, &src);
        let title = ix.get(&h.path).map(|r| r.title.clone()).unwrap_or_default();
        let sec = h.heading.as_deref().map(|x| format!(" › {x}")).unwrap_or_default();
        let how = if h.by_meaning { " (by meaning)" } else { "" };
        let snippet = if untrusted { fence_untrusted(&h.path, origin.as_deref(), &h.snippet) } else { h.snippet.clone() };
        let line = if h.page.is_some() {
            // A page of a PDF or an image: "paper.pdf p. 3".
            format!("- {}{how}\n  {}", h.place(), snippet.replace('\n', "\n  "))
        } else {
            format!("- {}{sec} (lines {}–{}){how}\n  {}", h.path, h.lines.0, h.lines.1, snippet.replace('\n', "\n  "))
        };
        let st = json!({
            "uri": note_uri(&h.path), "path": h.path, "title": title, "heading": h.heading, "lines": [h.lines.0, h.lines.1],
            "snippet": h.snippet, "score": h.score, "by_meaning": h.by_meaning, "modified": modified_ms(cx, &ix, &h.path).map(iso),
            "version": ContentHash::of(&src), "last_actor": last_actor(cx, &h.path),
            "trust": if untrusted { "untrusted" } else { "trusted" },
        });
        let mut st = st;
        if let Some(p) = h.page {
            st["page"] = json!(p);
        }
        rows.push((line, st, h.path.clone()));
    }
    let n = bounds::fit(rows.iter().map(|r| r.0.as_str()), budget);
    let truncated = n < rows.len();
    let next = truncated.then(|| Cursor::at(skip + n, fingerprint.clone()).encode());
    let shown = &rows[..n];
    let mut text =
        if shown.is_empty() { "No matching notes.".to_string() } else { shown.iter().map(|r| r.0.as_str()).collect::<Vec<_>>().join("\n") };
    if let Some(c) = &next {
        write!(text, "\n… more results: call search again with cursor=\"{c}\"").expect("a String takes any text");
    }
    let links = shown.iter().map(|r| note_link(&r.2)).collect();
    let mut st = json!({"hits": shown.iter().map(|r| r.1.clone()).collect::<Vec<_>>(), "next_cursor": next, "truncated": truncated});
    // Still reading PDFs at start-up: their pages may be missing yet.
    if let Some((done, total)) = ext.pdf_indexing.filter(|_| filters.kind != SearchKind::Notes) {
        st["indexing"] = json!({"done": done, "total": total});
        let of = if total == 0 { String::new() } else { format!(" ({done} of {total} read)") };
        write!(text, "\n(Quire is still reading the vault's PDFs{of}: PDF pages may be missing. Search again in a moment.)")
            .expect("a String takes any text");
    }
    ToolOutput::ok(text, st).with_links(links)
}
