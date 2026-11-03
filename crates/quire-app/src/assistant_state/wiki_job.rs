// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The wiki's upkeep as a background job (design §D.7, §9):
//!
//! * `index`: 10 s after a page in `wiki/` changes, `wiki/index.md` is
//!   regenerated between its markers and written as `job:index` (waiting
//!   while the person has it open with unsaved edits);
//! * `log`: agent runs that changed `wiki/` (once quiet for a minute) and
//!   batches of accepted suggestions in `wiki/` get one line each in
//!   `wiki/log.md`, written as `job:log`. Runs that wrote the log
//!   themselves (ingest) are skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quire_agent::{Actor, ActorKind, Change, ChangeSet, LedgerQuery, Op};
use quire_vault::NotePath;

use super::wiki;
use crate::jobs::{Budget, Job, JobCx, Needs, Step};

const INDEX_DELAY: Duration = Duration::from_secs(10);
const RUN_QUIET_MS: i64 = 60_000;

#[derive(Default)]
struct Shared {
    dirty_at: Mutex<Option<Instant>>,
    index_open_dirty: AtomicBool,
}

/// The workspace's side: tell the job about changes.
#[derive(Clone)]
pub(crate) struct Handle(Arc<Shared>);

impl Handle {
    pub(crate) fn touch(&self) {
        let mut d = self.0.dirty_at.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *d = Some(Instant::now());
    }

    pub(crate) fn set_index_open_dirty(&self, v: bool) {
        self.0.index_open_dirty.store(v, Ordering::Release);
    }
}

/// A run being watched until it goes quiet.
struct RunSeen {
    last: i64,
    label: String,
    paths: BTreeSet<String>,
    logged_itself: bool,
}

pub(crate) struct WikiJob {
    shared: Arc<Shared>,
    device: String,
    /// Ledger read up to here (Unix ms).
    seen_until: Option<i64>,
    /// Entries already handled at exactly `seen_until` (several can share a millisecond).
    seen_at_edge: BTreeSet<String>,
    runs: BTreeMap<String, RunSeen>,
}

impl WikiJob {
    pub(crate) fn new(device: String) -> (Self, Handle) {
        let shared = Arc::new(Shared::default());
        (
            Self { shared: Arc::clone(&shared), device, seen_until: None, seen_at_edge: BTreeSet::new(), runs: BTreeMap::new() },
            Handle(shared),
        )
    }

    fn index(&self, cx: &JobCx) {
        let due = {
            let mut d = self.shared.dirty_at.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match *d {
                Some(t) if t.elapsed() >= INDEX_DELAY && !self.shared.index_open_dirty.load(Ordering::Acquire) => {
                    *d = None;
                    true
                }
                _ => false,
            }
        };
        if !due {
            return;
        }
        let Ok(all) = cx.vault.notes() else { return };
        if !all.iter().any(wiki::is_page) {
            return;
        }
        let pages: Vec<(NotePath, String)> =
            all.iter().filter(|p| wiki::is_page(p)).map(|p| (p.clone(), cx.vault.read(p).unwrap_or_default())).collect();
        let path = NotePath::from(wiki::INDEX);
        let old = cx.vault.read(&path).ok();
        if let Some(next) = wiki::regenerate(old.as_deref(), &pages, &all) {
            let change = if old.is_some() { Change::Edit { path, new_text: next } } else { Change::Create { path, text: next } };
            let _ = cx.agent.submit(&Actor::job("index", &self.device), ChangeSet::one(change).titled("Update the wiki index"));
        }
    }

    fn log(&mut self, cx: &JobCx) {
        let now = cx.agent.now_ms();
        // Changes from before the vault was opened aren't logged.
        let since = *self.seen_until.get_or_insert(now - 1);
        let entries = cx.agent.log(&LedgerQuery { since: Some(since), ..LedgerQuery::default() });
        let mut accepted: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
        for e in &entries {
            let key = format!(
                "{}|{}|{:?}|{}",
                e.actor.id,
                e.path.as_ref().map(NotePath::as_str).unwrap_or_default(),
                e.op,
                e.after.as_ref().map(ToString::to_string).unwrap_or_default()
            );
            if e.at < since || (e.at == since && self.seen_at_edge.contains(&key)) {
                continue;
            }
            let edge = self.seen_until.unwrap_or(since);
            if e.at > edge {
                self.seen_until = Some(e.at);
                self.seen_at_edge.clear();
            }
            if e.at == self.seen_until.unwrap_or(since) {
                self.seen_at_edge.insert(key);
            }
            let Some(path) = e.path.as_ref().map(|p| p.as_str().to_string()) else { continue };
            if e.actor.kind == ActorKind::Job {
                continue;
            }
            if e.op == Op::Accept && path.starts_with(wiki::WIKI) && path != wiki::LOG && path != wiki::INDEX {
                let key = e.batch.clone().or_else(|| e.proposal.as_ref().map(ToString::to_string)).unwrap_or_else(|| e.at.to_string());
                let from =
                    e.from.as_deref().map_or_else(|| "an agent".to_string(), |id| id.split_once(':').map_or(id, |(_, n)| n).to_string());
                accepted.entry(key).or_insert_with(|| (from, BTreeSet::new())).1.insert(path);
                continue;
            }
            let (Some(run), ActorKind::Agent) = (&e.run, e.actor.kind) else { continue };
            if !matches!(e.op, Op::Write | Op::Create | Op::Rename | Op::Delete) || !path.starts_with(wiki::WIKI) {
                continue;
            }
            let r = self.runs.entry(run.to_string()).or_insert_with(|| RunSeen {
                last: e.at,
                label: e.actor.label.clone(),
                paths: BTreeSet::new(),
                logged_itself: false,
            });
            r.last = r.last.max(e.at);
            if path == wiki::LOG {
                r.logged_itself = true;
            } else if path != wiki::INDEX {
                r.paths.insert(path);
            }
        }
        let mut lines = Vec::new();
        let at = crate::chrono_fmt(now);
        for (from, paths) in accepted.into_values() {
            let n = paths.len();
            lines.push(wiki::log_line(&at, "accepted", &format!("{n} suggestion{} from {from}", if n == 1 { "" } else { "s" }), "", None));
        }
        let done: Vec<String> = self.runs.iter().filter(|(_, r)| now - r.last >= RUN_QUIET_MS).map(|(k, _)| k.clone()).collect();
        for k in done {
            let Some(r) = self.runs.remove(&k) else { continue };
            if r.logged_itself || r.paths.is_empty() {
                continue;
            }
            let n = r.paths.len();
            lines.push(wiki::log_line(&at, "agent run", &format!("{n} page{} changed", if n == 1 { "" } else { "s" }), &r.label, Some(&k)));
        }
        if lines.is_empty() {
            return;
        }
        let path = NotePath::from(wiki::LOG);
        let old = cx.vault.read(&path).ok();
        let mut text = old.clone();
        for l in &lines {
            text = Some(wiki::append_log(text.as_deref(), l));
        }
        let Some(text) = text else { return };
        let change = if old.is_some() { Change::Edit { path, new_text: text } } else { Change::Create { path, text } };
        let _ = cx.agent.submit(&Actor::job("log", &self.device), ChangeSet::one(change).titled("Add to the wiki log"));
    }
}

impl Job for WikiJob {
    fn id(&self) -> &'static str {
        "wiki"
    }

    fn budget(&self) -> Budget {
        Budget { cpu_ms_per_min: 2_000, max_calls_per_day: 0, needs: Needs::Any }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        self.index(cx);
        self.log(cx);
        Step::Idle(Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::vault::Vault;
    use crate::workspace::Workspace;

    fn cx(ws: &Workspace) -> JobCx {
        JobCx {
            vault: ws.vault().unwrap().clone(),
            index: ws.index().handle(),
            agent: ws.agent_store().unwrap().clone(),
            chat: None,
            user_active: Arc::default(),
            cancel: quire_intel::chat::CancelFlag::new(),
        }
    }

    #[test]
    fn the_index_follows_wiki_pages_and_runs_are_logged() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[
            ("wiki/concepts/Ropes.md", "---\ntype: concept\n---\nRopes are trees."),
            ("Mine.md", "# Mine\n"),
        ]));
        let cx = cx(&ws);
        let (mut job, h) = WikiJob::new("d_test".into());
        job.step(&cx);
        assert!(!cx.vault.exists(&wiki::INDEX.into()), "nothing until a page changes");
        h.touch();
        *h.0.dirty_at.lock().unwrap() = Some(Instant::now().checked_sub(INDEX_DELAY).unwrap());
        h.set_index_open_dirty(true);
        job.step(&cx);
        assert!(!cx.vault.exists(&wiki::INDEX.into()), "waits while the index is open with unsaved edits");
        h.set_index_open_dirty(false);
        job.step(&cx);
        let index = cx.vault.read(&wiki::INDEX.into()).unwrap();
        assert!(index.contains("- [[Ropes]] — Ropes are trees."), "{index}");

        // An agent run in wiki/ gets one log line once it has been quiet.
        let agent = Actor::agent("mcp:claude-code", "Claude Code", quire_agent::Via::McpStdio, "d_test");
        let run = quire_agent::RunId::new(cx.agent.now_ms());
        let cs = ChangeSet { run: Some(run), ..ChangeSet::one(Change::Create { path: "wiki/Gap.md".into(), text: "Gaps.".into() }) };
        cx.agent.submit(&agent, cs).unwrap();
        job.step(&cx);
        assert!(!cx.vault.exists(&wiki::LOG.into()), "not while the run may go on");
        for r in job.runs.values_mut() {
            r.last -= RUN_QUIET_MS;
        }
        job.step(&cx);
        let log = cx.vault.read(&wiki::LOG.into()).unwrap();
        assert!(log.contains("· agent run · 1 page changed · Claude Code (run "), "{log}");
    }
}
