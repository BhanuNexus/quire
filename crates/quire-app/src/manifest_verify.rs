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
/// Only after this long without input or typing.
pub(crate) const IDLE: Duration = Duration::from_mins(5);
/// The file in the vault's state folder holding the last run, Unix ms.
const FILE: &str = "manifest-verify";

/// The workspace's part.
#[derive(Default)]
pub(crate) struct VerifyState {
    /// When the vault's deferred start ran (idle is measured from it until
    /// the first input).
    pub(crate) opened: Option<Instant>,
    /// The vault's state folder on this device.
    pub(crate) dir: Option<PathBuf>,
    /// The last run, Unix ms, as read from `dir` or set here.
    pub(crate) last_ms: Option<i64>,
    running: Option<JoinHandle<()>>,
}

/// Whether a verify is due: none recorded, or the last a day ago or more
/// (or in the future: a clock set back), and idle long enough.
pub(crate) fn due(last_ms: Option<i64>, now_ms: i64, idle_for: Duration) -> bool {
    idle_for >= IDLE && last_ms.is_none_or(|l| now_ms < l || now_ms - l >= EVERY_MS)
}

/// When the next verify is due, given the last run and the last activity.
pub(crate) fn due_at(last_ms: Option<i64>, now_ms: i64, now: Instant, active: Instant) -> Instant {
    let by_day =
        last_ms.filter(|l| *l <= now_ms).map_or(now, |l| now + Duration::from_millis(u64::try_from(l + EVERY_MS - now_ms).unwrap_or(0)));
    by_day.max(active + IDLE)
}

/// The last run recorded in `dir`.
pub(crate) fn last_run(dir: &Path) -> Option<i64> {
    std::fs::read_to_string(dir.join(FILE)).ok()?.trim().parse().ok()
}

/// Record a run in `dir`.
pub(crate) fn record(dir: &Path, now_ms: i64) -> std::io::Result<()> {
    quire_base::fs::write_atomic(&dir.join(FILE), now_ms.to_string().as_bytes(), quire_base::fs::Options::STATE)
}

/// The vault is open with a manifest: start keeping time for the verify.
pub(crate) fn opened(ws: &mut Workspace, root: &Path) {
    let dir = quire_agent::approval::default_state_dir(root);
    let last_ms = dir.as_deref().and_then(last_run);
    ws.verify = VerifyState { opened: Some(Instant::now()), dir, last_ms, running: None };
}

/// The latest input or typing, or the vault's start.
fn last_active(ws: &Workspace) -> Option<Instant> {
    crate::secure_state::last_activity(ws).into_iter().chain(ws.verify.opened).max()
}

/// When [`tick`] should run next for the verify.
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    ws.manifest()?;
    if ws.verify.running.as_ref().is_some_and(|h| !h.is_finished()) {
        return None;
    }
    let active = last_active(ws)?;
    Some(due_at(ws.verify.last_ms, crate::history_ops::now_ms(), Instant::now(), active))
}

/// Start the verify on a thread of its own if it is due.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) {
    if ws.verify.running.as_ref().is_some_and(|h| !h.is_finished()) {
        return;
    }
    let (Some(manifest), Some(active)) = (ws.manifest().cloned(), last_active(ws)) else { return };
    let now_ms = crate::history_ops::now_ms();
    if !due(ws.verify.last_ms, now_ms, now.saturating_duration_since(active)) {
        return;
    }
    ws.verify.last_ms = Some(now_ms);
    if let Some(dir) = &ws.verify.dir {
        // Not recorded (a read-only state folder): it runs again at the next
        // start once idle, which costs reading the notes, never a result.
        let _ = record(dir, now_ms);
    }
    ws.verify.running = std::thread::Builder::new()
        .name("quire-verify".into())
        .spawn(move || {
            quire_index::background::lower_priority();
            // A listing error leaves every row as it was; the next day tries again.
            let _ = manifest.verify_where(&|p| p.is_markdown());
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_000_000_000;

    #[test]
    fn a_verify_is_due_once_a_day_when_idle() {
        let idle = IDLE;
        assert!(due(None, NOW, idle), "never ran");
        assert!(!due(None, NOW, idle.saturating_sub(Duration::from_secs(1))), "not idle long enough");
        assert!(!due(Some(NOW - EVERY_MS + 1), NOW, idle), "ran less than a day ago");
        assert!(due(Some(NOW - EVERY_MS), NOW, idle), "a day ago");
        assert!(due(Some(NOW + 60_000), NOW, idle), "a clock set back");
    }

    #[test]
    fn the_wake_up_is_the_later_of_a_day_and_idle() {
        let active = Instant::now();
        assert_eq!(due_at(None, NOW, active, active), active + IDLE);
        let later = active + IDLE * 2;
        assert_eq!(due_at(None, NOW, later, active), later, "due now");
        let hour = Duration::from_hours(1);
        assert_eq!(due_at(Some(NOW - EVERY_MS + 3_600_000), NOW, later, active), later + hour);
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
