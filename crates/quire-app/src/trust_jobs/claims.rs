// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Claims and the contradiction finder (design §C.5), a Preview that is
//! off until the person turns it on (Settings → AI).
//!
//! While the person is idle, the job goes through changed passages: their
//! claims come from the cache or one model request each; new claims are
//! compared with their nearest claims elsewhere, and the model judges each
//! new pair once. Cloud requests count against a daily cap. Pairs judged a
//! contradiction (or a strong tension) become Inbox items; an item whose
//! claim was edited away is resolved.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use quire_agent::inbox::{InboxItem, Verdict};
use quire_index::chunk::chunk_note;
use quire_intel::anchor::{self, Anchor};
use quire_intel::chat::{ChatModel, Locality, chat_text};
use quire_intel::claims::{self, Claim, ClaimCache};
use quire_intel::contradict::{self, ClaimIndex, ClaimRef, JudgeCache, JudgeSide, Relation};
use quire_intel::timeline::{self, Event, Seen};
use quire_markdown::Value;
use quire_vault::{ContentHash, NotePath, Vault};

use super::TrustShared;
use crate::jobs::{Budget, DailyCap, Job, JobCx, Needs, Step};

/// What the claims job is doing, for Settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimsStatus {
    /// Model requests today (extraction and judging).
    pub requests_today: u32,
    /// Passages waiting.
    pub waiting: usize,
    pub claims: usize,
    pub contradictions: usize,
    /// Why it stopped, if it did.
    pub note: Option<String>,
}

/// One passage to go through.
#[derive(Debug, Clone)]
struct Passage {
    path: NotePath,
    start: usize,
    end: usize,
    text: String,
    hash: String,
    raw: bool,
}

/// A note is in a raw (sources) zone: never the subject of a contradiction.
fn is_raw(agent: &quire_agent::AgentStore, path: &NotePath) -> bool {
    agent.policy().zones.iter().any(|z| {
        z.role == quire_agent::policy::ZoneRole::Raw && {
            let zp = z.path.trim_end_matches('/').to_lowercase();
            let p = path.as_str().to_lowercase();
            p == zp || p.starts_with(&format!("{zp}/"))
        }
    })
}

/// Notes marked as replaced or past their `valid_until`.
fn retired(props: &std::collections::BTreeMap<String, Value>, today: chrono::NaiveDate) -> bool {
    let get = |k: &str| props.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).and_then(|(_, v)| v.as_str().map(str::to_string));
    get("superseded_by").is_some_and(|s| !s.trim().is_empty())
        || get("valid_until").and_then(|d| quire_intel::stale::parse_date(&d)).is_some_and(|d| d < today)
}

/// How many passages the first run would send: (passages, not yet cached).
pub fn estimate(vault: &Vault, notes: &[(NotePath, String)], model: &str) -> (usize, usize) {
    let cache = ClaimCache::new(vault.clone(), &claims::extractor_key(model));
    let mut total = 0;
    let mut missing = 0;
    for (p, src) in notes {
        for c in chunk_note(p, src) {
            total += 1;
            if cache.get(&claims::passage_hash(&c.text)).is_none() {
                missing += 1;
            }
        }
    }
    (total, missing)
}

pub struct ClaimsJob {
    shared: Arc<TrustShared>,
    cap: DailyCap,
    /// Notes gone through, at this version.
    done: HashMap<NotePath, ContentHash>,
    queue: VecDeque<Passage>,
    /// Claims of each passage last time (for the timeline's events).
    previous: HashMap<(NotePath, usize), Vec<Claim>>,
    index: ClaimIndex,
    judges: Option<(String, JudgeCache)>,
    /// Claim vectors were made with this semantic index version.
    vectors_of: u64,
    model_name: String,
}

impl ClaimsJob {
    pub fn new(shared: Arc<TrustShared>) -> Self {
        Self {
            shared,
            cap: DailyCap::default(),
            done: HashMap::new(),
            queue: VecDeque::new(),
            previous: HashMap::new(),
            index: ClaimIndex::default(),
            judges: None,
            vectors_of: 0,
            model_name: String::new(),
        }
    }

    fn status(&self, f: impl FnOnce(&mut ClaimsStatus)) {
        if let Ok(mut s) = self.shared.claims.lock() {
            f(&mut s);
        }
    }

    /// Take one request from today's cap (cloud models only).
    fn take_call(&mut self, model: &dyn ChatModel, cap: u32) -> bool {
        if model.locality() != Locality::Cloud {
            return true;
        }
        let ok = self.cap.try_take(cap, chrono::Utc::now().timestamp_millis());
        if ok {
            self.status(|s| s.requests_today += 1);
        }
        ok
    }

    /// Queue the passages of notes that changed since last time.
    fn refill(&mut self, cx: &JobCx) {
        let notes: Vec<(NotePath, String)> = cx
            .index
            .read()
            .map(|ix| {
                ix.paths()
                    .into_iter()
                    .filter_map(|p| {
                        let src = ix.source(&p)?;
                        let h = ContentHash::of(src);
                        (self.done.get(&p) != Some(&h)).then(|| (p, src.to_string()))
                    })
                    .take(200)
                    .collect()
            })
            .unwrap_or_default();
        for (p, src) in notes {
            self.done.insert(p.clone(), ContentHash::of(&src));
            self.index.remove_note(&p);
            let raw = is_raw(&cx.agent, &p);
            for c in chunk_note(&p, &src) {
                let hash = claims::passage_hash(&c.text);
                self.queue.push_back(Passage { path: p.clone(), start: c.range.start, end: c.range.end, text: c.text, hash, raw });
            }
            Self::resolve_gone(cx, &p, &src);
        }
        // Notes that went away.
        let live: HashSet<NotePath> = cx.index.read().map(|ix| ix.paths().into_iter().collect()).unwrap_or_default();
        let gone: Vec<NotePath> = self.done.keys().filter(|p| !live.contains(*p)).cloned().collect();
        for p in gone {
            self.done.remove(&p);
            self.index.remove_note(&p);
            Self::resolve_gone(cx, &p, "");
        }
        let waiting = self.queue.len();
        self.status(|s| s.waiting = waiting);
    }

    /// Contradictions about `path` whose claim is no longer in it are resolved.
    fn resolve_gone(cx: &JobCx, path: &NotePath, src: &str) {
        let inbox = cx.agent.inbox();
        for s in inbox.stored() {
            let InboxItem::Contradiction { a, b, .. } = &s.item else { continue };
            for side in [a, b] {
                if &side.path == path && !anchor::resolve(side, src, None).holds() {
                    let _ = inbox.resolve(&s.fingerprint);
                }
            }
        }
    }

    fn vector(&self, text: &str) -> Option<Vec<f32>> {
        let sem = self.shared.semantic()?;
        let v = sem.embed_query(text);
        (!v.iter().all(|x| *x == 0.0)).then_some(v)
    }

    /// The claims of one passage: cached, or one model request (and one retry).
    fn extract(&mut self, cx: &JobCx, model: &dyn ChatModel, cap: u32, p: &Passage) -> Result<Option<Vec<Claim>>, String> {
        let cache = ClaimCache::new(cx.vault.clone(), &claims::extractor_key(model.name()));
        if let Some(c) = cache.get(&p.hash) {
            return Ok(Some(c));
        }
        for _ in 0..2 {
            if !self.take_call(model, cap) {
                return Ok(None);
            }
            let (reply, _) = chat_text(model, &claims::extraction_request(&p.text), &cx.cancel).map_err(|e| e.to_string())?;
            if let Some(found) = claims::parse_claims(&reply, &p.text) {
                cache.put(&p.hash, &found)?;
                return Ok(Some(found));
            }
        }
        // Two unreadable replies: remember "none" so it isn't asked again.
        cache.put(&p.hash, &[])?;
        Ok(Some(Vec::new()))
    }

    fn step_passage(&mut self, cx: &JobCx, model: &Arc<dyn ChatModel>, cap: u32) -> Step {
        let Some(p) = self.queue.pop_front() else { return Step::Idle(Duration::from_secs(5)) };
        let claims = match self.extract(cx, model.as_ref(), cap, &p) {
            Ok(Some(c)) => c,
            Ok(None) => {
                self.queue.push_front(p);
                self.status(|s| s.note = Some("Today's request limit is reached; checking continues tomorrow.".into()));
                return Step::Idle(Duration::from_mins(10));
            }
            Err(e) => {
                self.queue.push_front(p);
                self.status(|s| s.note = Some(format!("The AI model failed: {e}. Trying again in a few minutes.")));
                return Step::Idle(Duration::from_mins(5));
            }
        };
        self.status(|s| s.note = None);
        // The passage's text in its note: offsets of claim spans move with it.
        let src = cx.index.read().ok().and_then(|ix| ix.source(&p.path).map(str::to_string)).unwrap_or_default();
        let body_at = src.get(p.start..p.end.min(src.len())).and_then(|b| {
            let body = p.text.split_once('\n').map_or(p.text.as_str(), |(_, b)| b);
            b.find(body.lines().next().unwrap_or_default()).map(|i| (p.start + i, body))
        });
        let refs: Vec<ClaimRef> = claims
            .iter()
            .filter_map(|c| {
                let span_text = p.text.get(c.span.0..c.span.1)?;
                let (at, _) = body_at?;
                let i = src.get(at..p.end.min(src.len()))?.find(span_text)?;
                Some(ClaimRef {
                    id: c.id.clone(),
                    text: c.text.clone(),
                    subject: c.subject.clone(),
                    path: p.path.clone(),
                    passage: p.start,
                    span: (at + i, at + i + span_text.len()),
                })
            })
            .collect();
        self.record_seen(cx, &p, &claims);
        let today = chrono::Local::now().date_naive();
        let excluded: HashSet<NotePath> = cx
            .index
            .read()
            .map(|ix| ix.records().filter(|r| retired(&r.properties, today)).map(|r| r.path.clone()).collect())
            .unwrap_or_default();
        let neural = self.shared.semantic().is_some_and(|s| !s.embedder_name().contains("latent semantic"));
        let min_cos = if neural { contradict::CANDIDATE_MIN_NEURAL } else { contradict::CANDIDATE_MIN_LSA };
        for r in refs {
            let Some(v) = self.vector(&r.text) else { continue };
            if !p.raw && !excluded.contains(&r.path) {
                let cands: Vec<ClaimRef> = self.index.candidates(&r, &v, min_cos, &excluded).into_iter().map(|(c, _)| c.clone()).collect();
                for other in cands {
                    if !self.judge(cx, model.as_ref(), cap, &r, &other) {
                        self.queue.push_front(p.clone());
                        return Step::Idle(Duration::from_mins(10));
                    }
                }
            }
            self.index.add(r, v);
        }
        let n = self.index.len();
        let waiting = self.queue.len();
        self.status(|s| {
            s.claims = n;
            s.waiting = waiting;
        });
        Step::Continue
    }

    /// Judge a pair (cached); raise it if it disagrees. False when the cap is reached.
    fn judge(&mut self, cx: &JobCx, model: &dyn ChatModel, cap: u32, a: &ClaimRef, b: &ClaimRef) -> bool {
        let key = contradict::judge_key(model.name());
        if self.judges.as_ref().is_none_or(|(k, _)| *k != key) {
            self.judges = Some((key.clone(), JudgeCache::open(cx.vault.clone(), &key)));
        }
        let cached = self.judges.as_ref().and_then(|(_, c)| c.get(&a.id, &b.id).cloned());
        let j = if let Some(j) = cached {
            j
        } else {
            if !self.take_call(model, cap) {
                return false;
            }
            let (side_a, side_b) = match cx.index.read() {
                Ok(ix) => (side(&ix, a), side(&ix, b)),
                Err(_) => return true,
            };
            let req = contradict::judge_request(
                &JudgeSide { text: &a.text, context: &side_a.0, title: &side_a.1, modified: &side_a.2 },
                &JudgeSide { text: &b.text, context: &side_b.0, title: &side_b.1, modified: &side_b.2 },
            );
            let Ok((reply, _)) = chat_text(model, &req, &cx.cancel) else { return true };
            let Some(j) = contradict::parse_judgement(&reply) else { return true };
            if let Some((_, c)) = self.judges.as_mut() {
                let _ = c.put(&a.id, &b.id, j.clone());
            }
            j
        };
        if let Some(rel) = j.verdict() {
            let srcs = cx.index.read().map(|ix| (ix.source(&a.path).map(str::to_string), ix.source(&b.path).map(str::to_string)));
            if let Ok((Some(sa), Some(sb))) = srcs {
                let anchor_a: Anchor = anchor::make_anchor(&a.path, &sa, a.span.0..a.span.1);
                let anchor_b: Anchor = anchor::make_anchor(&b.path, &sb, b.span.0..b.span.1);
                let verdict = if rel == Relation::Contradiction { Verdict::Contradiction } else { Verdict::Tension };
                let item = InboxItem::Contradiction { a: anchor_a, b: anchor_b, verdict, confidence: j.confidence, rationale: j.why };
                if cx.agent.inbox().upsert(item).is_some() {
                    self.status(|s| s.contradictions += 1);
                    let at = chrono::Utc::now().timestamp_millis();
                    let ev = |c: &ClaimRef| Seen {
                        v: 1,
                        claim: c.id.clone(),
                        path: c.path.clone(),
                        event: Event::Contradicted,
                        at,
                        text: c.text.clone(),
                        other: serde_json::Map::new(),
                    };
                    let _ = timeline::append_seen(&cx.vault, &[ev(a), ev(b)]);
                }
            }
        }
        true
    }

    fn record_seen(&mut self, cx: &JobCx, p: &Passage, claims: &[Claim]) {
        let key = (p.path.clone(), p.start);
        let before = self.previous.insert(key, claims.to_vec());
        let Some(before) = before else {
            // First time this session: the cache's view is the baseline.
            return;
        };
        let (added, removed) = timeline::changes(&before, claims);
        let at = chrono::Utc::now().timestamp_millis();
        let mut events: Vec<Seen> = Vec::new();
        for (list, event) in [(added, Event::Added), (removed, Event::Removed)] {
            for c in list {
                events.push(Seen {
                    v: 1,
                    claim: c.id.clone(),
                    path: p.path.clone(),
                    event,
                    at,
                    text: c.text.clone(),
                    other: serde_json::Map::new(),
                });
            }
        }
        let _ = timeline::append_seen(&cx.vault, &events);
    }
}

/// (context sentence, title, modified date) of a claim for the judge.
fn side(ix: &quire_index::Index, c: &ClaimRef) -> (String, String, String) {
    let src = ix.source(&c.path).unwrap_or_default();
    let ctx_start = src[..c.span.0.min(src.len())].rfind("\n\n").map_or(0, |i| i + 2);
    let ctx_end = src[c.span.1.min(src.len())..].find("\n\n").map_or(src.len(), |i| c.span.1 + i);
    let ctx: String = src.get(ctx_start..ctx_end).unwrap_or_default().chars().take(800).collect();
    let title = ix.get(&c.path).map_or_else(|| c.path.stem().to_string(), |r| r.title.clone());
    let modified = ix
        .modified(&c.path)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    (ctx, title, modified)
}

impl Job for ClaimsJob {
    fn id(&self) -> &'static str {
        "claims"
    }

    fn budget(&self) -> Budget {
        let cap = self.shared.settings().claims_daily_cap;
        Budget { cpu_ms_per_min: 6_000, max_calls_per_day: cap, needs: Needs::Idle }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        if !self.shared.alive.load(Ordering::Acquire) {
            return Step::Done;
        }
        let settings = self.shared.settings();
        let Some(model) = self.shared.chat().filter(|_| settings.contradictions) else {
            self.status(|s| s.note = None);
            return Step::Idle(Duration::from_secs(5));
        };
        if model.name() != self.model_name {
            // Another model: its own cache; start over.
            self.model_name = model.name().to_string();
            *self = Self {
                shared: Arc::clone(&self.shared),
                model_name: self.model_name.clone(),
                cap: self.cap.clone(),
                ..Self::new(Arc::clone(&self.shared))
            };
        }
        let version = self.shared.semantic_version.load(Ordering::Acquire);
        if version != self.vectors_of {
            // A rebuilt semantic index: claim vectors are made again.
            self.vectors_of = version;
            self.index = ClaimIndex::default();
            self.done.clear();
            self.queue.clear();
        }
        if self.queue.is_empty() {
            self.refill(cx);
            if self.queue.is_empty() {
                return Step::Idle(Duration::from_secs(20));
            }
        }
        self.step_passage(cx, &model, settings.claims_daily_cap)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;
    use std::sync::atomic::AtomicBool;

    use quire_intel::chat::ScriptedChat;

    use super::*;

    fn claim_reply(text: &str, span: &str, subject: &str) -> String {
        serde_json::json!([{"text": text, "source_span": span, "subject": subject, "polarity": "positive"}]).to_string()
    }

    /// The scripted-model fixture of §6.2 WS-C: exactly the expected Inbox
    /// item, and no second request on a cache hit.
    #[test]
    fn finds_a_contradiction_once_and_caches() {
        let v = Vault::in_memory(&[
            ("Plan.md", "# Plan\n\nThe launch date is March 2027 for the new app.\n"),
            ("Update.md", "# Update\n\nThe launch date is June 2027 for the new app.\n"),
            // Other topics, so the built-in embedder learns which words matter.
            ("Bread.md", "# Bread\n\nSourdough bread needs flour, water and a slow rise.\n"),
            ("Garden.md", "# Garden\n\nTomatoes need sun, water and a stake in the garden.\n"),
            ("Cars.md", "# Cars\n\nThe car engine needs oil and a long road trip.\n"),
        ]);
        let svc = quire_index::IndexService::spawn();
        svc.load(v.clone());
        svc.flush_semantic();
        let agent = quire_agent::AgentStore::open(
            &v,
            quire_agent::DeviceInfo { id: "d".into(), name: "D".into() },
            Arc::new(quire_agent::SystemClock),
        );
        let cx = JobCx {
            vault: v,
            index: svc.handle(),
            agent: agent.clone(),
            chat: None,
            user_active: Arc::new(AtomicBool::new(false)),
            cancel: quire_intel::chat::CancelFlag::new(),
        };
        // Notes in path order: Bread, Cars, Garden (no claims), Plan, Update, then the judge.
        let script = ScriptedChat::texts([
            "[]".to_string(),
            "[]".to_string(),
            "[]".to_string(),
            claim_reply("The launch date is March 2027.", "The launch date is March 2027", "launch date"),
            claim_reply("The launch date is June 2027.", "The launch date is June 2027", "launch date"),
            r#"{"relation":"contradiction","confidence":0.9,"why":"The two notes give different launch dates."}"#.to_string(),
        ]);
        let model: Arc<ScriptedChat> = Arc::new(script);
        let shared = Arc::new(TrustShared::default());
        shared.set_chat(Some(Arc::clone(&model) as Arc<dyn ChatModel>));
        shared.set_semantic(svc.semantic(), svc.semantic_version());
        shared.set_settings(crate::settings::TrustSettings { contradictions: true, ..Default::default() });
        let mut job = ClaimsJob::new(Arc::clone(&shared));
        for _ in 0..20 {
            if matches!(job.step(&cx), Step::Idle(_)) && job.queue.is_empty() {
                break;
            }
        }
        assert_eq!(model.remaining(), 0, "five extractions and one judgement");
        let items: Vec<InboxItem> = agent.inbox().stored().into_iter().map(|s| s.item).collect();
        assert_eq!(items.len(), 1, "{items:?}");
        let InboxItem::Contradiction { a, b, verdict, confidence, rationale } = &items[0] else { panic!("{items:?}") };
        assert_eq!(*verdict, Verdict::Contradiction);
        assert!((confidence - 0.9).abs() < 1e-6);
        assert!(rationale.contains("different launch dates"));
        let mut quotes = [a.quote.as_str(), b.quote.as_str()];
        quotes.sort_unstable();
        assert_eq!(quotes, ["The launch date is June 2027 for the new app.", "The launch date is March 2027 for the new app."]);
        // Everything again from scratch: all from the caches, no request.
        let mut again = ClaimsJob::new(Arc::clone(&shared));
        let quiet = Arc::new(ScriptedChat::texts(Vec::<String>::new()));
        shared.set_chat(Some(Arc::clone(&quiet) as Arc<dyn ChatModel>));
        // Same model name: the scripted one is called "Scripted" too.
        for _ in 0..20 {
            if matches!(again.step(&cx), Step::Idle(_)) && again.queue.is_empty() {
                break;
            }
        }
        assert!(quiet.requests().is_empty(), "cache hits make no request");
        assert_eq!(agent.inbox().stored().len(), 1, "the same item, not a second one");
        // Edited away: resolved.
        svc.upsert("Update.md".into(), "# Update\n\nWe haven't picked a date.\n".into());
        svc.flush_semantic();
        shared.set_semantic(svc.semantic(), svc.semantic_version());
        let more = Arc::new(ScriptedChat::texts(["[]".to_string()]));
        shared.set_chat(Some(Arc::clone(&more) as Arc<dyn ChatModel>));
        for _ in 0..20 {
            if matches!(again.step(&cx), Step::Idle(_)) && again.queue.is_empty() {
                break;
            }
        }
        assert!(agent.inbox().stored().is_empty());
        let _ = RwLock::new(());
    }

    #[test]
    fn nothing_happens_while_off_and_raw_notes_are_never_the_subject() {
        let v = Vault::in_memory(&[("raw/a.md", "x")]);
        let agent = quire_agent::AgentStore::open(
            &v,
            quire_agent::DeviceInfo { id: "d".into(), name: "D".into() },
            Arc::new(quire_agent::SystemClock),
        );
        assert!(is_raw(&agent, &NotePath::new("raw/a.md")));
        assert!(!is_raw(&agent, &NotePath::new("rawness.md")));
        let shared = Arc::new(TrustShared::default());
        let model = Arc::new(ScriptedChat::texts(["[]"]));
        shared.set_chat(Some(Arc::clone(&model) as Arc<dyn ChatModel>));
        let cx = JobCx {
            vault: v.clone(),
            index: Arc::new(RwLock::new(quire_index::Index::new())),
            agent,
            chat: None,
            user_active: Arc::new(AtomicBool::new(false)),
            cancel: quire_intel::chat::CancelFlag::new(),
        };
        let mut job = ClaimsJob::new(Arc::clone(&shared));
        assert!(matches!(job.step(&cx), Step::Idle(_)));
        assert!(model.requests().is_empty(), "off by default");
        assert_eq!(estimate(&v, &[(NotePath::new("A.md"), "# A\nOne passage of several words here.\n".into())], "m"), (1, 1));
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let props = std::collections::BTreeMap::from([("superseded_by".to_string(), Value::Str("[[B]]".into()))]);
        assert!(retired(&props, today));
    }
}
