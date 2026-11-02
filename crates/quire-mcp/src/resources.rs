// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Resources, resource templates and prompts (design §4.3).
//!
//! * `quire://note/{+path}` and `quire://note/{+path}{?section,lines}`: a note
//!   (or part of it) as Markdown, with its version in `_meta`.
//! * `quire://vault/instructions`: the approved `AGENTS.md` / `CLAUDE.md`.
//! * `quire://vault/index`: `wiki/index.md`, or a generated listing.
//! * `quire://proposal/{id}`: a proposal as JSON plus a unified diff.
//! * `quire://attachment/{+path}`: the text extracted from a PDF or image.
//!
//! Paths are percent-encoded per segment. Notes in `deny` zones are never
//! listed or readable.

use std::fmt::Write as _;

use quire_agent::{Actor, AgentStore, Mode, OpKind, ProposalId};
use quire_app::index::IndexService;
use quire_app::vault::{NotePath, Vault};
use serde_json::{Value, json};

use crate::bounds::{Cursor, Start};
use crate::edit::section_lines;
use crate::instructions;
use crate::served::ServedCache;
use crate::tools::common::{decode_path, encode_path, fence_untrusted, iso, note_uri, unified};

pub const NOTES_PAGE: usize = 500;
pub const RESOURCES_TTL_MS: u64 = 10_000;
const INDEX_URI: &str = "quire://vault/index";
const INSTRUCTIONS_URI: &str = "quire://vault/instructions";

/// One built-in prompt (texts come from `quire-assistant::prompts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDef {
    pub name: String,
    pub title: String,
    pub description: String,
    /// (name, description, required)
    pub args: Vec<(String, String, bool)>,
    /// The text, with `{arg}` placeholders.
    pub text: String,
}

/// What a resource read needs.
pub struct ReadCx<'a> {
    pub vault: &'a Vault,
    pub index: &'a IndexService,
    pub agent: &'a AgentStore,
    pub actor: &'a Actor,
    pub served: &'a ServedCache,
    pub state_dir: Option<&'a std::path::Path>,
}

impl ReadCx<'_> {
    fn visible(&self, p: &NotePath) -> bool {
        self.agent.effective_mode(self.actor, p, OpKind::Read) != Mode::Deny
    }
}

/// Why a read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    NotFound,
    BadRequest(String),
}

pub fn templates() -> Value {
    json!([
        {"uriTemplate": "quire://note/{+path}", "name": "note", "title": "A note", "description": "A note's Markdown. The version is in _meta.version.", "mimeType": "text/markdown"},
        {"uriTemplate": "quire://note/{+path}{?section,lines}", "name": "note-part", "title": "Part of a note", "description": "A section (heading or ^block id) or a line range (\"10-40\") of a note.", "mimeType": "text/markdown"},
        {"uriTemplate": "quire://proposal/{id}", "name": "proposal", "title": "A suggestion", "description": "A pending or decided suggestion: JSON summary and a unified diff.", "mimeType": "application/json"},
        {"uriTemplate": "quire://attachment/{+path}", "name": "attachment-text", "title": "Text of an attachment", "description": "Text extracted from a PDF or image, with page markers.", "mimeType": "text/plain"},
    ])
}

/// `resources/list`: the fixed resources, then notes 500 at a time.
pub fn list(cx: &ReadCx<'_>, params: &Value) -> Result<Value, ReadError> {
    let ix = cx.index.read();
    let paths: Vec<NotePath> = ix.paths().into_iter().filter(|p| cx.visible(p)).collect();
    let fingerprint = format!("r:{}", paths.len());
    let start = match params.get("cursor").and_then(Value::as_str) {
        None => 0,
        Some(c) => match crate::bounds::start(&json!({ "cursor": c }), &fingerprint) {
            Start::At(n) => n,
            Start::Beginning => 0,
            Start::Stale => return Err(ReadError::BadRequest("The cursor is stale; list again from the start.".into())),
        },
    };
    let mut out = Vec::new();
    if start == 0 {
        if instructions::find(cx.vault).is_some() {
            out.push(json!({"uri": INSTRUCTIONS_URI, "name": "instructions", "title": "Agent instructions (AGENTS.md)", "mimeType": "text/markdown"}));
        }
        out.push(json!({"uri": INDEX_URI, "name": "index", "title": "Vault index", "mimeType": "text/markdown"}));
    }
    for p in paths.iter().skip(start).take(NOTES_PAGE) {
        let title = ix.get(p).map_or_else(|| p.stem().to_string(), |r| r.title.clone());
        let mut r = json!({"uri": note_uri(p), "name": p.as_str(), "title": title, "mimeType": "text/markdown"});
        if let Some(ms) = cx.vault.storage().modified(p).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            r["annotations"] = json!({"lastModified": iso(i64::try_from(ms.as_millis()).unwrap_or(0))});
        }
        out.push(r);
    }
    let mut v = json!({ "resources": out });
    if start + NOTES_PAGE < paths.len() {
        v["nextCursor"] = Cursor { offset: start + NOTES_PAGE, version: fingerprint }.encode().into();
    }
    Ok(v)
}

/// A note URI's parts: path, `section`, `lines`.
type NoteUri = (NotePath, Option<String>, Option<(usize, usize)>);

/// The note path and query of a `quire://note/…` URI.
fn parse_note_uri(uri: &str) -> Option<NoteUri> {
    let rest = uri.strip_prefix("quire://note/")?;
    let (path, query) = rest.split_once('?').map_or((rest, ""), |(p, q)| (p, q));
    let path = NotePath::new(decode_path(path)?);
    let mut section = None;
    let mut lines = None;
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = decode_path(&v.replace('+', " "))?;
        match k {
            "section" => section = Some(v),
            "lines" => {
                let (a, b) = v.split_once(['-', ',']).unwrap_or((&v, &v));
                lines = Some((a.trim().parse().ok()?, b.trim().parse().ok()?));
            }
            _ => {}
        }
    }
    Some((path, section, lines))
}

/// `resources/read`.
pub fn read(cx: &ReadCx<'_>, uri: &str) -> Result<Value, ReadError> {
    if uri == INSTRUCTIONS_URI {
        let (path, approved, text) = instructions::for_agents(cx.vault, cx.state_dir, usize::MAX).ok_or(ReadError::NotFound)?;
        return Ok(
            json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": text, "_meta": {"path": path, "approved": approved}}]}),
        );
    }
    if uri == INDEX_URI {
        return Ok(json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": vault_index(cx)}]}));
    }
    if let Some(id) = uri.strip_prefix("quire://proposal/") {
        return proposal(cx, uri, id);
    }
    if let Some(p) = uri.strip_prefix("quire://attachment/") {
        let path = NotePath::new(decode_path(p).ok_or(ReadError::NotFound)?);
        if !cx.visible(&path) {
            return Err(ReadError::NotFound);
        }
        let ix = cx.index.read();
        let a = ix.attachment(&path).ok_or(ReadError::NotFound)?;
        let mut text = String::new();
        for (i, page) in a.pages.iter().enumerate() {
            let _ = write!(text, "--- page {} ---\n{}\n", i + 1, page.trim_end());
        }
        return Ok(json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": text}]}));
    }
    let (path, section, lines) = parse_note_uri(uri).ok_or(ReadError::NotFound)?;
    if !cx.visible(&path) || !path.is_markdown() {
        return Err(ReadError::NotFound);
    }
    let full = cx.vault.read(&path).map_err(|_| ReadError::NotFound)?;
    let version = cx.served.store(&full);
    let total = full.lines().count();
    let range = match (section, lines) {
        (Some(s), _) => Some(section_lines(&full, &s).map_err(|e| ReadError::BadRequest(e.message(path.as_str())))?),
        (None, Some((a, b))) if a >= 1 && b >= a => Some((a, b.min(total))),
        (None, Some(_)) => return Err(ReadError::BadRequest("lines must be first-last, 1-based".into())),
        (None, None) => None,
    };
    let body = range.map_or_else(
        || full.clone(),
        |(a, b)| {
            full.lines().skip(a - 1).take(b + 1 - a.min(b + 1)).fold(String::new(), |mut s, l| {
                s.push_str(l);
                s.push('\n');
                s
            })
        },
    );
    let tool_cx = crate::tools::ToolCx {
        vault: cx.vault,
        index: cx.index,
        agent: cx.agent,
        actor: cx.actor,
        cancel: &quire_app::intel::chat::CancelFlag::new(),
        allow_write: false,
    };
    let (untrusted, origin) = crate::tools::common::trust(&tool_cx, &path, &full);
    let text = if untrusted { fence_untrusted(&path, origin.as_deref(), &body) } else { body };
    let mut meta =
        json!({"version": version, "path": path, "trust": if untrusted { "untrusted" } else { "trusted" }, "total_lines": total});
    if let Some((a, b)) = range {
        meta["lines"] = json!([a, b]);
    }
    let mut item = json!({"uri": uri, "mimeType": "text/markdown", "text": text, "_meta": meta});
    if let Some(ms) = cx.vault.storage().modified(&path).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
        item["annotations"] = json!({"lastModified": iso(i64::try_from(ms.as_millis()).unwrap_or(0))});
    }
    Ok(json!({ "contents": [item] }))
}

/// `wiki/index.md` if there is one, else a listing by folder.
fn vault_index(cx: &ReadCx<'_>) -> String {
    let wiki = NotePath::new("wiki/index.md");
    if cx.visible(&wiki)
        && let Ok(t) = cx.vault.read(&wiki)
    {
        return t;
    }
    let ix = cx.index.read();
    let mut out = String::from("# Vault index\n\nGenerated by Quire (there is no wiki/index.md).\n");
    let mut folder = None;
    for p in ix.paths().iter().filter(|p| cx.visible(p)) {
        if folder.as_deref() != Some(p.folder()) {
            folder = Some(p.folder().to_string());
            let _ = write!(out, "\n## {}\n\n", if p.folder().is_empty() { "(top level)" } else { p.folder() });
        }
        let title = ix.get(p).map_or_else(|| p.stem().to_string(), |r| r.title.clone());
        let _ = writeln!(out, "- [[{}]] — {title}", p.without_ext());
    }
    out
}

fn proposal(cx: &ReadCx<'_>, uri: &str, id: &str) -> Result<Value, ReadError> {
    let id = ProposalId::parse(id).ok_or(ReadError::NotFound)?;
    let p = cx.agent.proposal(&id).ok_or(ReadError::NotFound)?;
    let sum = p.summary();
    if sum.files.iter().any(|f| !cx.visible(f)) {
        return Err(ReadError::NotFound);
    }
    let mut diff = String::new();
    for ch in &p.changes {
        match (ch.path.as_ref(), ch.base_text(), ch.new_text.as_ref(), ch.text.as_ref()) {
            (Some(path), Some(base), Some(new), _) => diff.push_str(&unified(&base, new, path.as_str())),
            (Some(path), _, _, Some(text)) => diff.push_str(&unified("", text, path.as_str())),
            _ => {
                let _ = writeln!(diff, "{}", serde_json::to_string(ch).unwrap_or_default());
            }
        }
    }
    let summary = json!({"id": sum.id, "status": sum.status, "title": sum.title, "actor": sum.actor.id, "run": sum.run, "files": sum.files,
        "created": iso(sum.created), "updated": iso(sum.updated), "rationale": p.rationale, "decided_hunks": sum.decided_hunks});
    Ok(json!({"contents": [
        {"uri": uri, "mimeType": "application/json", "text": summary.to_string()},
        {"uri": format!("{uri}#diff"), "mimeType": "text/x-diff", "text": diff},
    ]}))
}

/// `prompts/list`.
pub fn prompts_list(prompts: &[PromptDef]) -> Value {
    let list: Vec<Value> = prompts
        .iter()
        .map(|p| {
            let args: Vec<Value> = p.args.iter().map(|(n, d, r)| json!({"name": n, "description": d, "required": r})).collect();
            json!({"name": p.name, "title": p.title, "description": p.description, "arguments": args})
        })
        .collect();
    json!({ "prompts": list })
}

/// `prompts/get`.
pub fn prompts_get(prompts: &[PromptDef], params: &Value) -> Result<Value, ReadError> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let p = prompts.iter().find(|p| p.name == name).ok_or(ReadError::NotFound)?;
    let args = params.get("arguments").cloned().unwrap_or_default();
    let mut text = p.text.clone();
    for (n, _, required) in &p.args {
        let v = args.get(n).and_then(Value::as_str);
        if *required && v.is_none() {
            return Err(ReadError::BadRequest(format!("missing argument {n}")));
        }
        text = text.replace(&format!("{{{n}}}"), v.unwrap_or(""));
    }
    Ok(json!({"description": p.description, "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
}

/// Paths in URIs, for tests and the server's change notifications.
pub fn uri_for_path(p: &NotePath) -> String {
    if p.is_markdown() { note_uri(p) } else { format!("quire://attachment/{}", encode_path(p.as_str())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_uris_carry_section_and_lines() {
        let (p, s, l) = parse_note_uri("quire://note/Projects/Rope%20Notes.md?section=%23%23%20Undo&lines=2-4").unwrap();
        assert_eq!((p.as_str(), s.as_deref(), l), ("Projects/Rope Notes.md", Some("## Undo"), Some((2, 4))));
        assert_eq!(parse_note_uri("quire://note/A.md").unwrap().0.as_str(), "A.md");
        assert!(parse_note_uri("quire://other/A.md").is_none());
        assert!(parse_note_uri("quire://note/A.md?lines=x-y").is_none());
    }

    #[test]
    fn prompts_fill_arguments() {
        let p = vec![PromptDef {
            name: "quire_ingest".into(),
            title: "Ingest".into(),
            description: "d".into(),
            args: vec![("source".into(), "what".into(), true)],
            text: "Ingest {source}.".into(),
        }];
        assert_eq!(prompts_list(&p)["prompts"][0]["arguments"][0]["required"], true);
        let got = prompts_get(&p, &json!({"name": "quire_ingest", "arguments": {"source": "raw/a.md"}})).unwrap();
        assert_eq!(got["messages"][0]["content"]["text"], "Ingest raw/a.md.");
        assert!(matches!(prompts_get(&p, &json!({"name": "quire_ingest"})), Err(ReadError::BadRequest(_))));
        assert_eq!(prompts_get(&p, &json!({"name": "nope"})), Err(ReadError::NotFound));
    }
}
