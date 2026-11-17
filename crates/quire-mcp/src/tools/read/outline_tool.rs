// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `outline`: a note's headings.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "outline",
        "Outline of a note",
        "The headings of a note with their line ranges and block ids, to choose a section to `view` or `edit_section`. Example: {\"path\": \"Rope Notes.md\"}.",
        schema!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        schema!({"type": "object", "properties": {
                "headings": {"type": "array", "items": {"type": "object", "properties": {
                    "level": {"type": "integer"}, "text": {"type": "string"}, "line": {"type": "integer"}, "end_line": {"type": "integer"},
                    "block_ids": {"type": "array", "items": {"type": "string"}}}}},
                "version": {"type": "string"}}, "required": ["headings", "version"]}),
        r,
        Access::Read,
        outline_tool,
    )
}

fn outline_tool(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let path = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let text = cx.vault.read(&path).unwrap_or_default();
    let version = ext.served.store(&text);
    let total = line_count(&text);
    let line_of = |b: usize| text[..b.min(text.len())].matches('\n').count() + 1;
    let heads = outline(&text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = format!("# Outline: {path} ({total} lines, version {version})\n");
    let mut rows = Vec::new();
    for (i, h) in heads.iter().enumerate() {
        let line = line_of(h.offset);
        let end = heads[i + 1..].iter().find(|n| n.level <= h.level).map_or(total, |n| line_of(n.offset) - 1).max(line);
        let next_any = heads.get(i + 1).map_or(total, |n| line_of(n.offset) - 1);
        let ids: Vec<String> = lines
            .iter()
            .take(next_any)
            .skip(line - 1)
            .filter_map(|l| {
                let t = l.trim_end();
                let id = t.rsplit_once(" ^").map(|(_, id)| id)?;
                (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| format!("^{id}"))
            })
            .collect();
        let _ = writeln!(
            out,
            "{}{} {} (lines {line}–{end}){}",
            "  ".repeat(usize::from(h.level.saturating_sub(1))),
            "#".repeat(usize::from(h.level)),
            h.text,
            if ids.is_empty() { String::new() } else { format!(" {}", ids.join(" ")) }
        );
        rows.push(json!({"level": h.level, "text": h.text, "line": line, "end_line": end, "block_ids": ids}));
    }
    if heads.is_empty() {
        out.push_str("(no headings)\n");
    }
    ToolOutput::ok(out, json!({"headings": rows, "version": version})).with_links(vec![note_link(&path)])
}
