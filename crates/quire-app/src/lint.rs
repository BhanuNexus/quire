// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vault lint job (design §A.8, §9).
//!
//! It runs 30 seconds after the notes stop changing (or at once for "Check
//! the vault"), reads the notes from the index a few at a time, and keeps
//! the inbox's Checks in step: new findings are added, fixed ones removed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quire_agent::inbox::InboxItem;
use quire_agent::lint::{Finding, Level, LintInput, LintNote, LintPrefs, lint};
use quire_vault::NotePath;

use crate::jobs::{Budget, Job, JobCx, Needs, Step};

/// Quiet time after a change before checking.
pub const DEBOUNCE: Duration = Duration::from_secs(30);
/// Notes read and parsed per step (each step stays well under 20 ms).
const CHUNK: usize = 40;

/// Shared between the job and the app.
#[derive(Default)]
pub struct LintShared {
    /// Bumped by every change to the notes.
    pub changes: AtomicU64,
    /// Check now, without waiting.
    pub now: AtomicBool,
    pub last: Mutex<Option<LintRun>>,
}

/// What the last run found.
#[derive(Debug, Clone)]
pub struct LintRun {
    pub at: Instant,
    pub findings: usize,
    pub notes: usize,
    pub took: Duration,
}

enum Phase {
    Waiting,
    Reading { paths: Vec<NotePath>, notes: Vec<LintNote>, started: Instant },
}

pub struct LintJob {
    shared: Arc<LintShared>,
    seen: u64,
    changed_at: Option<Instant>,
    phase: Phase,
}

impl LintJob {
    pub fn new(shared: Arc<LintShared>) -> Self {
        // The first check runs after the usual quiet time, not during startup.
        Self { shared, seen: 0, changed_at: Some(Instant::now()), phase: Phase::Waiting }
    }
}

impl Job for LintJob {
    fn id(&self) -> &'static str {
        "lint"
    }

    fn budget(&self) -> Budget {
        Budget { cpu_ms_per_min: 3_000, max_calls_per_day: 0, needs: Needs::Any }
    }

    fn step(&mut self, cx: &JobCx) -> Step {
        match &mut self.phase {
            Phase::Waiting => {
                let n = self.shared.changes.load(Ordering::Acquire);
                if n != self.seen {
                    self.seen = n;
                    self.changed_at = Some(Instant::now());
                }
                let now = self.shared.now.swap(false, Ordering::AcqRel);
                let due = self.changed_at.is_some_and(|t| t.elapsed() >= DEBOUNCE);
                if !now && !due {
                    let wait = self.changed_at.map_or(Duration::from_secs(5), |t| DEBOUNCE.saturating_sub(t.elapsed()));
                    return Step::Idle(wait.clamp(Duration::from_millis(250), Duration::from_secs(5)));
                }
                self.changed_at = None;
                let paths = cx.index.read().map(|ix| ix.paths()).unwrap_or_default();
                self.phase = Phase::Reading { paths, notes: Vec::new(), started: Instant::now() };
                Step::Continue
            }
            Phase::Reading { paths, notes, started } => {
                if !paths.is_empty() {
                    let take: Vec<NotePath> = paths.drain(..paths.len().min(CHUNK)).collect();
                    // Copy the text under the lock; parse outside it.
                    let sources: Vec<(NotePath, String)> = cx
                        .index
                        .read()
                        .map(|ix| take.into_iter().filter_map(|p| Some((p.clone(), ix.source(&p)?.to_string()))).collect())
                        .unwrap_or_default();
                    notes.extend(sources.into_iter().map(|(p, t)| LintNote::from_source(p, t)));
                    return Step::Continue;
                }
                let notes = std::mem::take(notes);
                let started = *started;
                self.phase = Phase::Waiting;
                let findings = run_checks(cx, &notes);
                sync_inbox(cx, &findings);
                if let Ok(mut l) = self.shared.last.lock() {
                    *l = Some(LintRun { at: Instant::now(), findings: findings.len(), notes: notes.len(), took: started.elapsed() });
                }
                Step::Idle(Duration::from_secs(1))
            }
        }
    }
}

/// The vault's `[lint]` preferences.
pub fn prefs(vault: &quire_vault::Vault) -> LintPrefs {
    quire_vault::VaultPrefs::load(vault).section::<LintPrefs>("lint")
}

/// Run the checks over `notes` (the CLI and the job share this).
pub fn run_checks(cx: &JobCx, notes: &[LintNote]) -> Vec<Finding> {
    check_vault(&cx.vault, &cx.agent, notes)
}

pub fn check_vault(vault: &quire_vault::Vault, agent: &quire_agent::AgentStore, notes: &[LintNote]) -> Vec<Finding> {
    let mut files: Vec<NotePath> = vault.storage().list().unwrap_or_default().into_iter().filter(|p| !p.is_markdown()).collect();
    // 0.12: a link to a note of a locked folder isn't broken. Only its name
    // is used (names aren't secret); the lint never reads it.
    let locked: Vec<NotePath> = files
        .iter()
        .filter_map(quire_vault::lock::plain_name)
        .filter(|p| p.is_markdown() && vault.locked_folder(p).is_some())
        .map(|p| NotePath::new(p.without_ext()))
        .collect();
    files.extend(locked);
    let policy = agent.policy();
    let contracts = agent.contracts();
    let prefs = prefs(vault);
    lint(&LintInput {
        notes,
        files: &files,
        policy: &policy,
        contracts: &contracts,
        prefs: &prefs,
        daily_folder: &vault.config.daily_folder,
        templates_folder: &vault.config.templates_folder,
    })
}

/// Add new findings to the inbox and remove the ones that are fixed.
fn sync_inbox(cx: &JobCx, findings: &[Finding]) {
    let inbox = cx.agent.inbox();
    let mut live = HashSet::new();
    for f in findings.iter().filter(|f| f.level != Level::Off) {
        let item = f.to_item();
        live.insert(item.fingerprint());
        inbox.upsert(item);
    }
    for s in inbox.stored() {
        if matches!(s.item, InboxItem::Lint { .. }) && !live.contains(&s.fingerprint) {
            let _ = inbox.resolve(&s.fingerprint);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use quire_agent::{AgentStore, DeviceInfo, SystemClock};
    use quire_vault::Vault;

    use super::*;

    #[test]
    fn the_job_checks_in_steps_and_keeps_the_inbox_in_step() {
        let v = Vault::in_memory(&[("A.md", "# A\n[[Missing]]\n"), ("B.md", "# B\n[[A]]\n")]);
        let mut ix = quire_index::Index::new();
        for p in v.notes().unwrap() {
            ix.upsert(p.clone(), &v.read(&p).unwrap());
        }
        let agent = AgentStore::open(&v, DeviceInfo { id: "d".into(), name: "D".into() }, Arc::new(SystemClock));
        let cx = JobCx {
            vault: v,
            index: Arc::new(RwLock::new(ix)),
            agent: agent.clone(),
            chat: None,
            user_active: Arc::new(AtomicBool::new(false)),
            cancel: quire_intel::chat::CancelFlag::new(),
        };
        let shared = Arc::new(LintShared::default());
        let mut job = LintJob::new(Arc::clone(&shared));
        assert!(matches!(job.step(&cx), Step::Idle(_)), "waits for quiet first");
        shared.now.store(true, Ordering::Release);
        while matches!(job.step(&cx), Step::Continue) {}
        let stored = agent.inbox().stored();
        assert_eq!(stored.len(), 1);
        assert!(matches!(&stored[0].item, InboxItem::Lint { path, .. } if path.as_str() == "A.md"));
        assert_eq!(shared.last.lock().unwrap().as_ref().unwrap().findings, 1);
        // Fixed: the finding goes away on the next run.
        if let Ok(mut ix) = cx.index.write() {
            ix.upsert("A.md".into(), "# A\n[[B]]\n");
        }
        shared.now.store(true, Ordering::Release);
        while matches!(job.step(&cx), Step::Continue) {}
        assert!(agent.inbox().stored().is_empty());
    }
}
