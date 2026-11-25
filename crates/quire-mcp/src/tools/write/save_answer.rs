// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `save_answer`, which saves an answer as a note, with its sources.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "save_answer",
        "Save an answer",
        "File an answer you gave as a note, with citations as footnotes that Quire checks when the cited notes change. Use the citations from `ask` (or your own: {path, quote}). Goes to wiki/answers/ unless you give folder. Example: {\"question\": \"Why is undo cheap?\", \"answer_markdown\": \"Ropes share subtrees.[^1]\", \"citations\": [{\"path\": \"Rope Notes.md\", \"quote\": \"Ropes make undo cheap\"}]}.",
        json!({"type": "object", "properties": props(schema!({
                "question": {"type": "string"}, "answer_markdown": {"type": "string", "description": "Use [^1], [^2]… for the citations, in order."},
                "citations": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "quote": {"type": "string"}}}},
                "folder": {"type": "string"}})), "required": ["question", "answer_markdown"]}),
        o(),
        Annotations::CREATE,
        Access::Write,
        save_answer,
    )
}

fn save_answer(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let (question, answer, run) = match (req(a, "question"), req(a, "answer_markdown"), run_arg(a)) {
        (Ok(q), Ok(an), Ok(r)) => (q, an, r),
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return e,
    };
    let citations = a.get("citations").and_then(Value::as_array).cloned().unwrap_or_default();
    let (notes, sources, links) = match footnotes(cx, &citations) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let policy = cx.agent.policy();
    let default_folder =
        if policy.zones.iter().any(|z| z.path.trim_end_matches('/').eq_ignore_ascii_case("wiki")) { "wiki/answers" } else { "Answers" };
    let folder = opt(a, "folder").unwrap_or_else(|| default_folder.into()).trim_matches('/').to_string();
    let mut name = sanitize_title(&question);
    name.truncate(name.char_indices().nth(80).map_or(name.len(), |(i, _)| i));
    if name.is_empty() {
        name = "Answer".into();
    }
    let path = match cx.vault.unique_path(&folder, &name) {
        Ok(p) => p,
        Err(e) => return ToolOutput::error("invalid_args", e.to_string()),
    };
    let today = quire_base::dates::today_ymd();
    let by = cx.actor.model.clone().unwrap_or_else(|| cx.actor.label.clone());
    let q = quire_app::markdown::frontmatter::yaml_scalar(&question);
    let src_list = if links.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", links.iter().map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(", "))
    };
    let mut text = format!(
        "---\ntype: answer\nquestion: {q}\nasked: {today}\nanswered_by: {}\nai_written: true\nsources: {src_list}\n---\n\n> [!ai] Written by {by} from {} note{} on {today}\n\n{}\n",
        quire_app::markdown::frontmatter::yaml_scalar(&cx.actor.id),
        citations.len(),
        if citations.len() == 1 { "" } else { "s" },
        answer.trim_end()
    );
    if !notes.is_empty() {
        text.push('\n');
        text.push_str(&notes);
    }
    let diff = unified("", &text, path.as_str());
    let cs = ChangeSet {
        changes: vec![Change::Create { path: path.clone(), text }],
        run,
        title: format!("Save answer: {question}"),
        sources,
        ..ChangeSet::default()
    };
    submit(cx, ext, a, "save_answer", &path, cs, &diff, &ContentHash::of(""), json!({"path": path}))
}

/// Footnotes for `save_answer`: each citation as `[[Note#Heading]] — “quote”`
/// with a `quire:cite` comment, and the sources they name.
fn footnotes(cx: &ToolCx<'_>, citations: &[Value]) -> Result<(String, Vec<SourceRef>, Vec<String>), ToolOutput> {
    let mut out = String::new();
    let mut sources = Vec::new();
    let mut links = Vec::new();
    for (i, c) in citations.iter().enumerate() {
        let path = path_arg(c, "path")?;
        if !visible(cx, &path) || !cx.vault.exists(&path) {
            return Err(ToolOutput::error("not_found", format!("Citation {} names {path}, which doesn't exist.", i + 1)));
        }
        let text = cx.vault.read(&path).unwrap_or_default();
        let quote = s(c, "quote");
        let anchor = match c.get("version").and_then(Value::as_str) {
            // A full anchor from `ask`: keep it.
            Some(_) => serde_json::from_value::<quire_app::intel::anchor::Anchor>(c.clone()).ok(),
            None => None,
        }
        .or_else(|| {
            let at = text.find(quote.trim()).filter(|_| !quote.trim().is_empty())?;
            Some(quire_app::intel::anchor::make_anchor(&path, &text, at..at + quote.trim().len()))
        });
        let Some(anchor) = anchor else {
            return Err(ToolOutput::error(
                "no_match",
                format!("Citation {}: the quote isn't in {path}. Copy it exactly from view.", i + 1),
            ));
        };
        let target = anchor.heading.as_deref().map_or_else(|| path.stem().to_string(), |h| format!("{}#{h}", path.stem()));
        let lines = format!("{}-{}", anchor.lines.0, anchor.lines.1);
        let hex = anchor.version.hex().to_string();
        let args = comment::format_args(&[("v", "1"), ("sha", &hex), ("l", &lines), ("p", &anchor.prefix), ("s", &anchor.suffix)]);
        writeln!(out, "[^{}]: [[{target}]] — “{}” {}", i + 1, anchor.quote.replace('\n', " "), comment::format("cite", &args))
            .expect("a String takes any text");
        links.push(format!("[[{target}]]"));
        let mut src = SourceRef::uri(note_uri(&path));
        src.anchor = Some(anchor);
        sources.push(src);
    }
    Ok((out, sources, links))
}
