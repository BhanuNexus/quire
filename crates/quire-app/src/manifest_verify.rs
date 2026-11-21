// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vault manifest's verify pass, run by the workspace about once a day
//! when the person is idle (`docs/design/0.14-architecture.md` §2: tools can
//! keep a file's size and time while changing its bytes, and only reading
//! every note finds that).
//!
//! It reads the Markdown notes only (`Manifest::verify_where`); what it finds
//! reaches the index through the manifest's subscribers. When it last ran is
//! kept in this device's state folder for the vault, never in a cache (a
//! deleted cache must not make it run at once, nor a copied one stop it).

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::workspace::Workspace;

/// At most one verify a day.
pub(crate) const EVERY_MS: i64 = 86_400_000;
/// Only after this long without input or typing, until it is two days
/// late: then whatever the person does (as sync's own rule).
pub(crate) const IDLE: Duration = Duration::from_mins(5);
/// After a verify that failed (a listing error, a disk gone), wait this
/// long before trying again.
pub(crate) const RETRY: Duration = Duration::from_hours(1);
/// The file in the vault's state folder holding the last run, Unix ms.
const FILE: &str = "manifest-verify";
/// The file holding when this device first saw the vault with no verify
/// recorded, Unix ms.
const FIRST_FILE: &str = "manifest-verify-first-seen";

/// The workspace's part.
#[derive(Default)]
pub(crate) struct VerifyState {
    /// When the vault's deferred start ran (idle is measured from it until
    /// the first input).
    pub(crate) opened: Option<Instant>,
    /// The vault's state folder on this device.
    pub(crate) dir: Option<PathBuf>,
    /// The last run that finished, Unix ms, as read from `dir` or set here.
    pub(crate) last_ms: Option<i64>,
    /// When this device first saw the vault with no verify recorded, Unix
    /// ms (kept in `dir`): the two-days-late rule counts from it until the
    /// first verify.
    pub(crate) first_ms: Option<i64>,
    /// The verify in progress, with when it started.
    running: Option<(JoinHandle<bool>, i64)>,
    /// No new try before this (the last one failed).
    retry_at: Option<Instant>,
}

impl VerifyState {
    /// A verify that started at `started` ended: done for a day only if it
    /// read the notes (`ok`); otherwise tried again after [`RETRY`]. Gives
    /// whether to record the run.
    pub(crate) fn finished(&mut self, ok: bool, started: i64, now: Instant) -> bool {
        if ok {
            self.last_ms = Some(started);
            self.retry_at = None;
        } else {
            self.retry_at = Some(now + RETRY);
        }
        ok
    }
}

/// Whether a verify is due: the last a day ago or more (or in the future: a
/// clock set back) and idle long enough, or two days ago or more, idle or
/// not. Never run here: due when idle, or two days after `first_ms` (when
/// this device first saw the vault) whatever the person does.
pub(crate) fn due(last_ms: Option<i64>, first_ms: Option<i64>, now_ms: i64, idle_for: Duration) -> bool {
    let idle = idle_for >= IDLE;
    match last_ms.filter(|l| *l <= now_ms) {
        Some(l) => (idle && now_ms - l >= EVERY_MS) || now_ms - l >= 2 * EVERY_MS,
        None => idle || first_ms.filter(|f| *f <= now_ms).is_some_and(|f| now_ms - f >= 2 * EVERY_MS),
    }
}

/// When the next verify is due, given the last run (or the first sight of
/// the vault) and the last activity.
pub(crate) fn due_at(last_ms: Option<i64>, first_ms: Option<i64>, now_ms: i64, now: Instant, active: Instant) -> Instant {
    let at = |t: i64, ms: i64| now + Duration::from_millis(u64::try_from(t + ms - now_ms).unwrap_or(0));
    let idle = (active + IDLE).max(now);
    match last_ms.filter(|l| *l <= now_ms) {
        Some(l) => at(l, EVERY_MS).max(active + IDLE).min(at(l, 2 * EVERY_MS)),
        None => first_ms.filter(|f| *f <= now_ms).map_or(idle, |f| idle.min(at(f, 2 * EVERY_MS))),
    }
}

/// The last run recorded in `dir`.
pub(crate) fn last_run(dir: &Path) -> Option<i64> {
    std::fs::read_to_string(dir.join(FILE)).ok()?.trim().parse().ok()
}

/// When this device first saw the vault without a verify, from `dir`;
/// `now_ms` (recorded there) if never.
pub(crate) fn first_seen(dir: &Path, now_ms: i64) -> i64 {
    if let Some(t) = std::fs::read_to_string(dir.join(FIRST_FILE)).ok().and_then(|s| s.trim().parse().ok()) {
        return t;
    }
    // Not recorded (a read-only state folder): counted from this start.
    let _ = quire_base::fs::write_atomic(&dir.join(FIRST_FILE), now_ms.to_string().as_bytes(), quire_base::fs::Options::STATE);
    now_ms
}

/// Record a run in `dir`.
pub(crate) fn record(dir: &Path, now_ms: i64) -> std::io::Result<()> {
    quire_base::fs::write_atomic(&dir.join(FILE), now_ms.to_string().as_bytes(), quire_base::fs::Options::STATE)
}

/// The vault is open with a manifest: start keeping time for the verify.
pub(crate) fn opened(ws: &mut Workspace, root: &Path) {
    let dir = quire_agent::approval::default_state_dir(root);
    let last_ms = dir.as_deref().and_then(last_run);
    // Never verified here: the two-days-late rule counts from the first sight.
    let now_ms = crate::history_ops::now_ms();
    let first_ms = last_ms.is_none().then(|| dir.as_deref().map_or(now_ms, |d| first_seen(d, now_ms)));
    ws.verify = VerifyState { opened: Some(Instant::now()), dir, last_ms, first_ms, running: None, retry_at: None };
}

/// The latest input or typing, or the vault's start.
fn last_active(ws: &Workspace) -> Option<Instant> {
    crate::secure_state::last_activity(ws).into_iter().chain(ws.verify.opened).max()
}

/// When [`tick`] should run next for the verify.
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    ws.manifest()?;
    let now = Instant::now();
    if ws.verify.running.is_some() {
        // To see it finish.
        return Some(now + Duration::from_secs(1));
    }
    let active = last_active(ws)?;
    let at = due_at(ws.verify.last_ms, ws.verify.first_ms, crate::history_ops::now_ms(), now, active);
    Some(ws.verify.retry_at.map_or(at, |r| at.max(r)))
}

/// Start the verify on a thread of its own if it is due, and take in the
/// one that ended. A run is recorded only once it read the notes: a quit
/// in the middle, or a listing error, doesn't count as done for a day.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) {
    if let Some((handle, started)) = ws.verify.running.take() {
        if !handle.is_finished() {
            ws.verify.running = Some((handle, started));
            return;
        }
        let ok = handle.join().unwrap_or(false);
        if ws.verify.finished(ok, started, now)
            && let Some(dir) = &ws.verify.dir
        {
            // Not recorded (a read-only state folder): it runs again at the
            // next start once idle, which costs reading the notes, never a result.
            let _ = record(dir, started);
        }
    }
    if ws.verify.retry_at.is_some_and(|r| now < r) {
        return;
    }
    let (Some(manifest), Some(active)) = (ws.manifest().cloned(), last_active(ws)) else { return };
    let now_ms = crate::history_ops::now_ms();
    if !due(ws.verify.last_ms, ws.verify.first_ms, now_ms, now.saturating_duration_since(active)) {
        return;
    }
    let handle = std::thread::Builder::new().name("quire-verify".into()).spawn(move || {
        quire_index::background::lower_priority();
        // A listing error leaves every row as it was.
        manifest.verify_where(&|p| p.is_markdown()).is_ok()
    });
    match handle {
        Ok(h) => ws.verify.running = Some((h, now_ms)),
        Err(_) => ws.verify.retry_at = Some(now + RETRY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_000_000_000;

    #[test]
    fn a_verify_is_due_once_a_day_when_idle() {
        let idle = IDLE;
        assert!(due(None, None, NOW, idle), "never ran");
        assert!(!due(None, None, NOW, idle.saturating_sub(Duration::from_secs(1))), "not idle long enough");
        assert!(!due(Some(NOW - EVERY_MS + 1), None, NOW, idle), "ran less than a day ago");
        assert!(due(Some(NOW - EVERY_MS), None, NOW, idle), "a day ago");
        assert!(due(Some(NOW + 60_000), None, NOW, idle), "a clock set back");
        assert!(!due(Some(NOW - EVERY_MS), None, NOW, Duration::ZERO), "a day late waits for idle");
        assert!(due(Some(NOW - 2 * EVERY_MS), None, NOW, Duration::ZERO), "two days late: whatever the person does");
        assert!(!due(None, Some(NOW), NOW, Duration::ZERO), "never ran, just seen: when idle");
    }

    /// A vault never verified here: due when idle, and two days after this
    /// device first saw it, whatever the person does (a person who is never
    /// idle for 5 minutes still gets a first verify).
    #[test]
    fn a_vault_never_verified_counts_two_days_from_when_it_was_first_seen() {
        assert!(!due(None, Some(NOW), NOW, Duration::ZERO), "just seen: waits for idle");
        assert!(due(None, Some(NOW), NOW, IDLE), "idle: at once");
        assert!(!due(None, Some(NOW - 2 * EVERY_MS + 1), NOW, Duration::ZERO));
        assert!(due(None, Some(NOW - 2 * EVERY_MS), NOW, Duration::ZERO), "two days after first seen");
        let busy = Instant::now() + IDLE * 100;
        assert_eq!(due_at(None, Some(NOW - 2 * EVERY_MS + 60_000), NOW, busy, busy), busy + Duration::from_mins(1));
        // First seen is kept in the state folder, once.
        let dir = tempfile::tempdir().expect("temp");
        assert_eq!(first_seen(dir.path(), NOW), NOW);
        assert_eq!(first_seen(dir.path(), NOW + EVERY_MS), NOW, "the first time stays");
    }

    /// Recorded only when it read the notes; a failed run is tried again
    /// after an hour, not a day.
    #[test]
    fn only_a_verify_that_read_the_notes_counts() {
        let now = Instant::now();
        let mut v = VerifyState::default();
        assert!(!v.finished(false, NOW, now));
        assert_eq!((v.last_ms, v.retry_at), (None, Some(now + RETRY)));
        assert!(v.finished(true, NOW, now + RETRY));
        assert_eq!((v.last_ms, v.retry_at), (Some(NOW), None));
    }

    #[test]
    fn the_wake_up_is_the_later_of_a_day_and_idle() {
        let active = Instant::now();
        assert_eq!(due_at(None, None, NOW, active, active), active + IDLE);
        let later = active + IDLE * 2;
        assert_eq!(due_at(None, None, NOW, later, active), later, "due now");
        let hour = Duration::from_hours(1);
        assert_eq!(due_at(Some(NOW - EVERY_MS + 3_600_000), None, NOW, later, active), later + hour);
        // Two days late comes even while the person is busy.
        let busy = active + IDLE * 100;
        assert_eq!(due_at(Some(NOW - 2 * EVERY_MS + 60_000), None, NOW, busy, busy), busy + Duration::from_mins(1));
    }

    #[test]
    fn the_last_run_is_kept_in_the_state_folder() {
        let dir = tempfile::tempdir().expect("temp");
        assert_eq!(last_run(dir.path()), None);
        record(dir.path(), NOW).expect("write");
        assert_eq!(last_run(dir.path()), Some(NOW));
        std::fs::write(dir.path().join(FILE), "junk").expect("write");
        assert_eq!(last_run(dir.path()), None, "unreadable: as never run");
    }
}
