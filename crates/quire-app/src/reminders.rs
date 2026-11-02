// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Task reminders while Quire runs (design §E.2).
//!
//! The schedule is rebuilt from the index when it changes. A reminder that
//! comes due shows in the app (a notice and the Agenda) and, where the
//! system allows, as a desktop notification with "Open" and "Snooze 10
//! min". Snoozes are kept on this device for the session. Reminders missed
//! in the last week while Quire was closed are listed once at start.
//!
//! Owned by WS-E; kept in `ws.features.tasks.reminders`.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, Local, NaiveDateTime};
use crossbeam_channel::{Receiver, Sender, unbounded};
use quire_plan::reminders::{self, Reminder};
use quire_vault::NotePath;

use crate::workspace::{NoticeLevel, Workspace};

/// How long "Snooze" waits.
pub const SNOOZE_MINUTES: i64 = 10;

/// A reminder that came due this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub reminder: Reminder,
    /// When it showed (it may have been snoozed first).
    pub at: NaiveDateTime,
}

/// What the person did with a desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reply {
    Open(String),
    Snooze(String),
}

#[derive(Debug)]
pub struct RemindersState {
    pub schedule: Vec<Reminder>,
    version: Option<u64>,
    /// Up to when reminders have been fired.
    checked: Option<NaiveDateTime>,
    /// Shown this session, newest last.
    pub fired: Vec<Fired>,
    /// Missed while Quire was closed (listed once at start).
    pub missed: Vec<Reminder>,
    missed_done: bool,
    snoozed: HashMap<String, (NaiveDateTime, Reminder)>,
    pub dismissed: HashSet<String>,
    replies: (Sender<Reply>, Receiver<Reply>),
}

impl Default for RemindersState {
    fn default() -> Self {
        Self {
            schedule: Vec::new(),
            version: None,
            checked: None,
            fired: Vec::new(),
            missed: Vec::new(),
            missed_done: false,
            snoozed: HashMap::new(),
            dismissed: HashSet::new(),
            replies: unbounded(),
        }
    }
}

impl RemindersState {
    /// The next time something is due (a reminder or a snooze ending).
    pub fn next_due(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        let next = reminders::next_after(&self.schedule, now).map(|r| r.at);
        let snooze = self.snoozed.values().map(|(t, _)| *t).filter(|t| *t > now).min();
        next.into_iter().chain(snooze).min()
    }

    /// Reminders shown this session or missed, not yet dismissed, newest first.
    pub fn active(&self) -> Vec<&Reminder> {
        let mut v: Vec<&Reminder> =
            self.fired.iter().map(|f| &f.reminder).chain(&self.missed).filter(|r| !self.dismissed.contains(&r.key())).collect();
        v.sort_by_key(|r| std::cmp::Reverse(r.at));
        v.dedup_by(|a, b| a.key() == b.key());
        v
    }

    pub fn snooze(&mut self, r: &Reminder, now: NaiveDateTime) {
        self.dismissed.insert(r.key());
        self.snoozed.insert(r.key(), (now + Duration::minutes(SNOOZE_MINUTES), r.clone()));
    }
}

/// Desktop notifications are off in tests and when `QUIRE_NO_NOTIFICATIONS` is set.
fn desktop_allowed() -> bool {
    !cfg!(test) && std::env::var_os("QUIRE_NO_NOTIFICATIONS").is_none()
}

/// Show a desktop notification on its own thread (the system call can
/// block); the answer comes back on `tx`.
fn notify_desktop(r: &Reminder, tx: Sender<Reply>) {
    let (key, text, note) = (r.key(), r.text.clone(), r.path.trim_end_matches(".md").to_string());
    let _ = std::thread::Builder::new().name("quire-reminder".into()).spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary("Quire reminder").body(&format!("{text}\n{note}")).appname("Quire");
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            n.action("open", "Open").action("snooze", "Snooze 10 min");
            if let Ok(handle) = n.show() {
                handle.wait_for_action(|action| match action {
                    "open" | "default" => {
                        let _ = tx.send(Reply::Open(key.clone()));
                    }
                    "snooze" => {
                        let _ = tx.send(Reply::Snooze(key.clone()));
                    }
                    _ => {}
                });
            }
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            // Actions need a reply channel these platforms don't give us
            // here; the in-app notice carries Open and Snooze.
            let _ = (n.show(), tx, key);
        }
    });
}

/// Per-frame reminder work; true when something changed.
pub(crate) fn tick(ws: &mut Workspace) -> bool {
    let now = Local::now().naive_local();
    let mut changed = false;
    let version = ws.index.version();
    let ready = !ws.index.status().busy && version > 0;
    if ws.features.tasks.reminders.version != Some(version) && ready {
        let schedule = {
            let ix = ws.index.read();
            let tasks: Vec<(String, quire_markdown::Task)> =
                ix.records().flat_map(|r| r.tasks.iter().map(|t| (r.path.as_str().to_string(), t.clone()))).collect();
            reminders::schedule(tasks.iter().map(|(p, t)| (p.as_str(), t)))
        };
        let st = &mut ws.features.tasks.reminders;
        st.schedule = schedule;
        st.version = Some(version);
        if !st.missed_done {
            st.missed_done = true;
            st.missed = reminders::missed(&st.schedule, now, None).into_iter().cloned().collect();
            st.checked = Some(now);
            let n = st.missed.len();
            if n > 0 {
                ws.notify(
                    NoticeLevel::Info,
                    format!("{n} reminder{} came due while Quire was closed (see the Agenda)", if n == 1 { "" } else { "s" }),
                );
            }
        }
        changed = true;
    }
    if !ws.features.tasks.reminders.missed_done {
        return changed;
    }
    // Replies from desktop notifications.
    while let Ok(reply) = ws.features.tasks.reminders.replies.1.try_recv() {
        changed = true;
        let find =
            |ws: &Workspace, key: &str| ws.features.tasks.reminders.fired.iter().map(|f| f.reminder.clone()).find(|r| r.key() == key);
        match reply {
            Reply::Open(key) => {
                if let Some(r) = find(ws, &key) {
                    open_reminder(ws, &r);
                }
            }
            Reply::Snooze(key) => {
                if let Some(r) = find(ws, &key) {
                    ws.features.tasks.reminders.snooze(&r, now);
                }
            }
        }
    }
    let after = ws.features.tasks.reminders.checked.unwrap_or(now);
    if now <= after {
        return changed;
    }
    let st = &mut ws.features.tasks.reminders;
    let mut due: Vec<Reminder> = reminders::due_in(&st.schedule, after, now).into_iter().cloned().collect();
    let woke: Vec<String> = st.snoozed.iter().filter(|(_, (t, _))| *t <= now).map(|(k, _)| k.clone()).collect();
    for k in woke {
        if let Some((_, r)) = st.snoozed.remove(&k) {
            st.dismissed.remove(&k);
            due.push(r);
        }
    }
    st.checked = Some(now);
    if due.is_empty() {
        return changed;
    }
    let desktop = ws.settings.tasks.reminders && desktop_allowed();
    for r in due {
        ws.features.tasks.reminders.fired.push(Fired { reminder: r.clone(), at: now });
        ws.notify(NoticeLevel::Info, format!("Reminder: {} ({})", r.text, r.path.trim_end_matches(".md")));
        if desktop {
            notify_desktop(&r, ws.features.tasks.reminders.replies.0.clone());
        }
    }
    true
}

/// Open the note of a reminder at its task.
pub fn open_reminder(ws: &mut Workspace, r: &Reminder) {
    let path = NotePath::new(&r.path);
    if ws.open_note(&path).is_some() {
        ws.jump_to_byte(r.offset);
    }
}

impl Workspace {
    /// Snooze a reminder for ten minutes.
    pub fn snooze_reminder(&mut self, r: &Reminder) {
        let now = Local::now().naive_local();
        self.features.tasks.reminders.snooze(r, now);
    }

    /// Hide a reminder from the Agenda's list.
    pub fn dismiss_reminder(&mut self, r: &Reminder) {
        self.features.tasks.reminders.dismissed.insert(r.key());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missed_then_due_then_snoozed() {
        let soon = (Local::now() + Duration::hours(2)).naive_local().format("%Y-%m-%d %H:%M").to_string();
        let past = (Local::now() - Duration::days(1)).naive_local().format("%Y-%m-%d %H:%M").to_string();
        let mut w = Workspace::new(crate::Settings::default());
        w.attach_vault(quire_vault::Vault::in_memory(&[(
            "T.md",
            &format!("- [ ] Call Bo ⏰ {past}\n- [ ] Ship ⏰ {soon}\n- [x] Done ⏰ {past}\n"),
        )]));
        w.index().flush();
        assert!(tick(&mut w));
        let st = &w.features.tasks.reminders;
        assert_eq!(st.schedule.len(), 2);
        assert_eq!(st.missed.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["Call Bo"]);
        assert!(w.notices.iter().any(|n| n.text.contains("came due while Quire was closed")));
        // Pretend time passed: everything up to now+2min is due.
        w.features.tasks.reminders.checked = Some(Local::now().naive_local() - Duration::minutes(2));
        w.features.tasks.reminders.schedule[1].at = Local::now().naive_local() - Duration::minutes(1);
        assert!(tick(&mut w));
        assert_eq!(w.features.tasks.reminders.fired.len(), 1);
        assert_eq!(w.features.tasks.reminders.active().len(), 2);
        let r = w.features.tasks.reminders.fired[0].reminder.clone();
        w.snooze_reminder(&r);
        assert_eq!(w.features.tasks.reminders.active().len(), 1);
        assert!(w.features.tasks.reminders.next_due(Local::now().naive_local()).is_some());
    }
}
