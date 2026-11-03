// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Background work with budgets (design §9).
//!
//! A [`Job`] does its work in small [`Job::step`]s (at most ~20 ms each,
//! holding the index lock for at most ~2 ms). One worker thread, started
//! the first time a job is added, runs the steps one at a time. Jobs pause
//! while the person types or a dialog is open, a `Needs::Idle` job waits
//! for a minute of quiet, and each job's CPU use per minute is capped by its
//! [`Budget`]. Statuses are shown in Settings → AI → Background work.
//!
//! The worker runs at normal priority: lowering it needs platform calls
//! that aren't possible without `unsafe`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use quire_agent::AgentStore;
use quire_index::Index;
use quire_intel::chat::{CancelFlag, ChatModel};
use quire_vault::Vault;

/// How long without activity counts as idle for `Needs::Idle` jobs.
pub const IDLE_AFTER: Duration = Duration::from_mins(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// Runs whenever nobody is typing.
    Any,
    /// Runs only after a minute without typing (expensive work: claims, OCR).
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// CPU time per minute the runner allows this job.
    pub cpu_ms_per_min: u32,
    /// Model or network calls per day; jobs count them with [`DailyCap`].
    pub max_calls_per_day: u32,
    pub needs: Needs,
}

impl Default for Budget {
    fn default() -> Self {
        Self { cpu_ms_per_min: 3_000, max_calls_per_day: 0, needs: Needs::Any }
    }
}

/// What a step asks for next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// More to do now.
    Continue,
    /// Nothing to do for a while.
    Idle(Duration),
    /// Finished; the job is dropped.
    Done,
}

/// What a job may use. Cloned for each step.
#[derive(Clone)]
pub struct JobCx {
    pub vault: Vault,
    pub index: Arc<RwLock<Index>>,
    pub agent: AgentStore,
    /// The configured chat model, when there is one.
    pub chat: Option<Arc<dyn ChatModel>>,
    /// True while the person types (jobs that loop inside a step check it).
    pub user_active: Arc<AtomicBool>,
    /// Set when the vault closes or the app quits.
    pub cancel: CancelFlag,
}

pub trait Job: Send {
    /// Stable id: `lint`, `pdf-text`, `claims`.
    fn id(&self) -> &'static str;
    fn budget(&self) -> Budget;
    fn step(&mut self, cx: &JobCx) -> Step;
}

/// A per-day call counter for jobs with `max_calls_per_day`.
#[derive(Debug, Clone, Default)]
pub struct DailyCap {
    day: i64,
    used: u32,
}

impl DailyCap {
    /// Take one call if today's cap allows it. `cap == 0` means no limit.
    pub fn try_take(&mut self, cap: u32, now_ms: i64) -> bool {
        let day = now_ms.div_euclid(86_400_000);
        if day != self.day {
            self.day = day;
            self.used = 0;
        }
        if cap > 0 && self.used >= cap {
            return false;
        }
        self.used += 1;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Waiting,
    Running,
    /// Why it isn't running: "you're typing", "waiting for idle", "over budget".
    Paused(&'static str),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStatus {
    pub id: &'static str,
    pub state: JobState,
    pub steps: u64,
    pub last_step: Option<Instant>,
}

struct Entry {
    job: Option<Box<dyn Job>>,
    budget: Budget,
    status: JobStatus,
    next_at: Instant,
    minute_start: Instant,
    used_ms: u64,
}

struct State {
    jobs: Vec<Entry>,
    cx: Option<JobCx>,
    idle_since: Instant,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    typing: Arc<AtomicBool>,
    modal: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Runs jobs on one background thread.
pub struct JobRunner {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Default for JobRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRunner {
    pub fn new() -> Self {
        let shared = Shared {
            state: Mutex::new(State { jobs: Vec::new(), cx: None, idle_since: Instant::now() }),
            wake: Condvar::new(),
            typing: Arc::new(AtomicBool::new(false)),
            modal: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        };
        Self { shared: Arc::new(shared), worker: None }
    }

    /// What jobs work with; `None` while no vault is open (jobs wait). The
    /// previous context is cancelled.
    pub fn set_context(&self, cx: Option<JobCx>) {
        let mut st = self.shared.lock();
        if let Some(old) = st.cx.take() {
            old.cancel.cancel();
        }
        st.cx = cx;
        drop(st);
        self.shared.wake.notify_all();
    }

    /// Add a job; the worker thread starts with the first one.
    pub fn add(&mut self, job: Box<dyn Job>) {
        let now = Instant::now();
        let budget = job.budget();
        let status = JobStatus { id: job.id(), state: JobState::Waiting, steps: 0, last_step: None };
        self.shared.lock().jobs.push(Entry { job: Some(job), budget, status, next_at: now, minute_start: now, used_ms: 0 });
        if self.worker.is_none() {
            let shared = Arc::clone(&self.shared);
            self.worker = std::thread::Builder::new().name("quire-jobs".into()).spawn(move || work(&shared)).ok();
        }
        self.shared.wake.notify_all();
    }

    /// The person is typing (fed from `Workspace::tick`).
    pub fn set_user_active(&self, active: bool) {
        let was = self.shared.typing.swap(active, Ordering::AcqRel);
        if was && !active {
            self.shared.lock().idle_since = Instant::now();
            self.shared.wake.notify_all();
        }
    }

    /// A dialog is open: jobs wait, like while typing.
    pub fn set_modal_open(&self, open: bool) {
        if self.shared.modal.swap(open, Ordering::AcqRel) && !open {
            self.shared.lock().idle_since = Instant::now();
            self.shared.wake.notify_all();
        }
    }

    /// The flag jobs see in [`JobCx::user_active`].
    pub fn user_active_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.typing)
    }

    pub fn statuses(&self) -> Vec<JobStatus> {
        self.shared.lock().jobs.iter().map(|e| e.status.clone()).collect()
    }
}

impl Drop for JobRunner {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.set_context(None);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Why `e` can't run now, if it can't.
fn blocked(e: &Entry, now: Instant, active: Option<&'static str>, idle_long: bool) -> Option<&'static str> {
    if let Some(why) = active {
        return Some(why);
    }
    if e.budget.needs == Needs::Idle && !idle_long {
        return Some("waiting until you're idle");
    }
    let fresh_minute = now.duration_since(e.minute_start) >= Duration::from_mins(1);
    if !fresh_minute && e.used_ms >= u64::from(e.budget.cpu_ms_per_min) {
        return Some("over its time budget");
    }
    None
}

fn work(shared: &Shared) {
    let tick = Duration::from_millis(250);
    loop {
        let mut st = shared.lock();
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let Some(cx) = st.cx.clone() else {
            drop(shared.wake.wait_timeout(st, tick));
            continue;
        };
        let now = Instant::now();
        let active = if shared.typing.load(Ordering::Acquire) {
            Some("you're typing")
        } else if shared.modal.load(Ordering::Acquire) {
            Some("a dialog is open")
        } else {
            None
        };
        let idle_long = active.is_none() && now.duration_since(st.idle_since) >= IDLE_AFTER;
        let mut pick = None;
        for (i, e) in st.jobs.iter_mut().enumerate() {
            if e.job.is_none() || e.status.state == JobState::Done || e.next_at > now {
                continue;
            }
            match blocked(e, now, active, idle_long) {
                Some(why) => e.status.state = JobState::Paused(why),
                None if pick.is_none() => pick = Some(i),
                None => e.status.state = JobState::Waiting,
            }
        }
        let Some(i) = pick else {
            drop(shared.wake.wait_timeout(st, tick));
            continue;
        };
        let mut job = st.jobs[i].job.take();
        st.jobs[i].status.state = JobState::Running;
        drop(st);
        let started = Instant::now();
        let step = job.as_mut().map_or(Step::Done, |j| j.step(&cx));
        let spent = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut st = shared.lock();
        let e = &mut st.jobs[i];
        if started.duration_since(e.minute_start) >= Duration::from_mins(1) {
            e.minute_start = started;
            e.used_ms = 0;
        }
        e.used_ms += spent;
        e.status.steps += 1;
        e.status.last_step = Some(Instant::now());
        match step {
            Step::Continue => {
                e.next_at = Instant::now();
                e.job = job;
                e.status.state = JobState::Waiting;
            }
            Step::Idle(d) => {
                e.next_at = Instant::now() + d;
                e.job = job;
                e.status.state = JobState::Waiting;
            }
            Step::Done => e.status.state = JobState::Done,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    struct Counter {
        n: Arc<AtomicU32>,
        stop_at: u32,
    }

    impl Job for Counter {
        fn id(&self) -> &'static str {
            "counter"
        }
        fn budget(&self) -> Budget {
            Budget::default()
        }
        fn step(&mut self, _cx: &JobCx) -> Step {
            let n = self.n.fetch_add(1, Ordering::SeqCst) + 1;
            if n >= self.stop_at { Step::Done } else { Step::Idle(Duration::from_millis(5)) }
        }
    }

    fn wait_until(f: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn cx(runner: &JobRunner) -> JobCx {
        let vault = Vault::in_memory(&[]);
        let index = quire_index::IndexService::spawn();
        let agent = AgentStore::open(&vault, quire_agent::DeviceInfo::generate("t"), Arc::new(quire_agent::SystemClock));
        JobCx { vault, index: index.handle(), agent, chat: None, user_active: runner.user_active_flag(), cancel: CancelFlag::new() }
    }

    #[test]
    fn runs_steps_pauses_for_typing_and_finishes() {
        let mut runner = JobRunner::new();
        let n = Arc::new(AtomicU32::new(0));
        runner.add(Box::new(Counter { n: Arc::clone(&n), stop_at: 1_000_000 }));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(n.load(Ordering::SeqCst), 0, "no vault, no work");
        runner.set_context(Some(cx(&runner)));
        assert!(wait_until(|| n.load(Ordering::SeqCst) >= 3));
        runner.set_user_active(true);
        assert!(wait_until(|| runner.statuses()[0].state == JobState::Paused("you're typing")));
        let paused_at = n.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(n.load(Ordering::SeqCst), paused_at, "nothing runs while typing");
        runner.set_user_active(false);
        assert!(wait_until(|| n.load(Ordering::SeqCst) > paused_at));
        let done = Arc::new(AtomicU32::new(0));
        runner.add(Box::new(Counter { n: Arc::clone(&done), stop_at: 2 }));
        assert!(wait_until(|| runner.statuses()[1].state == JobState::Done));
        assert_eq!(done.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn daily_caps_reset_each_day() {
        let mut cap = DailyCap::default();
        assert!(cap.try_take(2, 0) && cap.try_take(2, 1));
        assert!(!cap.try_take(2, 2));
        assert!(cap.try_take(2, 86_400_000), "a new day");
        let mut open = DailyCap::default();
        assert!((0..100).all(|i| open.try_take(0, i)), "0 = no limit");
    }
}
