// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Retrieval evaluation (design §B.6), behind `quire-cli eval` (feature
//! `eval`, never in releases).
//!
//! * **`synthetic-edits`**: generated from a seed, so every run is the same.
//!   300 notes about fictional projects whose facts change through scripted
//!   edits (the old value stays in a "History" section as a distractor), and
//!   200 questions whose gold answer and gold line come from the latest
//!   version, 40 of them unanswerable.
//! * **`longmemeval-s`**: the LongMemEval-S file the user supplies (it isn't
//!   vendored), one note per chat session.
//! * **`sample-vault`**: the repository's sample vault with answerable and
//!   unanswerable questions (answerability only).
//! * **`contradictions`** ([`contradict`]): labelled claim pairs through the
//!   contradiction pipeline, scored as the Inbox shows them.
//!
//! An embedder other than the built-in LSA can be given (an endpoint, for
//! the neural `τ_sem`); every run reports the abstain features and what each
//! `τ_sem` would do.
//!
//! Questions go through the MCP tools a client would call (`search`, `ask`
//! in extractive mode, `view` of the cited lines), in process. Reported:
//! recall@k of the gold passage (a hit's line range covers the gold line),
//! the same at note level, citation span accuracy (gold-line overlap of at
//! least 50%), abstain precision and recall, `view` tokens per answer,
//! and latency. Answers are judged by an LLM only when a provider is given
//! (not yet wired: `judge` is null).

pub mod contradict;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use quire_agent::{DeviceInfo, FixedClock};
use quire_app::intel::chat::CancelFlag;
use quire_app::vault::Vault;
use serde_json::{Value, json};

use crate::bounds::est_tokens;
use crate::server::{Conn, Reply, Server, ServerConfig};

/// One question with its gold answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub text: String,
    /// `None` for unanswerable questions.
    pub answer: Option<String>,
    /// Where the answer is: note path and 1-based line.
    pub gold: Option<(String, u32)>,
}

/// A suite: notes and questions.
#[derive(Debug, Clone, Default)]
pub struct Suite {
    pub name: String,
    pub notes: Vec<(String, String)>,
    pub questions: Vec<Question>,
    pub params: Value,
}

/// A deterministic random sequence (`SplitMix64`).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const FIRST: &[&str] = &[
    "Asha", "Bruno", "Chen", "Dalia", "Emeka", "Freya", "Goran", "Hana", "Ivo", "Jonas", "Kofi", "Lena", "Mateo", "Nadia", "Omar", "Priya",
];
const LAST: &[&str] =
    &["Moreno", "Okafor", "Lindqvist", "Tanaka", "Haddad", "Novak", "Silva", "Kowalski", "Byrne", "Iyer", "Fischer", "Mensah"];
const CITIES: &[&str] =
    &["Lisbon", "Nairobi", "Osaka", "Tallinn", "Montreal", "Porto", "Accra", "Lyon", "Bergen", "Cusco", "Hobart", "Graz"];
const BIRDS: &[&str] = &["Kestrel", "Heron", "Osprey", "Wren", "Plover", "Merlin", "Avocet", "Dunlin", "Petrel", "Shrike", "Swift", "Tern"];
const SECOND: &[&str] = &[
    "Lumen", "Quarry", "Harbor", "Cinder", "Meadow", "Atlas", "Beacon", "Cobalt", "Delta", "Ember", "Fjord", "Garnet", "Helix", "Indigo",
    "Juniper", "Kiln", "Lattice", "Mosaic", "Nimbus", "Orchard", "Prism", "Quill", "Ridge", "Sable", "Tundra",
];
const STATUS: &[&str] = &["planning", "active", "paused", "shipping", "under review"];

fn person(r: &mut Rng) -> String {
    format!("{} {}", r.pick(FIRST), r.pick(LAST))
}

/// The `synthetic-edits` suite for `seed`: `notes` notes (at most 300 distinct
/// projects), `questions` questions.
pub fn synthetic(seed: u64, notes: usize, questions: usize) -> Suite {
    let mut r = Rng(seed);
    let mut docs = Vec::with_capacity(notes);
    // (path, title, fact name, current value, line of the current fact, old value if it changed)
    let mut facts: Vec<(String, String, &str, String, u32)> = Vec::new();
    for i in 0..notes {
        // Distinct two-word names (12 × 25), so a question names one project.
        let name = format!("{} {}", BIRDS[i % BIRDS.len()], SECOND[(i / BIRDS.len()) % SECOND.len()]);
        let path = format!("Projects/Project {name}.md");
        let mut owner = person(&mut r);
        let mut city = r.pick(CITIES).to_string();
        let mut status = r.pick(STATUS).to_string();
        let mut history = Vec::new();
        // Scripted edits: about half the facts change once or twice.
        for _ in 0..r.below(3) {
            match r.below(3) {
                0 => {
                    let new = person(&mut r);
                    history.push(format!("- Ownership moved from {owner} to {new}."));
                    owner = new;
                }
                1 => {
                    let new = r.pick(CITIES).to_string();
                    history.push(format!("- The team moved from {city} to {new}."));
                    city = new;
                }
                _ => {
                    let new = r.pick(STATUS).to_string();
                    history.push(format!("- Status changed from {status} to {new}."));
                    status = new;
                }
            }
        }
        let mut text = format!(
            "# Project {name}\n\nProject {name} is one of the team's {} efforts.\n\n",
            r.pick(&["research", "infrastructure", "design", "outreach"])
        );
        let line = |text: &mut String, s: String| {
            text.push_str(&s);
            text.push('\n');
            u32::try_from(text.lines().count()).unwrap_or(0)
        };
        line(&mut text, "## Owner".into());
        let l_owner = line(&mut text, format!("The owner of Project {name} is {owner}."));
        text.push('\n');
        line(&mut text, "## Location".into());
        let l_city = line(&mut text, format!("Project {name} is based in {city}."));
        text.push('\n');
        line(&mut text, "## Status".into());
        let l_status = line(&mut text, format!("Project {name} is currently {status}."));
        if !history.is_empty() {
            text.push_str("\n## History\n");
            for h in &history {
                text.push_str(h);
                text.push('\n');
            }
        }
        facts.push((path.clone(), name.clone(), "owner", owner, l_owner));
        facts.push((path.clone(), name.clone(), "city", city, l_city));
        facts.push((path.clone(), name, "status", status, l_status));
        docs.push((path, text));
    }
    let unanswerable = questions / 5;
    let mut qs = Vec::with_capacity(questions);
    for i in 0..questions - unanswerable {
        let (path, name, fact, value, line) = facts[r.below(facts.len())].clone();
        let text = match fact {
            "owner" => format!("Who owns Project {name}?"),
            "city" => format!("Where is Project {name} based?"),
            _ => format!("What is the status of Project {name}?"),
        };
        qs.push(Question { id: format!("q{i:03}"), text, answer: Some(value), gold: Some((path, line)) });
    }
    for i in 0..unanswerable {
        // Projects that don't exist, or facts the notes never record.
        let text = if i % 2 == 0 {
            format!("Who owns Project {} Vortex?", r.pick(BIRDS))
        } else {
            let (_, name, ..) = facts[r.below(facts.len())].clone();
            format!("What is the annual budget of Project {name}?")
        };
        qs.push(Question { id: format!("u{i:03}"), text, answer: None, gold: None });
    }
    Suite {
        name: "synthetic-edits".into(),
        notes: docs,
        questions: qs,
        params: json!({"seed": seed, "notes": notes, "questions": questions, "unanswerable": unanswerable}),
    }
}

/// The LongMemEval-S suite from its JSON file (questions with `haystack_sessions`).
///
/// One suite per question would be slow, so all sessions go into one vault,
/// one note per session; gold is the answer session (line 1 of its note).
pub fn longmemeval(json_text: &str, limit: usize) -> Result<Suite, String> {
    let items: Vec<Value> = serde_json::from_str(json_text).map_err(|e| format!("not a LongMemEval file: {e}"))?;
    let mut notes = std::collections::BTreeMap::new();
    let mut qs = Vec::new();
    for item in items.iter().take(limit) {
        let qid = item.get("question_id").and_then(Value::as_str).ok_or("an item has no question_id")?;
        let ids = item.get("haystack_session_ids").and_then(Value::as_array).cloned().unwrap_or_default();
        let sessions = item.get("haystack_sessions").and_then(Value::as_array).cloned().unwrap_or_default();
        for (sid, sess) in ids.iter().zip(&sessions) {
            let sid = sid.as_str().unwrap_or_default();
            let path = format!("sessions/{}.md", sid.replace(['/', '\\'], "_"));
            notes.entry(path).or_insert_with(|| {
                let mut t = format!("# Session {sid}\n\n");
                for turn in sess.as_array().into_iter().flatten() {
                    let role = turn.get("role").and_then(Value::as_str).unwrap_or("user");
                    let content = turn.get("content").and_then(Value::as_str).unwrap_or_default().replace('\n', " ");
                    let _ = writeln!(t, "**{role}:** {content}\n");
                }
                t
            });
        }
        let gold = item
            .get("answer_session_ids")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(|s| (format!("sessions/{}.md", s.replace(['/', '\\'], "_")), 1));
        let abstain = qid.ends_with("_abs");
        qs.push(Question {
            id: qid.to_string(),
            text: item.get("question").and_then(Value::as_str).unwrap_or_default().to_string(),
            answer: (!abstain)
                .then(|| item.get("answer").map(|a| a.as_str().map_or_else(|| a.to_string(), str::to_string)).unwrap_or_default()),
            gold: if abstain { None } else { gold },
        });
    }
    Ok(Suite { name: "longmemeval-s".into(), notes: notes.into_iter().collect(), questions: qs, params: json!({"limit": limit}) })
}

/// JSON values, for callers without `serde_json`.
pub type Json = Value;

/// Pretty-printed JSON.
pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Results of one run.
#[derive(Debug, Clone)]
pub struct Report {
    pub json: Value,
    pub text: String,
}

fn call(srv: &Server, conn: &Conn, id: u64, tool: &str, args: &Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args,
        "_meta": {"io.modelcontextprotocol/protocolVersion": crate::protocol::MODERN, "io.modelcontextprotocol/clientInfo": {"name": "quire-eval", "version": "1"}}}});
    match srv.handle(&msg, conn, &CancelFlag::new()) {
        Reply::Response(v) => v["result"].clone(),
        _ => Value::Null,
    }
}

fn covers(lines: &Value, gold_line: u32) -> bool {
    let (Some(a), Some(b)) = (lines.get(0).and_then(Value::as_u64), lines.get(1).and_then(Value::as_u64)) else { return false };
    (a..=b).contains(&u64::from(gold_line))
}

/// How much of the gold line a span `[a, b]` covers, as overlap over the
/// union (a span on the exact line is 1; a 3-line span around it is 1/3).
fn overlap(a: u64, b: u64, gold: u64) -> f64 {
    if !(a..=b).contains(&gold) {
        return 0.0;
    }
    1.0 / (b - a + 1) as f64
}

fn ratio(n: usize, d: usize) -> Value {
    if d == 0 { Value::Null } else { json!((n as f64 / d as f64 * 1000.0).round() / 1000.0) }
}

/// Run a suite at `k` with the built-in embedder and report.
pub fn run(suite: &Suite, k: usize) -> Report {
    run_with(suite, k, None)
}

/// The questions of the repository's sample vault (`sample-vault/`).
///
/// The 12 answerable ones `ask`'s calibration test guards, and 10 its notes
/// don't answer. Gold lines aren't recorded (no recall@k), only answerability.
pub fn sample_vault(root: &std::path::Path) -> Result<Suite, String> {
    fn walk(d: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) -> Result<(), String> {
        for e in std::fs::read_dir(d).map_err(|e| format!("{}: {e}", d.display()))?.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out)?;
            } else if p.extension().is_some_and(|x| x == "md") {
                let rel = p.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
                out.push((rel, std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?));
            }
        }
        Ok(())
    }
    let mut notes = Vec::new();
    walk(root, root, &mut notes)?;
    notes.sort();
    let answerable = [
        "Why do editors use ropes?",
        "What does Emacs use to store text?",
        "Which editor uses a piece table?",
        "Why is undo cheap?",
        "What is the architecture of Quire?",
        "How fast is jumping to a line?",
        "What are the plans for sync?",
        "How are the layers of the app arranged?",
        "What is the budget for designers?",
        "Which data structure is best for large documents with scattered edits?",
        "Is there a mobile version?",
        "What did I write about importers?",
    ];
    let unanswerable = [
        "What is the annual budget of the rocket programme?",
        "Who won the chess tournament in Oslo?",
        "What is my sister's phone number?",
        "Which vaccine did the clinic recommend?",
        "How much did the kitchen renovation cost?",
        "What is the recipe for sourdough bread?",
        "When does the train to Vienna leave?",
        "What grade did Maria get in chemistry?",
        "Which hiking boots did I buy last winter?",
        "What is the capital of Mongolia?",
    ];
    let mut questions: Vec<Question> = answerable
        .iter()
        .enumerate()
        .map(|(i, q)| Question { id: format!("a{i}"), text: (*q).into(), answer: Some(String::new()), gold: None })
        .collect();
    questions.extend(unanswerable.iter().enumerate().map(|(i, q)| Question {
        id: format!("u{i}"),
        text: (*q).into(),
        answer: None,
        gold: None,
    }));
    Ok(Suite { name: "sample-vault".into(), notes, questions, params: json!({"root": root.display().to_string()}) })
}

/// The `τ_sem` values the abstain sweep tries.
const TAUS: [f32; 21] = [0.0, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35, 0.4, 0.45, 0.5, 0.55, 0.6, 0.65, 0.7, 0.75, 0.8, 0.85, 0.9, 0.95, 1.0];

fn range(v: &[f32]) -> Value {
    let lo = v.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if v.is_empty() { Value::Null } else { json!([(f64::from(lo) * 1000.0).round() / 1000.0, (f64::from(hi) * 1000.0).round() / 1000.0]) }
}

/// Run a suite at `k` and report. `semantic` replaces the built-in embedder
/// (an endpoint embedder, for neural calibration).
pub fn run_with(suite: &Suite, k: usize, semantic: Option<quire_app::index::SemanticBuilder>) -> Report {
    let files: Vec<(&str, &str)> = suite.notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let t0 = Instant::now();
    let vault = Vault::in_memory(&files);
    let cfg = ServerConfig {
        device: Some(DeviceInfo { id: "d_eval".into(), name: "eval".into() }),
        clock: FixedClock::new(1_793_577_600_000),
        ..ServerConfig::default()
    };
    let srv = Server::with_config(vault, None, cfg);
    if let Some(b) = semantic {
        srv.index().set_semantic_builder(Some(b));
        srv.index().flush_semantic();
    }
    let index_ms = t0.elapsed().as_millis();
    let sem = srv.index().semantic();
    let embedder = sem.as_deref().map_or_else(|| "none".to_string(), |s| s.embedder_name().to_string());
    // Abstain features of each question (as `ask` sees them): best passage
    // similarity and coverage, and whether anything was found.
    let mut features: Vec<(bool, Option<(f32, f32)>)> = Vec::new();
    let conn = Conn::stdio(srv.new_conn());
    let (mut found, mut answerable, mut note_found) = (0usize, 0usize, 0usize);
    let (mut span_ok, mut cited) = (0usize, 0usize);
    let (mut abstain_right, mut abstained, mut should_abstain) = (0usize, 0usize, 0usize);
    let mut view_tokens = Vec::new();
    let mut latencies: Vec<Duration> = Vec::new();
    let mut misses = Vec::new();
    for (i, q) in suite.questions.iter().enumerate() {
        let id = u64::try_from(i).unwrap_or(0) * 3;
        let started = Instant::now();
        let hits = call(&srv, &conn, id, "search", &json!({"query": q.text, "limit": k, "max_tokens": 20_000}));
        let ask = call(&srv, &conn, id + 1, "ask", &json!({"question": q.text, "k": k}));
        let s = &ask["structuredContent"];
        let did_abstain = s["abstained"] == true;
        {
            let ix = srv.index().read();
            let src = quire_app::intel::retrieve(&ix, sem.as_deref(), &q.text, k);
            let best = (!src.is_empty())
                .then(|| (src.iter().map(|s| s.sim).fold(0.0, f32::max), src.iter().map(|s| s.coverage).fold(0.0, f32::max)));
            features.push((q.answer.is_some(), best));
        }
        // What a client reads to verify the answer: the first citation's lines.
        if let Some(c) = s["citations"].get(0) {
            let v = call(&srv, &conn, id + 2, "view", &json!({"path": c["path"], "lines": c["lines"]}));
            view_tokens.push(est_tokens(v["content"][0]["text"].as_str().unwrap_or_default()));
        }
        latencies.push(started.elapsed());
        if did_abstain {
            abstained += 1;
        }
        match (&q.answer, &q.gold) {
            (None, _) => {
                should_abstain += 1;
                if did_abstain {
                    abstain_right += 1;
                }
            }
            // Answerable, but without a gold line (the sample vault).
            (Some(_), None) => {}
            (Some(_), Some((path, line))) => {
                answerable += 1;
                let top: Vec<&Value> = hits["structuredContent"]["hits"].as_array().into_iter().flatten().take(k).collect();
                if top.iter().any(|h| h["path"] == path.as_str()) {
                    note_found += 1;
                }
                let hit = top.iter().any(|h| h["path"] == path.as_str() && covers(&h["lines"], *line));
                if hit {
                    found += 1;
                } else if misses.len() < 10 {
                    misses.push(json!({"id": q.id, "question": q.text, "gold": path}));
                }
                if let Some(c) = s["citations"].as_array().and_then(|cs| cs.iter().find(|c| c["path"] == path.as_str())) {
                    cited += 1;
                    let (a, b) = (c["lines"][0].as_u64().unwrap_or(0), c["lines"][1].as_u64().unwrap_or(0));
                    if overlap(a, b, u64::from(*line)) >= 0.5 {
                        span_ok += 1;
                    }
                } else if !s["citations"].as_array().is_some_and(Vec::is_empty) {
                    cited += 1;
                }
            }
        }
    }
    // What each τ_sem would do (abstain = nothing found, or similarity < τ
    // and coverage < COVERAGE_MIN): wrong "Not found"s and abstain recall.
    let cov_min = quire_app::intel::ask::COVERAGE_MIN;
    let sweep: Vec<Value> = TAUS
        .iter()
        .map(|&tau| {
            let abstains = |f: &Option<(f32, f32)>| f.is_none_or(|(sim, cov)| sim < tau && cov < cov_min);
            let wrong = features.iter().filter(|(a, f)| *a && abstains(f)).count();
            let right = features.iter().filter(|(a, f)| !*a && abstains(f)).count();
            let unans = features.iter().filter(|(a, _)| !*a).count();
            json!({"tau": (f64::from(tau) * 100.0).round() / 100.0, "wrong_not_found": wrong, "abstain_recall": ratio(right, unans), "abstain_precision": ratio(right, right + wrong)})
        })
        .collect();
    let feats = |answerable: bool, pick: fn(&(f32, f32)) -> f32| -> Vec<f32> {
        features.iter().filter(|(a, _)| *a == answerable).filter_map(|(_, f)| f.as_ref().map(pick)).collect()
    };
    let abstain_features = json!({
        "answerable": {"sim": range(&feats(true, |f| f.0)), "coverage": range(&feats(true, |f| f.1))},
        "unanswerable": {"sim": range(&feats(false, |f| f.0)), "coverage": range(&feats(false, |f| f.1))},
    });
    let tau_now = quire_app::intel::ask::tau_sem(sem.as_deref());
    latencies.sort();
    let pct = |p: f64| latencies.get(((latencies.len() as f64 - 1.0) * p).round() as usize).map_or(0, Duration::as_micros) as f64 / 1000.0;
    let mean_tokens = if view_tokens.is_empty() { 0.0 } else { view_tokens.iter().sum::<usize>() as f64 / view_tokens.len() as f64 };
    let json = json!({
        "suite": suite.name,
        "params": suite.params,
        "k": k,
        "quire": env!("CARGO_PKG_VERSION"),
        "questions": suite.questions.len(),
        "index_ms": index_ms,
        "recall_at_k": ratio(found, answerable),
        "note_recall_at_k": ratio(note_found, answerable),
        "citation_span_accuracy": ratio(span_ok, cited),
        "abstain_precision": ratio(abstain_right, abstained),
        "abstain_recall": ratio(abstain_right, should_abstain),
        "view_tokens_per_answer": (mean_tokens * 10.0).round() / 10.0,
        "latency_ms": {"p50": pct(0.5), "p95": pct(0.95), "max": pct(1.0)},
        "judge": Value::Null,
        "misses": misses,
        "embedder": embedder,
        "tau_sem": (f64::from(tau_now) * 100.0).round() / 100.0,
        "abstain_features": abstain_features,
        "tau_sweep": sweep,
    });
    let mut text = format!("{} ({} notes, {} questions, k = {k})\n", suite.name, suite.notes.len(), suite.questions.len());
    for key in
        ["recall_at_k", "note_recall_at_k", "citation_span_accuracy", "abstain_precision", "abstain_recall", "view_tokens_per_answer"]
    {
        let _ = writeln!(text, "  {key:<24} {}", json[key]);
    }
    let _ = writeln!(text, "  {:<24} p50 {} ms, p95 {} ms", "latency", json["latency_ms"]["p50"], json["latency_ms"]["p95"]);
    let _ = writeln!(text, "  {:<24} {} (τ_sem {})", "embedder", json["embedder"], json["tau_sem"]);
    let af = &json["abstain_features"];
    let _ = writeln!(
        text,
        "  {:<24} answerable sim {} cov {}; unanswerable sim {} cov {}",
        "abstain features",
        af["answerable"]["sim"],
        af["answerable"]["coverage"],
        af["unanswerable"]["sim"],
        af["unanswerable"]["coverage"]
    );
    let _ = writeln!(text, "  τ_sem sweep (wrong \"Not found\" / abstain recall):");
    for row in json["tau_sweep"].as_array().into_iter().flatten() {
        let _ = writeln!(text, "    {:<5} {:>3} / {}", row["tau"], row["wrong_not_found"], row["abstain_recall"]);
    }
    Report { json, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_suite_is_deterministic() {
        let a = synthetic(7, 30, 20);
        let b = synthetic(7, 30, 20);
        assert_eq!(a.notes, b.notes);
        assert_eq!(a.questions, b.questions);
        assert_ne!(synthetic(8, 30, 20).notes, a.notes);
        assert_eq!(a.questions.iter().filter(|q| q.answer.is_none()).count(), 4);
        // Gold lines point at the current fact.
        for q in a.questions.iter().filter(|q| q.gold.is_some()) {
            let (path, line) = q.gold.clone().unwrap();
            let text = &a.notes.iter().find(|(p, _)| *p == path).unwrap().1;
            let l = text.lines().nth(line as usize - 1).unwrap();
            assert!(l.contains(q.answer.as_deref().unwrap()), "{l} / {:?}", q.answer);
        }
    }

    #[test]
    fn longmemeval_files_become_session_notes() {
        let data = json!([
            {"question_id": "q1", "question": "What did I name my cat?", "answer": "Miso",
             "haystack_session_ids": ["s1", "s2"], "answer_session_ids": ["s2"],
             "haystack_sessions": [[{"role": "user", "content": "I like tea."}], [{"role": "user", "content": "My cat is called Miso."}]]},
            {"question_id": "q2_abs", "question": "What is my dog's name?", "answer": "unknown",
             "haystack_session_ids": ["s1"], "answer_session_ids": [], "haystack_sessions": [[{"role": "user", "content": "I like tea."}]]}
        ]);
        let s = longmemeval(&data.to_string(), 10).unwrap();
        assert_eq!(s.notes.len(), 2);
        assert!(s.notes[1].1.contains("**user:** My cat is called Miso."));
        assert_eq!(s.questions[0].gold, Some(("sessions/s2.md".into(), 1)));
        assert_eq!(s.questions[1].answer, None);
        let r = run(&s, 3);
        assert_eq!(r.json["questions"], 2);
        assert!(longmemeval("{}", 1).is_err());
    }

    #[test]
    fn spans_and_ratios() {
        assert!((overlap(5, 5, 5) - 1.0).abs() < f64::EPSILON);
        assert!(overlap(4, 6, 5) < 0.5);
        assert!(overlap(1, 2, 5).abs() < f64::EPSILON);
        assert_eq!(ratio(1, 0), Value::Null);
        assert_eq!(ratio(2, 3), json!(0.667));
    }
}
