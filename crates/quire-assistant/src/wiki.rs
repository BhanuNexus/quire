// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Ingest into wiki…" (design §D.7).
//!
//! A source note becomes a summary page
//! in `wiki/sources/`, concept pages in `wiki/concepts/` (new, or a section
//! added to an existing page), suggested links from the person's own notes,
//! and an up-to-date `wiki/index.md` and `wiki/log.md`.
//!
//! The model plans and writes the bullets (each with the exact source words
//! it rests on); Quire builds the pages, cites every bullet with a
//! `quire:cite` footnote, drops bullets whose quote isn't in the source, and
//! submits two change sets under one run id: the wiki (agent-owned, written
//! directly) and the person's notes (suggestions). "Revert run" undoes both.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use quire_agent::{Actor, AgentStore, Change, ChangeSet, ProposalId, RunId, SourceRef, SubmitOutcome};
use quire_app::assistant_state::wiki as wikifiles;
use quire_app::intel::anchor::make_anchor;
use quire_app::intel::chat::{CancelFlag, ChatModel, ChatRequest, chat_text};
use quire_app::markdown::comment;
use quire_app::vault::NotePath;
use serde_json::Value;

use crate::prompts;

/// Sources longer than this (in estimated tokens) are read in parts first.
pub const LONG_SOURCE_TOKENS: usize = 12_000;
const CHUNK_BYTES: usize = 30_000;
const MAX_CONCEPTS: usize = 12;
const MAX_LINK_SUGGESTIONS: usize = 8;

/// The same estimate as MCP's token bounds: one token per three bytes.
pub fn est_tokens(s: &str) -> usize {
    s.len().div_ceil(3)
}

/// What an ingest will cost, shown before a cloud run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub calls: usize,
    pub input_tokens: usize,
    /// Parts a long source is read in (1 when it fits).
    pub parts: usize,
}

impl Estimate {
    /// "about 38k input tokens in 11 requests".
    pub fn describe(&self) -> String {
        let t = if self.input_tokens >= 1000 { format!("{}k", (self.input_tokens + 500) / 1000) } else { self.input_tokens.to_string() };
        format!("about {t} input tokens in {} requests", self.calls)
    }
}

/// Estimate for a source of this text, assuming 8 concepts.
pub fn estimate(source: &str) -> Estimate {
    let tokens = est_tokens(source);
    let parts = if tokens > LONG_SOURCE_TOKENS { source.len().div_ceil(CHUNK_BYTES) } else { 1 };
    // Long sources are condensed to their points (about a tenth) after the first pass.
    let working = if parts > 1 { tokens / 10 + 500 } else { tokens };
    let concepts = 8;
    let prompt = 400;
    let calls = (if parts > 1 { parts } else { 0 }) + 2 + concepts;
    let input = (if parts > 1 { tokens + parts * prompt } else { 0 }) + (2 + concepts) * (working + prompt);
    Estimate { calls, input_tokens: input, parts }
}

/// What happened, step by step (for the dialog and the CLI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestEvent {
    Step(String),
    Done(IngestReport),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngestReport {
    pub run: Option<RunId>,
    pub summary: Option<NotePath>,
    pub created: Vec<NotePath>,
    pub updated: Vec<NotePath>,
    /// Suggestions in the person's notes (one proposal), and the wiki's own
    /// if the vault's policy made those suggestions too.
    pub proposals: Vec<ProposalId>,
    /// Bullets dropped because their quote wasn't in the source.
    pub dropped: usize,
}

impl IngestReport {
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{} page{} created, {} updated",
            self.created.len(),
            if self.created.len() == 1 { "" } else { "s" },
            self.updated.len()
        );
        if !self.proposals.is_empty() {
            s.push_str(", suggestions waiting in the Inbox");
        }
        s
    }
}

/// One cited bullet as the model wrote it.
#[derive(Debug, Clone)]
struct Point {
    text: String,
    quote: String,
}

fn points(v: &Value) -> Vec<Point> {
    v["points"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let text = p["text"].as_str()?.trim().to_string();
            (!text.is_empty()).then(|| Point { text, quote: p["quote"].as_str().unwrap_or_default().trim().to_string() })
        })
        .collect()
}

/// The first JSON object in a reply (models sometimes wrap it in prose or a fence).
pub fn first_json(reply: &str) -> Option<Value> {
    let start = reply.find('{')?;
    let mut depth = 0i32;
    let (mut in_str, mut esc) = (false, false);
    for (i, c) in reply[start..].char_indices() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&reply[start..=start + i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn ask_json(model: &dyn ChatModel, prompt: String, cancel: &CancelFlag) -> Result<Value, String> {
    let mut req = ChatRequest::simple(prompts::INGEST_SYSTEM, prompt, 2000);
    req.json = true;
    req.temperature = 0.1;
    for _ in 0..2 {
        let (reply, _) = chat_text(model, &req, cancel).map_err(|e| e.to_string())?;
        if let Some(v) = first_json(&reply) {
            return Ok(v);
        }
    }
    Err("the model didn't answer in the expected format".into())
}

/// A file name from a title: no path separators or characters that
/// trouble file systems, at most 80 characters.
pub fn file_name(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']') { ' ' } else { c })
        .collect();
    let words = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = words.chars().take(80).collect();
    let t = t.trim_matches(['.', ' ']).to_string();
    if t.is_empty() { "Untitled".into() } else { t }
}

fn fold(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Footnotes cite `source` (its vault path without `.md`); returns the bullet
/// lines, the footnote lines, and how many points were dropped.
fn cite(points: &[Point], source_path: &NotePath, source: &str, first_note: usize) -> (Vec<String>, Vec<String>, usize) {
    let target_base = source_path.as_str().trim_end_matches(".md").to_string();
    let (mut bullets, mut notes, mut dropped) = (Vec::new(), Vec::new(), 0);
    for p in points {
        let Some(range) = locate(source, &p.quote) else {
            dropped += 1;
            continue;
        };
        let anchor = make_anchor(source_path, source, range);
        let n = first_note + notes.len();
        let target = anchor.heading.as_deref().map_or_else(|| target_base.clone(), |h| format!("{target_base}#{h}"));
        let args = comment::format_args(&[
            ("v", "1"),
            ("sha", anchor.version.hex()),
            ("l", &format!("{}-{}", anchor.lines.0, anchor.lines.1)),
            ("p", &anchor.prefix),
            ("s", &anchor.suffix),
        ]);
        let text = p.text.trim_end_matches(['.', ' ']);
        bullets.push(format!("- {text}.[^{n}]"));
        notes.push(format!("[^{n}]: [[{target}]] — “{}” {}", anchor.quote.replace('\n', " "), comment::format("cite", &args)));
    }
    (bullets, notes, dropped)
}

/// Where `quote` is in `source`: exactly, or ignoring case and runs of
/// white space. Quotes shorter than 12 characters are too vague to cite.
fn locate(source: &str, quote: &str) -> Option<std::ops::Range<usize>> {
    let q = quote.trim().trim_matches(['"', '“', '”']).trim();
    if q.chars().count() < 12 {
        return None;
    }
    if let Some(i) = source.find(q) {
        return Some(i..i + q.len());
    }
    // Normalized search: map each normalized char back to its byte offset.
    let mut norm = String::new();
    let mut map = Vec::new();
    let mut last_space = false;
    for (i, c) in source.char_indices() {
        if c.is_whitespace() {
            if !last_space {
                norm.push(' ');
                map.push(i);
            }
            last_space = true;
        } else {
            for l in c.to_lowercase() {
                norm.push(l);
                map.push(i);
            }
            last_space = false;
        }
    }
    let nq: String = q.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let at = norm.find(&nq)?;
    let start_char = norm[..at].chars().count();
    let end_char = start_char + nq.chars().count();
    let start = *map.get(start_char)?;
    let end = map.get(end_char).copied().unwrap_or(source.len());
    Some(start..end.max(start + 1).min(source.len()))
}

/// The page already in the vault for a concept: same name (folded), or an alias.
fn find_page(agent: &AgentStore, all: &[NotePath], concept: &str) -> Option<NotePath> {
    let want = fold(concept);
    if want.is_empty() {
        return None;
    }
    let by_name: Vec<&NotePath> = all.iter().filter(|p| fold(p.stem()) == want && !p.as_str().starts_with("raw/")).collect();
    // Prefer the wiki's own page.
    if let Some(p) = by_name.iter().find(|p| p.as_str().starts_with(wikifiles::WIKI)).or_else(|| by_name.first()) {
        return Some((*p).clone());
    }
    all.iter()
        .filter(|p| p.as_str().starts_with(wikifiles::WIKI))
        .find(|p| {
            let src = agent.vault().read(p).unwrap_or_default();
            let fm = quire_app::markdown::extract(&src).front_matter;
            fm.as_ref().and_then(|f| f.get("aliases")).is_some_and(|v| match v {
                quire_app::markdown::Value::List(l) => l.iter().any(|a| fold(a) == want),
                quire_app::markdown::Value::Str(s) => s.split(',').any(|a| fold(a) == want),
                quire_app::markdown::Value::Map(_) => false,
            })
        })
        .cloned()
}

/// The highest footnote number used in `text`.
fn max_footnote(text: &str) -> usize {
    text.match_indices("[^").filter_map(|(i, _)| text[i + 2..].split(']').next()?.parse::<usize>().ok()).max().unwrap_or(0)
}

/// Inputs for one ingest.
pub struct Ingest<'a> {
    pub model: &'a dyn ChatModel,
    pub agent: &'a AgentStore,
    pub actor: &'a Actor,
    pub source: NotePath,
    /// `YYYY-MM-DD` and `YYYY-MM-DD HH:MM`, from the caller's clock.
    pub today: String,
    pub now: String,
}

/// Run an ingest. Nothing is written until every model call is done; then
/// the two change sets are submitted.
pub fn ingest(job: &Ingest<'_>, cancel: &CancelFlag, progress: &mut dyn FnMut(IngestEvent)) -> Result<IngestReport, String> {
    // 0.11: the source goes to the model only if this client may read it:
    // not revoked, AI not off, inside its allow lists and outside deny zones
    // and locked folders (the same check as every agent read).
    if let Some(why) = job.agent.refused(&job.actor.id) {
        return Err(why);
    }
    job.agent
        .guard(job.actor, &job.source, quire_agent::OpKind::Read)
        .map_err(|e| format!("{} isn't sent to the model: {e}", job.source))?;
    let vault = job.agent.vault();
    let source_text = vault.read(&job.source).map_err(|e| format!("couldn't read {}: {e}", job.source))?;
    let title = quire_app::markdown::extract(&source_text).title().map_or_else(|| job.source.stem().to_string(), str::to_string);
    let source_link = format!("[[{}]]", job.source.as_str().trim_end_matches(".md"));

    // 1. Plan (long sources are read in parts and condensed first).
    let working = if est_tokens(&source_text) > LONG_SOURCE_TOKENS {
        let chunks = split_chunks(&source_text, CHUNK_BYTES);
        let mut condensed = String::new();
        for (i, c) in chunks.iter().enumerate() {
            progress(IngestEvent::Step(format!("Reading part {} of {}", i + 1, chunks.len())));
            let v = ask_json(job.model, prompts::ingest_chunk(&title, i + 1, chunks.len(), c), cancel)?;
            for p in points(&v) {
                let _ = writeln!(condensed, "- {} (“{}”)", p.text, p.quote);
            }
        }
        condensed
    } else {
        source_text.clone()
    };
    progress(IngestEvent::Step("Planning".into()));
    let plan = ask_json(job.model, prompts::ingest_plan(&title, &working), cancel)?;
    let summary_title = file_name(plan["summary_title"].as_str().unwrap_or(&title));
    let mut concepts: Vec<(String, String)> = Vec::new();
    for c in plan["concepts"].as_array().into_iter().flatten() {
        let name = file_name(c["name"].as_str().unwrap_or_default());
        if name != "Untitled" && !concepts.iter().any(|(n, _)| fold(n) == fold(&name)) && concepts.len() < MAX_CONCEPTS {
            concepts.push((name, c["why"].as_str().unwrap_or_default().to_string()));
        }
    }

    // 2. The source summary.
    progress(IngestEvent::Step("Summarizing the source".into()));
    let names: Vec<String> = concepts.iter().map(|(n, _)| n.clone()).collect();
    let summary = ask_json(job.model, prompts::ingest_summary(&title, &working, &names), cancel)?;
    let (bullets, notes, mut dropped) = cite(&points(&summary), &job.source, &source_text, 1);
    let summary_path = NotePath::from(format!("wiki/sources/{summary_title}.md").as_str());
    let by = job.model.name().to_string();
    let mut page = format!(
        "---\ntype: source\nsource: \"{source_link}\"\ningested: {}\nby: {by}\nai_written: true\n---\n# {summary_title}\n\n",
        job.today
    );
    let _ = write!(page, "> [!ai] Written by {by} from {source_link} on {}\n\n", job.today);
    for b in &bullets {
        page.push_str(b);
        page.push('\n');
    }
    if !names.is_empty() {
        page.push_str("\n## Concepts\n\n");
        for n in &names {
            let _ = writeln!(page, "- [[{n}]]");
        }
    }
    if !notes.is_empty() {
        page.push('\n');
        for n in &notes {
            page.push_str(n);
            page.push('\n');
        }
    }

    // 3. Concept pages.
    let all = vault.notes().map_err(|e| e.to_string())?;
    let mut wiki_changes: Vec<Change> = Vec::new();
    let mut human_changes: Vec<Change> = Vec::new();
    let mut report = IngestReport::default();
    let summary_exists = vault.exists(&summary_path);
    if summary_exists {
        wiki_changes.push(Change::Edit { path: summary_path.clone(), new_text: page.clone() });
        report.updated.push(summary_path.clone());
    } else {
        wiki_changes.push(Change::Create { path: summary_path.clone(), text: page.clone() });
        report.created.push(summary_path.clone());
    }
    let mut new_pages: Vec<(NotePath, String)> = vec![(summary_path.clone(), page)];
    let section = format!("## From {summary_title}");
    for (i, (name, why)) in concepts.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err("stopped".into());
        }
        progress(IngestEvent::Step(format!("Concept {} of {}: {name}", i + 1, concepts.len())));
        let existing = find_page(job.agent, &all, name);
        let existing_text = existing.as_ref().map(|p| vault.read(p).unwrap_or_default());
        let v = ask_json(job.model, prompts::ingest_concept(&title, &working, name, why, existing_text.as_deref()), cancel)?;
        let pts = points(&v);
        if let (Some(path), Some(text)) = (&existing, &existing_text) {
            let (bullets, notes, d) = cite(&pts, &job.source, &source_text, max_footnote(text) + 1);
            dropped += d;
            if bullets.is_empty() || text.contains(&section) {
                continue;
            }
            let mut next = text.trim_end().to_string();
            let _ = write!(next, "\n\n{section}\n\n{}\n\n{}\n", bullets.join("\n"), notes.join("\n"));
            let change = Change::Edit { path: path.clone(), new_text: next.clone() };
            if path.as_str().starts_with(wikifiles::WIKI) {
                wiki_changes.push(change);
                report.updated.push(path.clone());
                new_pages.push((path.clone(), next));
            } else {
                human_changes.push(change);
            }
        } else {
            let (bullets, notes, d) = cite(&pts, &job.source, &source_text, 1);
            dropped += d;
            if bullets.is_empty() {
                continue;
            }
            let path = NotePath::from(format!("wiki/concepts/{name}.md").as_str());
            let definition = v["definition"].as_str().unwrap_or_default().trim().to_string();
            let mut text = format!("---\ntype: concept\nsources:\n  - \"{source_link}\"\nai_written: true\n---\n# {name}\n\n");
            if !definition.is_empty() {
                text.push_str(&definition);
                text.push_str("\n\n");
            }
            let _ = write!(text, "{section}\n\nFrom [[{summary_title}]]:\n\n{}\n\n{}\n", bullets.join("\n"), notes.join("\n"));
            wiki_changes.push(Change::Create { path: path.clone(), text: text.clone() });
            report.created.push(path.clone());
            new_pages.push((path, text));
        }
    }
    report.dropped = dropped;

    // 4. Links from the person's notes: the first plain mention of a concept
    // in a note that doesn't link to it yet (suggestions only).
    for (name, _) in &concepts {
        if human_changes.len() >= MAX_LINK_SUGGESTIONS {
            break;
        }
        for p in all.iter().filter(|p| !p.as_str().starts_with(wikifiles::WIKI) && !p.as_str().starts_with("raw/") && **p != job.source) {
            if human_changes.iter().any(|c| c.path() == p) {
                continue;
            }
            let text = vault.read(p).unwrap_or_default();
            if let Some(next) = link_first_mention(&text, name) {
                human_changes.push(Change::Edit { path: p.clone(), new_text: next });
                break;
            }
        }
    }

    // 5. Index and log, deterministically, as part of the same run.
    let mut pages: BTreeMap<NotePath, String> =
        all.iter().filter(|p| wikifiles::is_page(p)).map(|p| (p.clone(), vault.read(p).unwrap_or_default())).collect();
    for (p, t) in new_pages {
        pages.insert(p, t);
    }
    let mut all_after = all;
    for c in &wiki_changes {
        if let Change::Create { path, .. } = c {
            all_after.push(path.clone());
        }
    }
    let pages: Vec<(NotePath, String)> = pages.into_iter().collect();
    let index_path = NotePath::from(wikifiles::INDEX);
    let index_old = vault.read(&index_path).ok();
    if let Some(next) = wikifiles::regenerate(index_old.as_deref(), &pages, &all_after) {
        wiki_changes.push(if index_old.is_some() {
            Change::Edit { path: index_path, new_text: next }
        } else {
            Change::Create { path: index_path, text: next }
        });
    }
    let run = RunId::new(job.agent.now_ms());
    let log_path = NotePath::from(wikifiles::LOG);
    let log_old = vault.read(&log_path).ok();
    let touched = report.created.len() + report.updated.len();
    let line = wikifiles::log_line(
        &job.now,
        "ingest",
        &format!("{source_link} → {touched} page{}", if touched == 1 { "" } else { "s" }),
        &by,
        Some(run.as_str()),
    );
    let log_new = wikifiles::append_log(log_old.as_deref(), &line);
    wiki_changes.push(if log_old.is_some() {
        Change::Edit { path: log_path, new_text: log_new }
    } else {
        Change::Create { path: log_path, text: log_new }
    });

    // 6. Submit: the wiki, then the person's notes.
    progress(IngestEvent::Step("Saving".into()));
    let sources = vec![SourceRef::uri(quire_mcp::resources::uri_for_path(&job.source))];
    let wiki = ChangeSet {
        changes: wiki_changes,
        run: Some(run.clone()),
        title: format!("Ingest “{title}” into the wiki"),
        rationale: format!("Summary of {source_link}, {} concept page(s), index and log.", concepts.len()),
        sources: sources.clone(),
        ..ChangeSet::default()
    };
    match job.agent.submit(job.actor, wiki).map_err(|e| e.to_string())? {
        SubmitOutcome::Applied { .. } => {}
        SubmitOutcome::Proposed { id, .. } => report.proposals.push(id),
    }
    if !human_changes.is_empty() {
        let n = human_changes.len();
        let mine = ChangeSet {
            changes: human_changes,
            run: Some(run.clone()),
            title: format!("Link {n} of your note{} to the wiki", if n == 1 { "" } else { "s" }),
            rationale: format!("These notes mention concepts from “{title}”; a link connects them to the wiki pages."),
            sources,
            ..ChangeSet::default()
        };
        if let SubmitOutcome::Proposed { id, .. } = job.agent.submit(job.actor, mine).map_err(|e| e.to_string())? {
            report.proposals.push(id);
        }
    }
    report.run = Some(run);
    report.summary = Some(summary_path);
    progress(IngestEvent::Done(report.clone()));
    Ok(report)
}

/// `text` with the first plain mention of `name` (outside code, links and
/// front matter, at a word boundary) turned into `[[name]]`, if it doesn't
/// link to it already.
pub fn link_first_mention(text: &str, name: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let lname = name.to_lowercase();
    if lname.chars().count() < 4 || lower.contains(&format!("[[{lname}")) {
        return None;
    }
    let body_start = quire_app::markdown::extract(text).front_matter.map_or(0, |f| f.range.end);
    let mut from = body_start;
    while let Some(off) = lower.get(from..).and_then(|s| s.find(&lname)) {
        let at = from + off;
        let end = at + lname.len();
        from = end;
        if lower.len() != text.len() || !text.is_char_boundary(at) || !text.is_char_boundary(end) {
            // Case folding changed lengths; don't guess positions.
            return None;
        }
        let before = text[..at].chars().next_back();
        let after = text[end..].chars().next();
        if before.is_some_and(char::is_alphanumeric) || after.is_some_and(char::is_alphanumeric) {
            continue;
        }
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let line = &text[line_start..text[at..].find('\n').map_or(text.len(), |i| at + i)];
        let in_code = text[..at].matches("```").count() % 2 == 1 || text[line_start..at].matches('`').count() % 2 == 1;
        let in_link = text[line_start..at].matches("[[").count() > text[line_start..at].matches("]]").count();
        if in_code || in_link || line.trim_start().starts_with('#') {
            continue;
        }
        let found = &text[at..end];
        let link = if found == name { format!("[[{name}]]") } else { format!("[[{name}|{found}]]") };
        return Some(format!("{}{link}{}", &text[..at], &text[end..]));
    }
    None
}

/// Split on blank lines into parts of at most about `max` bytes.
fn split_chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split_inclusive("\n\n") {
        if !cur.is_empty() && cur.len() + para.len() > max {
            out.push(std::mem::take(&mut cur));
        }
        if para.len() > max {
            let mut rest = para;
            while rest.len() > max {
                let mut cut = max;
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            cur.push_str(rest);
        } else {
            cur.push_str(para);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_in_chatty_replies() {
        assert_eq!(first_json("Sure! ```json\n{\"a\": {\"b\": \"}\"}}\n```").unwrap()["a"]["b"], "}");
        assert!(first_json("no json").is_none());
    }

    #[test]
    fn quotes_are_located_loosely_but_not_vaguely() {
        let src = "Ropes make undo\n  CHEAP because subtrees are shared.";
        let r = locate(src, "ropes make undo cheap because").unwrap();
        assert_eq!(&src[r], "Ropes make undo\n  CHEAP because");
        assert!(locate(src, "undo").is_none(), "too short to cite");
        assert!(locate(src, "something else entirely").is_none());
    }

    #[test]
    fn mentions_become_links_once() {
        let t = "---\ntitle: Gap buffers\n---\n# Gap buffers\nUnlike `gap buffers` here, Gap buffers keep a hole.\n";
        assert_eq!(
            link_first_mention(t, "Gap buffers").unwrap(),
            "---\ntitle: Gap buffers\n---\n# Gap buffers\nUnlike `gap buffers` here, [[Gap buffers]] keep a hole.\n"
        );
        assert!(link_first_mention("see [[Gap buffers]]", "Gap buffers").is_none());
        assert_eq!(link_first_mention("ropes, Ropes", "Ropes").unwrap(), "[[Ropes|ropes]], Ropes");
        assert!(link_first_mention("tropes", "Ropes").is_none(), "word boundaries");
        assert_eq!(file_name("A/B: c?"), "A B c");
        assert_eq!(max_footnote("x[^2] y[^10] [^a]"), 10);
        assert_eq!(split_chunks("aaaa\n\nbbbb\n\ncc", 7), vec!["aaaa\n\n", "bbbb\n\n", "cc"]);
        let e = estimate(&"word ".repeat(30_000));
        assert!(e.parts > 1 && e.calls > 10, "{e:?}");
        assert!(e.describe().starts_with("about "));
    }
}
