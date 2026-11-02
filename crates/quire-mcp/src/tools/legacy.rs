// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The tools of Quire 0.8, moved here unchanged.
//!
//! Names, schemas, texts and behaviour are 0.8's. `--allow-write` still
//! writes directly (WS-B moves writes to the policy pipeline, design §B.4),
//! now with provenance and a ledger entry for each write, and never under
//! `.quire/`.

use std::fmt::Write as _;

use quire_agent::{LedgerEntry, Op, Provenance};
use quire_app::find::{FindOptions, compile, replace_in_string};
use quire_app::intel::{extractive_answer, retrieve};
use quire_app::vault::{ContentHash, NotePath};
use serde_json::{Value, json};

use super::{Access, Annotations, FnTool, Tool, ToolCx, ToolOutput, ToolSpec, schema};

const READ_ONLY: &str = "This vault is read-only for AI tools. Start the server with --allow-write to permit edits.";

fn spec(name: &'static str, title: &'static str, description: &'static str, input_schema: Value, access: Access) -> ToolSpec {
    let annotations = if access == Access::Write { Annotations::WRITE } else { Annotations::READ };
    ToolSpec { name, title, description, input_schema, output_schema: None, annotations, access }
}

fn tool(spec: ToolSpec, run: fn(&ToolCx<'_>, &Value) -> ToolOutput) -> Box<dyn Tool> {
    Box::new(FnTool { spec, run })
}

/// The 0.8 tools, in 0.8's order.
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        tool(
            spec(
                "search_notes",
                "Search notes",
                "Search the user's notes by keywords and meaning. Supports \"phrases\", #tag filters and -exclusions. Returns note paths, sections and snippets.",
                schema(&json!({"query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 50}}), &["query"]),
                Access::Read,
            ),
            search_notes,
        ),
        tool(
            spec(
                "read_note",
                "Read a note",
                "Read a note's full Markdown by path (e.g. \"Projects/Plan.md\") or by title.",
                schema(&json!({"note": {"type": "string"}}), &["note"]),
                Access::Read,
            ),
            read_note,
        ),
        tool(
            spec(
                "list_notes",
                "List notes",
                "List notes, optionally only in a folder or with a tag.",
                schema(&json!({"folder": {"type": "string"}, "tag": {"type": "string"}, "limit": {"type": "integer"}}), &[]),
                Access::Read,
            ),
            list_notes,
        ),
        tool(
            spec(
                "backlinks",
                "Backlinks",
                "Notes that link to a note, with the linking line.",
                schema(&json!({"note": {"type": "string"}}), &["note"]),
                Access::Read,
            ),
            backlinks,
        ),
        tool(
            spec(
                "ask_notes",
                "Ask your notes",
                "Retrieve the passages most relevant to a question, with citations like [[Note#Heading]] and a short extractive answer. Use it to ground your own answer in the user's notes.",
                schema(&json!({"question": {"type": "string"}}), &["question"]),
                Access::Read,
            ),
            ask_notes,
        ),
        tool(spec("list_tags", "List tags", "All tags with note counts.", schema(&json!({}), &[]), Access::Read), list_tags),
        tool(
            spec(
                "create_note",
                "Create a note",
                "Create a new note. Never overwrites: a unique name is chosen.",
                schema(
                    &json!({"title": {"type": "string"}, "content": {"type": "string"}, "folder": {"type": "string"}}),
                    &["title", "content"],
                ),
                Access::Write,
            ),
            create_note,
        ),
        tool(
            spec(
                "append_to_note",
                "Append to a note",
                "Append Markdown to the end of an existing note.",
                schema(&json!({"note": {"type": "string"}, "text": {"type": "string"}}), &["note", "text"]),
                Access::Write,
            ),
            append_to_note,
        ),
        tool(
            spec(
                "replace_in_note",
                "Replace in a note",
                "Find and replace text in one note. Set regex=true for regular expressions with $1 captures.",
                schema(
                    &json!({"note": {"type": "string"}, "find": {"type": "string"}, "replace": {"type": "string"}, "regex": {"type": "boolean"}}),
                    &["note", "find", "replace"],
                ),
                Access::Write,
            ),
            replace_in_note,
        ),
    ]
}

fn s(a: &Value, k: &str) -> String {
    a.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn resolve(cx: &ToolCx<'_>, note: &str) -> Result<NotePath, String> {
    let ix = cx.index.read();
    let p = NotePath::new(note);
    if ix.get(&p).is_some() {
        return Ok(p);
    }
    ix.resolve(note.trim_end_matches(".md"), None)
        .ok_or_else(|| format!("No note matches “{note}”. Use search_notes or list_notes to find it."))
}

fn search_notes(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 50) as usize;
    let hits = cx.index.hybrid_search(&s(a, "query"), limit);
    if hits.is_empty() {
        return ToolOutput::text("No matching notes.");
    }
    let out: Vec<String> = hits
        .iter()
        .map(|h| {
            let sec = h.heading.as_deref().map(|x| format!(" › {x}")).unwrap_or_default();
            let how = if h.by_meaning { " (by meaning)" } else { "" };
            format!("- {}{sec}{how}\n  {}", h.path, h.snippet)
        })
        .collect();
    ToolOutput::text(out.join("\n"))
}

fn read_note(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    match resolve(cx, &s(a, "note")) {
        Ok(p) => match cx.vault.read(&p) {
            Ok(src) => ToolOutput::text(format!("# File: {p}\n\n{src}")),
            Err(e) => ToolOutput::error("io", e.to_string()),
        },
        Err(e) => ToolOutput::error("not_found", e),
    }
}

fn list_notes(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    let ix = cx.index.read();
    let (folder, tag) = (s(a, "folder"), s(a, "tag"));
    let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
    let list: Vec<String> = ix
        .paths()
        .into_iter()
        .filter(|p| folder.is_empty() || p.folder().eq_ignore_ascii_case(folder.trim_matches('/')))
        .filter(|p| tag.is_empty() || ix.get(p).is_some_and(|r| r.has_tag(&tag)))
        .take(limit)
        .map(|p| format!("- {p}"))
        .collect();
    ToolOutput::text(if list.is_empty() { "No notes.".into() } else { list.join("\n") })
}

fn backlinks(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    match resolve(cx, &s(a, "note")) {
        Ok(p) => {
            let ix = cx.index.read();
            let b: Vec<String> = ix.backlinks(&p).iter().map(|b| format!("- {}: {}", b.from, b.context)).collect();
            ToolOutput::text(if b.is_empty() { format!("Nothing links to {p}.") } else { b.join("\n") })
        }
        Err(e) => ToolOutput::error("not_found", e),
    }
}

fn ask_notes(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    let q = s(a, "question");
    let sem = cx.index.semantic();
    let ix = cx.index.read();
    let sources = retrieve(&ix, sem.as_deref(), &q, 6);
    let mut out = extractive_answer(&q, &sources, sem.as_deref());
    out.push_str("\n\nSources:\n");
    for src in &sources {
        let body = src.text.split_once('\n').map_or(src.text.as_str(), |(_, b)| b);
        let _ = write!(out, "\n[{}] {} ({})\n{}\n", src.n, src.citation(), src.path, body.chars().take(1500).collect::<String>());
    }
    ToolOutput::text(out)
}

fn list_tags(cx: &ToolCx<'_>, _a: &Value) -> ToolOutput {
    let tags: Vec<String> = cx.index.read().tags().into_iter().map(|(t, n)| format!("#{t} ({n})")).collect();
    ToolOutput::text(if tags.is_empty() { "No tags.".into() } else { tags.join(", ") })
}

/// Write `content` as the caller: the old text and the new one are kept in
/// version history (the new one with provenance), and the ledger records it.
fn write_note(cx: &ToolCx<'_>, path: &NotePath, content: &str, op: Op) -> Result<(), String> {
    if quire_agent::policy::hard_rule(path.as_str(), &cx.vault.config.templates_folder) == Some(quire_agent::policy::HardRule::AppData) {
        return Err(format!("{path} is Quire's own data and can't be written by AI tools."));
    }
    let now = cx.agent.now_ms();
    let old = cx.vault.read(path).ok();
    if let Some(old) = &old {
        let _ = cx.vault.snapshot(path, old, now - 1, true);
    }
    let snap_before = old.as_ref().and_then(|_| cx.vault.history(path).first().map(|s| s.at_ms));
    cx.vault.write(path, content).map_err(|e| e.to_string())?;
    let prov = Provenance::new(cx.actor.clone(), op, content, now);
    let _ = cx.vault.snapshot_with(path, content, now, true, Some(&prov.to_json()));
    let mut e = LedgerEntry::new(cx.actor.clone(), op, now).path(path);
    e.before = old.as_deref().map(ContentHash::of);
    e.after = Some(ContentHash::of(content));
    e.snap_before = snap_before;
    e.snap_after = cx.vault.history(path).first().map(|s| s.at_ms);
    e.bytes = Some(content.len() as u64);
    cx.agent.record(&e);
    cx.index.upsert(path.clone(), content.to_string());
    cx.index.flush();
    Ok(())
}

fn create_note(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    if !cx.allow_write {
        return ToolOutput::error("denied_by_policy", READ_ONLY);
    }
    match cx.vault.unique_path(&s(a, "folder"), &s(a, "title")) {
        Ok(p) => match write_note(cx, &p, &s(a, "content"), Op::Create) {
            Ok(()) => ToolOutput::text(format!("Created {p}")),
            Err(e) => ToolOutput::error("io", e),
        },
        Err(e) => ToolOutput::error("invalid_args", e.to_string()),
    }
}

fn append_to_note(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    if !cx.allow_write {
        return ToolOutput::error("denied_by_policy", READ_ONLY);
    }
    match resolve(cx, &s(a, "note")) {
        Ok(p) => {
            let mut src = cx.vault.read(&p).unwrap_or_default();
            if !src.is_empty() && !src.ends_with('\n') {
                src.push('\n');
            }
            src.push_str(&s(a, "text"));
            if !src.ends_with('\n') {
                src.push('\n');
            }
            match write_note(cx, &p, &src, Op::Write) {
                Ok(()) => ToolOutput::text(format!("Appended to {p}")),
                Err(e) => ToolOutput::error("io", e),
            }
        }
        Err(e) => ToolOutput::error("not_found", e),
    }
}

fn replace_in_note(cx: &ToolCx<'_>, a: &Value) -> ToolOutput {
    if !cx.allow_write {
        return ToolOutput::error("denied_by_policy", READ_ONLY);
    }
    let opts = FindOptions { regex: a.get("regex").and_then(Value::as_bool).unwrap_or(false), ..Default::default() };
    let re = match compile(&s(a, "find"), opts) {
        Ok(r) => r,
        Err(e) => return ToolOutput::error("invalid_args", e),
    };
    match resolve(cx, &s(a, "note")) {
        Ok(p) => {
            let src = cx.vault.read(&p).unwrap_or_default();
            let (out, n) = replace_in_string(&src, &re, &s(a, "replace"), opts);
            if n == 0 {
                return ToolOutput::text(format!("No matches in {p}."));
            }
            match write_note(cx, &p, &out, Op::Write) {
                Ok(()) => ToolOutput::text(format!("Replaced {n} match{} in {p}", if n == 1 { "" } else { "es" })),
                Err(e) => ToolOutput::error("io", e),
            }
        }
        Err(e) => ToolOutput::error("not_found", e),
    }
}
