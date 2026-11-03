// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The thinking timeline's data (design §C.7), built on a worker thread.
//!
//! For a topic (a tag, a note or a search) it takes the notes in scope (at
//! most [`MAX_NOTES`]), replays each note's saved versions (at most 20) and
//! dates every claim that appeared or was taken out. Claims come from the
//! claims cache when the model has seen that text, else from the offline
//! sentence heuristic (then the view says "approximate"). Contradictions
//! found by the claims job are added from `seen.jsonl`. "Backfill" asks the
//! model for the versions it hasn't seen, within a request limit.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use quire_index::IndexService;
use quire_index::chunk::chunk_note;
use quire_index::tokenize::content_tokens;
use quire_intel::chat::{CancelFlag, ChatModel, Locality, chat_text};
use quire_intel::claims::{self, Claim, ClaimCache};
use quire_intel::timeline::{self, Item, MAX_VERSIONS};
use quire_vault::{NotePath, Vault};

/// Notes in one timeline.
pub const MAX_NOTES: usize = 40;
/// Model requests one backfill may make.
pub const BACKFILL_REQUESTS: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topic {
    Tag(String),
    Note(NotePath),
    Query(String),
}

impl Topic {
    /// `#tag`, `[[Note]]` or words.
    pub fn parse(text: &str, resolve: &dyn Fn(&str) -> Option<NotePath>) -> Option<Self> {
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        if let Some(tag) = t.strip_prefix('#').filter(|x| !x.is_empty() && !x.contains(' ')) {
            return Some(Self::Tag(tag.to_string()));
        }
        if let Some(inner) = t.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")) {
            return resolve(inner).map(Self::Note).or_else(|| Some(Self::Query(inner.to_string())));
        }
        Some(Self::Query(t.to_string()))
    }

    pub fn label(&self) -> String {
        match self {
            Self::Tag(t) => format!("#{t}"),
            Self::Note(p) => p.stem().to_string(),
            Self::Query(q) => format!("“{q}”"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TimelineResult {
    pub items: Vec<Item>,
    pub notes: usize,
    /// Some claims come from the sentence heuristic.
    pub approximate: bool,
    /// Requests the backfill made.
    pub requests: u32,
    /// The backfill stopped at its limit.
    pub capped: bool,
}

/// How the claims of a text are found.
enum Extract<'a> {
    Heuristic,
    /// The cache only (no requests): the heuristic where it has nothing.
    Cached(&'a ClaimCache),
    /// The cache, then the model while requests last.
    Model(&'a ClaimCache, &'a dyn ChatModel, &'a mut u32),
}

fn claims_of(path: &NotePath, text: &str, how: &mut Extract<'_>, cancel: &CancelFlag, approximate: &mut bool) -> Vec<Claim> {
    let mut out = Vec::new();
    for c in chunk_note(path, text) {
        let hash = claims::passage_hash(&c.text);
        let found = match how {
            Extract::Heuristic => None,
            Extract::Cached(cache) => cache.get(&hash),
            Extract::Model(cache, model, left) => cache.get(&hash).or_else(|| {
                if **left == 0 || cancel.is_cancelled() {
                    return None;
                }
                **left -= 1;
                let (reply, _) = chat_text(*model, &claims::extraction_request(&c.text), cancel).ok()?;
                let found = claims::parse_claims(&reply, &c.text).unwrap_or_default();
                let _ = cache.put(&hash, &found);
                Some(found)
            }),
        };
        if let Some(f) = found {
            out.extend(f);
        } else {
            *approximate = true;
            out.extend(claims::heuristic_claims(c.text.split_once('\n').map_or(c.text.as_str(), |(_, b)| b)));
        }
    }
    out
}

/// The notes a topic covers.
pub fn scope(index: &IndexService, topic: &Topic) -> Vec<NotePath> {
    let mut v: Vec<NotePath> = match topic {
        Topic::Tag(t) => index.read().notes_with_tag(t),
        Topic::Note(p) => vec![p.clone()],
        Topic::Query(q) => {
            let mut seen = HashSet::new();
            index
                .search_sections(q, &quire_index::SearchFilters::default(), 3 * MAX_NOTES)
                .into_iter()
                .map(|h| h.path)
                .filter(|p| seen.insert(p.clone()))
                .collect()
        }
    };
    v.truncate(MAX_NOTES);
    v
}

/// Build the timeline of `notes`. `model` (with its cache) is for a backfill.
pub fn build(
    vault: &Vault,
    notes: &[(NotePath, String)],
    topic: &Topic,
    model: Option<(&dyn ChatModel, &str)>,
    cache_model: Option<&str>,
    cancel: &CancelFlag,
) -> TimelineResult {
    let mut items = Vec::new();
    let mut approximate = false;
    let mut left = BACKFILL_REQUESTS;
    let cache = model.map(|(_, name)| name).or(cache_model).map(|m| ClaimCache::new(vault.clone(), &claims::extractor_key(m)));
    let query_terms: HashSet<String> = match topic {
        Topic::Query(q) => content_tokens(q).collect(),
        _ => HashSet::new(),
    };
    for (path, current) in notes {
        if cancel.is_cancelled() {
            break;
        }
        let mut versions: Vec<(i64, String)> =
            vault.history(path).into_iter().take(MAX_VERSIONS).filter_map(|s| vault.read_snapshot(&s).ok().map(|t| (s.at_ms, t))).collect();
        versions.reverse();
        let now = vault
            .storage()
            .modified(path)
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        if versions.last().is_none_or(|(_, t)| t != current) {
            versions.push((now.max(versions.last().map_or(0, |v| v.0 + 1)), current.clone()));
        }
        let mut replayed: Vec<(i64, Vec<Claim>)> = Vec::new();
        for (at, text) in &versions {
            let mut how = match (&cache, model) {
                (Some(c), Some((m, _))) => Extract::Model(c, m, &mut left),
                (Some(c), None) => Extract::Cached(c),
                (None, _) => Extract::Heuristic,
            };
            let mut cs = claims_of(path, text, &mut how, cancel, &mut approximate);
            if !query_terms.is_empty() {
                cs.retain(|c| content_tokens(&c.text).any(|t| query_terms.contains(&t)));
            }
            replayed.push((*at, cs));
        }
        items.extend(timeline::replay(path, &replayed));
    }
    let scope: HashSet<NotePath> = notes.iter().map(|(p, _)| p.clone()).collect();
    let known: HashSet<String> = items.iter().map(|i| i.claim.clone()).collect();
    items.extend(
        timeline::from_seen(&timeline::read_seen(vault), &scope)
            .into_iter()
            .filter(|i| i.event == timeline::Event::Contradicted || known.contains(&i.claim)),
    );
    TimelineResult {
        items: timeline::merge(items),
        notes: notes.len(),
        approximate,
        requests: BACKFILL_REQUESTS - left,
        capped: model.is_some() && left == 0,
    }
}

/// The Timeline tab's state.
#[derive(Default)]
pub struct TimelineState {
    /// What the person typed.
    pub topic_text: String,
    pub topic: Option<Topic>,
    pub result: Option<TimelineResult>,
    pub error: Option<String>,
    pending: Option<(Receiver<TimelineResult>, CancelFlag, bool)>,
}

impl TimelineState {
    pub const fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// A backfill is running.
    pub fn backfilling(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.2)
    }

    pub fn cancel(&mut self) {
        if let Some((_, c, _)) = self.pending.take() {
            c.cancel();
        }
    }

    /// Start building for `topic`; a backfill with `model`.
    pub fn start(
        &mut self,
        vault: Vault,
        index: &IndexService,
        topic: Topic,
        model: Option<Arc<dyn ChatModel>>,
        cache_model: Option<String>,
    ) {
        self.cancel();
        let notes: Vec<(NotePath, String)> = {
            let paths = scope(index, &topic);
            let ix = index.read();
            paths.into_iter().filter_map(|p| ix.source(&p).map(|s| (p, s.to_string()))).collect()
        };
        self.topic = Some(topic.clone());
        self.error = None;
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let backfill = model.is_some();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("quire-timeline".into()).spawn(move || {
            let name = model.as_ref().map(|m| m.name().to_string());
            let m = model.as_deref().zip(name.as_deref());
            let r = build(&vault, &notes, &topic, m, cache_model.as_deref(), &flag);
            let _ = tx.send(r);
        });
        match spawned {
            Ok(_) => self.pending = Some((rx, cancel, backfill)),
            Err(e) => self.error = Some(format!("Couldn't start: {e}")),
        }
    }

    /// Take a finished result. True when something changed.
    pub fn poll(&mut self) -> bool {
        let Some((rx, ..)) = &self.pending else { return false };
        match rx.try_recv() {
            Ok(r) => {
                self.result = Some(r);
                self.pending = None;
                true
            }
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                self.error = Some("Building the timeline failed.".into());
                true
            }
            Err(TryRecvError::Empty) => false,
        }
    }
}

/// Whether a model may be used for a backfill: the person turned claim
/// checks on, and cloud models stay within the daily cap.
pub fn backfill_allowed(model: &dyn ChatModel, contradictions_on: bool) -> bool {
    contradictions_on || model.locality() != Locality::Cloud
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_intel::chat::ScriptedChat;
    use quire_intel::timeline::Event;

    #[test]
    fn replays_history_with_the_heuristic_and_a_backfill() {
        let v = Vault::in_memory(&[("Plan.md", "# Plan\nThe launch moved to June 2027 after the review.\n")]);
        let p = NotePath::new("Plan.md");
        v.snapshot(&p, "# Plan\nThe launch is planned for March 2027 at the latest.\n", 1_000, true).unwrap();
        v.snapshot(&p, "# Plan\nThe launch moved to June 2027 after the review.\n", 2_000, true).unwrap();
        let notes = vec![(p.clone(), v.read(&p).unwrap())];
        let cancel = CancelFlag::new();
        let r = build(&v, &notes, &Topic::Note(p.clone()), None, None, &cancel);
        assert!(r.approximate);
        let ev: Vec<(i64, Event)> = r.items.iter().map(|i| (i.at, i.event)).collect();
        assert_eq!(ev, [(1_000, Event::Added), (2_000, Event::Added), (2_000, Event::Removed)]);
        let q = build(&v, &notes, &Topic::Query("review".into()), None, None, &cancel);
        assert_eq!(q.items.len(), 1, "only claims about the query");
        // Backfill: the model's claims, cached for next time.
        let model = ScriptedChat::texts([
            r#"[{"text":"Launch is March 2027.","source_span":"The launch is planned for March 2027"}]"#,
            r#"[{"text":"Launch is June 2027.","source_span":"The launch moved to June 2027"}]"#,
        ]);
        let r = build(&v, &notes, &Topic::Note(p.clone()), Some((&model, "Scripted")), None, &cancel);
        assert!(!r.approximate);
        assert_eq!(r.requests, 2);
        assert_eq!(
            r.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            ["Launch is March 2027.", "Launch is June 2027.", "Launch is March 2027."]
        );
        let cached = build(&v, &notes, &Topic::Note(p), None, Some("Scripted"), &cancel);
        assert!(!cached.approximate, "the cache serves every version now");
        let resolve = |s: &str| (s == "Plan").then(|| NotePath::new("Plan.md"));
        assert_eq!(Topic::parse("#work", &resolve), Some(Topic::Tag("work".into())));
        assert_eq!(Topic::parse("[[Plan]]", &resolve), Some(Topic::Note(NotePath::new("Plan.md"))));
        assert_eq!(Topic::parse("launch dates", &resolve), Some(Topic::Query("launch dates".into())));
        assert_eq!(Topic::parse("  ", &resolve), None);
    }
}
