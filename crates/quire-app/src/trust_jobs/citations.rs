// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The citation check (design §C.3): drift and, when asked, verify.
//!
//! The job finds the notes that cite others (`quire:cite` footnotes), and
//! checks a note again whenever it, or a note it cites, changes. A quote
//! that drifted or was lost becomes a `CitationDrift` item in the Inbox;
//! one found again resolves it. "Check citations" also asks the AI model
//! whether each cited passage supports the sentence citing it (`no` raises
//! `VerifyFailed`); without a model only a weak word overlap is pointed out,
//! in the note and never in the Inbox.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use quire_agent::inbox::InboxItem;
use quire_intel::anchor::{self, Anchor, AnchorStatus, Citation, Support, WEAK_SUPPORT};
use quire_intel::chat::{ChatModel, chat_text};
use quire_vault::{ContentHash, NotePath, Vault};

use super::TrustShared;
use crate::jobs::{Budget, Job, JobCx, Needs, Step};

/// What the check found for one footnote.
#[derive(Debug, Clone, PartialEq)]
pub struct CiteCheck {
    pub citation: Citation,
    /// The cited note, when it resolves.
    pub target: Option<NotePath>,
    pub anchor: Option<Anchor>,
    pub status: AnchorStatus,
    /// What the cited lines say now (drifted), or the quote's text.
    pub now: Option<String>,
    /// Few words in common with the sentence citing it (offline hint).
    pub weak: bool,
    /// The model's verdict, after "Check citations".
    pub support: Option<(Support, String)>,
}

impl CiteCheck {
    /// Worth the person's attention in the note.
    pub fn flagged(&self) -> bool {
        !self.status.holds() || self.weak || matches!(self.support, Some((Support::No | Support::Partly, _)))
    }
}

/// History versions searched for an anchor's old text.
const HISTORY_LOOKUP: usize = 20;

/// The note at `version`, from its history.
pub fn old_text(vault: &Vault, path: &NotePath, version: &ContentHash) -> Option<String> {
    vault.history(path).iter().take(HISTORY_LOOKUP).filter_map(|s| vault.read_snapshot(s).ok()).find(|t| ContentHash::of(t) == *version)
}

/// Check every citation of `path` (its text `src`). `source` gives a
/// note's current text and `resolve` a wikilink's note.
pub fn check_note(
    vault: &Vault,
    path: &NotePath,
    src: &str,
    resolve: &dyn Fn(&str, &NotePath) -> Option<NotePath>,
    source: &dyn Fn(&NotePath) -> Option<String>,
    verify: Option<&dyn ChatModel>,
    cancel: &quire_intel::chat::CancelFlag,
) -> Vec<CiteCheck> {
    let mut out = Vec::new();
    for c in anchor::citations(src) {
        let target = resolve(c.note(), path);
        let Some((target, text)) = target.and_then(|t| source(&t).map(|s| (t, s))) else {
            out.push(CiteCheck {
                citation: c,
                target: None,
                anchor: None,
                status: AnchorStatus::Lost,
                now: None,
                weak: false,
                support: None,
            });
            continue;
        };
        let a = c.anchor(target.clone());
        let mut status = anchor::resolve(&a, &text, None);
        if !status.holds()
            && let Some(old) = old_text(vault, &target, &a.version)
        {
            status = anchor::resolve(&a, &text, Some(&old));
        }
        let now = status.lines(a.lines).map(|l| anchor::lines_text(&text, l));
        let span = if status.holds() { a.quote.clone() } else { now.clone().unwrap_or_default() };
        let weak = c.claim.as_deref().is_some_and(|claim| !span.is_empty() && anchor::lexical_support(claim, &span) < WEAK_SUPPORT);
        let support = match (verify, c.claim.as_deref()) {
            (Some(model), Some(claim)) if !span.is_empty() && !cancel.is_cancelled() => {
                chat_text(model, &anchor::verify_request(claim, &span), cancel).ok().and_then(|(reply, _)| anchor::parse_support(&reply))
            }
            _ => None,
        };
        out.push(CiteCheck { citation: c, target: Some(target), anchor: Some(a), status, now, weak, support });
    }
    out
}

/// Keep the Inbox in step with a note's checks.
pub fn sync_inbox(agent: &quire_agent::AgentStore, in_note: &NotePath, checks: &[CiteCheck], previous: &[CiteCheck]) {
    let inbox = agent.inbox();
    for c in checks {
        let Some(a) = &c.anchor else { continue };
        let drift = InboxItem::CitationDrift { in_note: in_note.clone(), anchor: a.clone(), status: c.status };
        if c.status.holds() {
            let _ = inbox.resolve(&drift.fingerprint());
        } else {
            inbox.upsert(drift);
        }
        let failed =
            InboxItem::VerifyFailed { in_note: in_note.clone(), anchor: a.clone(), claim: c.citation.claim.clone().unwrap_or_default() };
        match &c.support {
            Some((Support::No, _)) => {
                inbox.upsert(failed);
            }
            Some(_) => {
                let _ = inbox.resolve(&failed.fingerprint());
            }
            None => {}
        }
    }
    // Footnotes that went away take their findings with them.
    let live: HashSet<String> = checks.iter().filter_map(|c| c.anchor.as_ref()).map(|a| format!("{}#{}", a.path, a.quote)).collect();
    for p in previous {
        let Some(a) = &p.anchor else { continue };
        if live.contains(&format!("{}#{}", a.path, a.quote)) {
            continue;
        }
        let drift = InboxItem::CitationDrift { in_note: in_note.clone(), anchor: a.clone(), status: p.status };
        let _ = inbox.resolve(&drift.fingerprint());
        let failed =
            InboxItem::VerifyFailed { in_note: in_note.clone(), anchor: a.clone(), claim: p.citation.claim.clone().unwrap_or_default() };
        let _ = inbox.resolve(&failed.fingerprint());
    }
}

/// Notes scanned for footnotes per step.
const SCAN_CHUNK: usize = 400;

/// The drift check (`citations`).
pub struct CitationsJob {
    shared: Arc<TrustShared>,
    /// Paths still to scan for footnotes on the first run.
    scan: Option<Vec<NotePath>>,
    /// Cited note → notes citing it.
    cited_by: HashMap<NotePath, HashSet<NotePath>>,
    /// Notes with footnotes.
    citing: HashSet<NotePath>,
    queue: VecDeque<NotePath>,
}

impl CitationsJob {
    pub fn new(shared: Arc<TrustShared>) -> Self {
        Self { shared, scan: None, cited_by: HashMap::new(), citing: HashSet::new(), queue: VecDeque::new() }
    }

    fn enqueue(&mut self, p: &NotePath) {
        if !self.queue.contains(p) {
            self.queue.push_back(p.clone());
        }
    }

    fn note_citations(&mut self, path: &NotePath, src: &str, resolve: &dyn Fn(&str, &NotePath) -> Option<NotePath>) {
        for set in self.cited_by.values_mut() {
            set.remove(path);
        }
        if !src.contains("quire:cite") {
            self.citing.remove(path);
            return;
        }
        let cs = anchor::citations(src);
        if cs.is_empty() {
            self.citing.remove(path);
            return;
        }
        self.citing.insert(path.clone());
        for c in cs {
            if let Some(t) = resolve(c.note(), path) {
                self.cited_by.entry(t).or_default().insert(path.clone());
            }
        }
    }
}

impl Job for CitationsJob {
    fn id(&self) -> &'static str {
        "citations"
    }

    fn budget(&self) -> Budget {
        Budget { cpu_ms_per_min: 3_000, max_calls_per_day: 0, needs: Needs::Any }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        if !self.shared.alive.load(Ordering::Acquire) {
            return Step::Done;
        }
        let resolve = |target: &str, from: &NotePath| cx.index.read().ok().and_then(|ix| ix.resolve(target, Some(from)));
        // First run: find the notes that cite, once the index has loaded.
        if self.scan.is_none() {
            let paths = cx.index.read().map(|ix| ix.paths()).unwrap_or_default();
            if paths.is_empty() {
                return Step::Idle(Duration::from_secs(1));
            }
            self.scan = Some(paths);
        }
        let Some(scan) = self.scan.as_mut() else { return Step::Idle(Duration::from_secs(1)) };
        if !scan.is_empty() {
            let take: Vec<NotePath> = scan.drain(..scan.len().min(SCAN_CHUNK)).collect();
            let found: Vec<(NotePath, String)> = cx
                .index
                .read()
                .map(|ix| {
                    take.into_iter()
                        .filter_map(|p| ix.source(&p).filter(|s| s.contains("quire:cite")).map(|s| (p.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            for (p, s) in found {
                self.note_citations(&p, &s, &resolve);
                self.enqueue(&p);
            }
            return Step::Continue;
        }
        // Changes since last time: the note itself and the notes citing it.
        let changed: Vec<NotePath> = self.shared.drift_changed.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
        for p in changed {
            let src = cx.index.read().ok().and_then(|ix| ix.source(&p).map(str::to_string));
            if let Some(s) = src {
                self.note_citations(&p, &s, &resolve);
            } else {
                self.citing.remove(&p);
                if let Ok(mut m) = self.shared.citations.lock() {
                    m.remove(&p);
                }
            }
            if self.citing.contains(&p) {
                self.enqueue(&p);
            }
            for c in self.cited_by.get(&p).cloned().unwrap_or_default() {
                self.enqueue(&c);
            }
        }
        // "Check citations": this note now, with the model when there is one.
        let asked = self.shared.check_now.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
        let (path, verify) = match asked.into_iter().next() {
            Some(p) => (p, true),
            None => match self.queue.pop_front() {
                Some(p) => (p, false),
                None => return Step::Idle(Duration::from_millis(500)),
            },
        };
        let Some(src) = cx.index.read().ok().and_then(|ix| ix.source(&path).map(str::to_string)) else { return Step::Continue };
        let source = |p: &NotePath| cx.index.read().ok().and_then(|ix| ix.source(p).map(str::to_string));
        let model: Option<Arc<dyn ChatModel>> = if verify { self.shared.chat() } else { None };
        let checks = check_note(&cx.vault, &path, &src, &resolve, &source, model.as_deref(), &cx.cancel);
        let previous = self.shared.citations.lock().ok().and_then(|m| m.get(&path).cloned()).unwrap_or_default();
        // Keep earlier verdicts while the passage is unchanged.
        let checks: Vec<CiteCheck> = checks
            .into_iter()
            .map(|mut c| {
                if c.support.is_none()
                    && let Some(p) = previous
                        .iter()
                        .find(|p| p.citation.label == c.citation.label && p.now == c.now && p.citation.quote == c.citation.quote)
                {
                    c.support.clone_from(&p.support);
                }
                c
            })
            .collect();
        sync_inbox(&cx.agent, &path, &checks, &previous);
        let n = checks.len();
        if let Ok(mut m) = self.shared.citations.lock() {
            if n == 0 {
                m.remove(&path);
            } else {
                m.insert(path.clone(), checks);
            }
        }
        if verify && let Ok(mut done) = self.shared.checked.lock() {
            done.push((path, n, model.is_some()));
        }
        self.shared.bump();
        Step::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_intel::chat::ScriptedChat;

    fn setup(cited: &str) -> (Vault, String) {
        let v = Vault::in_memory(&[("Rope Notes.md", cited)]);
        let at = cited.find("Ropes make").unwrap();
        let end = cited[at..].find('.').unwrap() + at + 1;
        let a = anchor::make_anchor(&NotePath::new("Rope Notes.md"), cited, at..end);
        let answer = format!("# Answer\n\nUndo is cheap with ropes.[^1]\n\n{}\n", anchor::footnote("1", &a));
        (v, answer)
    }

    #[test]
    fn checks_drift_and_verifies_with_a_model() {
        let cited = "# Rope Notes\n\nRopes make undo cheap because subtrees are shared.\n";
        let (v, answer) = setup(cited);
        let path = NotePath::new("Answer.md");
        let resolve = |t: &str, _: &NotePath| (t == "Rope Notes").then(|| NotePath::new("Rope Notes.md"));
        let changed = cited.replace("subtrees are shared", "subtrees are mostly shared");
        let source = |p: &NotePath| (p.as_str() == "Rope Notes.md").then(|| changed.clone());
        let cancel = quire_intel::chat::CancelFlag::new();
        let checks = check_note(&v, &path, &answer, &resolve, &source, None, &cancel);
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].status, AnchorStatus::Drifted { .. }), "{:?}", checks[0].status);
        assert!(checks[0].now.as_deref().unwrap().contains("mostly"));
        assert!(!checks[0].weak);
        assert!(checks[0].flagged());
        let model = ScriptedChat::texts(["{\"supported\":\"no\",\"why\":\"different\"}"]);
        let same = |p: &NotePath| (p.as_str() == "Rope Notes.md").then(|| cited.to_string());
        let checks = check_note(&v, &path, &answer, &resolve, &same, Some(&model), &cancel);
        assert_eq!(checks[0].status, AnchorStatus::Exact);
        assert_eq!(checks[0].support, Some((Support::No, "different".into())));
        assert!(model.requests()[0].messages[0].text().contains("Undo is cheap with ropes."));
        // Into the Inbox and out again.
        let agent = quire_agent::AgentStore::open(
            &v,
            quire_agent::DeviceInfo { id: "d".into(), name: "D".into() },
            Arc::new(quire_agent::SystemClock),
        );
        sync_inbox(&agent, &path, &checks, &[]);
        assert!(matches!(agent.inbox().stored()[0].item, InboxItem::VerifyFailed { .. }));
        let drifted = check_note(&v, &path, &answer, &resolve, &source, None, &cancel);
        sync_inbox(&agent, &path, &drifted, &checks);
        assert_eq!(agent.inbox().stored().len(), 2, "drift added, the verdict stays until checked again");
        let ok: Vec<CiteCheck> = checks
            .iter()
            .cloned()
            .map(|mut c| {
                c.support = Some((Support::Yes, String::new()));
                c
            })
            .collect();
        sync_inbox(&agent, &path, &ok, &drifted);
        assert!(agent.inbox().stored().is_empty(), "both resolved");
        // A note that's gone: lost.
        let gone = |_: &NotePath| None;
        assert_eq!(check_note(&v, &path, &answer, &resolve, &gone, None, &cancel)[0].status, AnchorStatus::Lost);
    }

    #[test]
    fn old_versions_come_from_history() {
        let v = Vault::in_memory(&[("A.md", "old text\n")]);
        let p = NotePath::new("A.md");
        v.snapshot(&p, "old text\n", 1_000, true).unwrap();
        assert_eq!(old_text(&v, &p, &ContentHash::of("old text\n")).as_deref(), Some("old text\n"));
        assert_eq!(old_text(&v, &p, &ContentHash::of("other")), None);
    }
}
