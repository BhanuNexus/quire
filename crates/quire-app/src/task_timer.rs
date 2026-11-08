// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Task notes and the task timer in the workspace (design 0.11-tasks).
//!
//! * The timer: one per device, in `<config>/task-timer.json` (shared with
//!   `quire-cli tasks start|stop`), so it survives a restart. Stopping it
//!   writes one time entry to the task through the same write path as any
//!   other edit (an undoable edit in an open note, else an atomic write with
//!   history before and after).
//! * Pomodoro: the timer counts down 25/5 (`[tasks.pomodoro]`); each work
//!   session is recorded when it ends, and a desktop notification says
//!   what's next.
//! * Task notes: "New task…", ticking a task note (recurring ones move to
//!   their next date), and the weekly time report.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate};
use quire_markdown::Value;
use quire_plan::task_note::{self, NewTask, TaskPrefs};
use quire_plan::time::{self, TimeEntry, WeekReport};
use quire_plan::timer::{Phase, Running, TaskRef, Transition};
use quire_vault::{NotePath, VaultPrefs};

use crate::workspace::{NoticeLevel, Workspace};

/// Sessions shorter than this aren't logged (a timer started by mistake).
pub const MIN_SECONDS: i64 = 60;

/// A session longer than this (8 hours) isn't logged without asking.
pub const LONG_SECONDS: i64 = 8 * 3600;

/// The timer file under the config folder.
pub const TIMER_FILE: &str = "task-timer.json";

/// The timer file: `None` in tests unless one is set.
pub fn default_timer_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::settings::config_dir().map(|d| d.join(TIMER_FILE))
}

/// Read the timer file: `Ok(None)` when there is none.
///
/// A file that can't be read as a timer is moved aside to `task-timer.json.bad` (so the next
/// start doesn't overwrite the running session's start time) and the
/// error says so (0.11 review L9).
pub fn load_running(path: &Path) -> Result<Option<Running>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Couldn't read the timer ({}): {e}", path.display())),
    };
    match serde_json::from_str(&text) {
        Ok(r) => Ok(Some(r)),
        Err(e) => {
            let mut bad = path.with_extension("json.bad");
            let mut n = 1;
            while bad.exists() {
                n += 1;
                bad = path.with_extension(format!("json.bad{n}"));
            }
            let kept = std::fs::rename(path, &bad).map_or_else(|_| path.display().to_string(), |()| bad.display().to_string());
            Err(format!("The timer file was damaged ({e}); it was kept as {kept} and no timer is running"))
        }
    }
}

/// Write (or with `None` remove) the timer file.
pub fn save_running(path: &Path, r: Option<&Running>) -> std::io::Result<()> {
    match r {
        Some(r) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let json = serde_json::to_string_pretty(r).map_err(std::io::Error::other)?;
            crate::atomic::write(path, json.as_bytes(), false)
        }
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// `[tasks]` in the vault's prefs.
pub fn prefs(vault: &quire_vault::Vault) -> TaskPrefs {
    VaultPrefs::load(vault).section::<TaskPrefs>("tasks")
}

/// Now, in the local offset.
pub fn now() -> DateTime<FixedOffset> {
    Local::now().fixed_offset()
}

/// The timer file's modification time (`None` when there is none).
fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Show a desktop notification (not in tests, nor with `QUIRE_NO_NOTIFICATIONS`).
fn desktop_notify(summary: &str, body: &str) {
    if cfg!(test) || std::env::var_os("QUIRE_NO_NOTIFICATIONS").is_some() {
        return;
    }
    let _ = notify_rust::Notification::new().summary(summary).body(body).appname("Quire").show();
}

/// The timer and task prefs, kept in `ws.features.tasks.timer`.
///
/// The file is the source of truth: `quire-cli tasks start|stop` and other
/// windows change it too, so it is read again whenever its modification
/// time changes, and every change to it is a compare-and-swap (0.11 review
/// M2).
#[derive(Debug, Default)]
pub struct TimerState {
    loaded: bool,
    /// Where the timer lives; `None` keeps it in memory (tests).
    pub file: Option<PathBuf>,
    running: Option<Running>,
    prefs: Option<TaskPrefs>,
    /// A timer from another vault (it can't be stopped from here).
    pub elsewhere: Option<Running>,
    /// The file's modification time when it was last read or written.
    seen: Option<std::time::SystemTime>,
    /// When the file was last checked (at most twice a second while idle).
    checked: Option<std::time::Instant>,
}

impl TimerState {
    /// A state that keeps its timer in `file` (tests, and the CLI).
    pub fn with_file(file: PathBuf) -> Self {
        Self { file: Some(file), ..Self::default() }
    }

    /// Replace `expected` with `new` in the file, unless the file no
    /// longer holds `expected` (another window or the CLI changed it).
    fn swap(&mut self, expected: Option<&Running>, new: Option<&Running>) -> Result<(), String> {
        let Some(f) = self.file.clone() else {
            self.running = new.cloned();
            return Ok(());
        };
        let now_in_file = load_running(&f)?;
        let same = match (&now_in_file, expected) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same_timer(b),
            _ => false,
        };
        if !same {
            return Err("The timer was changed in another window or by quire-cli; nothing was changed here".into());
        }
        save_running(&f, new).map_err(|e| format!("Couldn't save the timer: {e}"))?;
        self.seen = mtime(&f);
        self.running = new.cloned();
        Ok(())
    }
}

/// The weekday's Monday.
pub fn monday_of(d: NaiveDate) -> NaiveDate {
    d - chrono::Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

impl Workspace {
    fn timer_state(&self) -> &TimerState {
        &self.features.tasks.timer
    }

    /// Read the timer file when it changed since it was last read (or
    /// first, per vault). Returns whether the timer changed.
    pub(crate) fn timer_load(&mut self) -> bool {
        let root = self.vault_root().map(|p| p.to_string_lossy().into_owned());
        let st = &mut self.features.tasks.timer;
        if !st.loaded {
            st.loaded = true;
            if st.file.is_none() {
                st.file = default_timer_path();
            }
        } else if st.file.as_deref().is_none_or(|f| mtime(f) == st.seen) {
            return false;
        }
        st.checked = Some(std::time::Instant::now());
        let Some(file) = st.file.clone() else { return false };
        st.seen = mtime(&file);
        let found = load_running(&file);
        let before = (st.running.clone(), st.elsewhere.clone());
        st.running = None;
        st.elsewhere = None;
        let found = match found {
            Ok(f) => f,
            Err(e) => {
                self.notify(NoticeLevel::Warn, e);
                return true;
            }
        };
        let st = &mut self.features.tasks.timer;
        match (found, root) {
            (Some(r), Some(root)) if same_folder(&r.vault, &root) => st.running = Some(r),
            (Some(r), _) => st.elsewhere = Some(r),
            (None, _) => {}
        }
        before != (st.running.clone(), st.elsewhere.clone())
    }

    /// [`Self::timer_load`] at most twice a second (the tick).
    fn timer_poll(&mut self) -> bool {
        let st = &self.features.tasks.timer;
        if st.loaded && st.checked.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(500)) {
            return false;
        }
        self.features.tasks.timer.checked = Some(std::time::Instant::now());
        self.timer_load()
    }

    /// The timer's task follows a rename or move of its note.
    pub(crate) fn timer_path_moved(&mut self, old: &NotePath, new: &NotePath) {
        self.timer_load();
        let Some(r) = self.features.tasks.timer.running.clone() else { return };
        if r.task.path != old.as_str() {
            return;
        }
        let mut moved = r.clone();
        moved.task.path = new.as_str().to_string();
        if self.locked_folder_of(new).is_some() {
            moved.task.title = quire_plan::timer::locked_title(new.as_str());
            if moved.task.at.is_some() {
                moved.task.line = None;
            }
        }
        if let Err(e) = self.features.tasks.timer.swap(Some(&r), Some(&moved)) {
            self.notify(NoticeLevel::Warn, e);
        }
    }

    /// The vault's `[tasks]` prefs (read once per vault; [`Self::reload_task_prefs`] after a change).
    pub fn task_prefs(&self) -> TaskPrefs {
        if let Some(p) = &self.timer_state().prefs {
            return p.clone();
        }
        self.vault().map(prefs).unwrap_or_default()
    }

    pub fn reload_task_prefs(&mut self) {
        let p = self.vault().map(prefs);
        self.features.tasks.timer.prefs = p;
    }

    /// Save `[tasks]` to vault.toml.
    pub fn set_task_prefs(&mut self, p: &TaskPrefs) -> Result<(), String> {
        let vault = self.vault().cloned().ok_or("No vault is open")?;
        let mut file = VaultPrefs::load(&vault);
        file.set_section("tasks", p).map_err(|e| e.to_string())?;
        file.save(&vault).map_err(|e| e.to_string())?;
        self.features.tasks.timer.prefs = Some(p.clone());
        Ok(())
    }

    /// The running timer in this vault.
    pub fn timer(&self) -> Option<&Running> {
        self.timer_state().running.as_ref()
    }

    /// Whether the task on `path` is the one being timed: an inline task
    /// by its words (`line`), else the note.
    pub fn is_timing(&self, path: &NotePath, line: Option<&str>) -> bool {
        self.timer().is_some_and(|r| {
            r.unlogged.is_none()
                && r.task.path == path.as_str()
                && match line {
                    Some(text) => r.task.matches_text(text),
                    None => !r.task.is_inline(),
                }
        })
    }

    /// The task at byte `offset` of `path` (an inline task's line), else the
    /// note: what the ▶ buttons of the agenda and boards time.
    pub fn task_ref(&self, path: &NotePath, offset: Option<usize>, title: &str) -> TaskRef {
        let locked = self.locked_folder_of(path).is_some();
        if let Some(off) = offset
            && let Some(src) = self.note_text_now(path)
        {
            let off = off.min(src.len());
            let start = src[..off].rfind('\n').map_or(0, |i| i + 1);
            let end = src[off..].find('\n').map_or(src.len(), |i| off + i);
            let line_no = src[..start].matches('\n').count();
            if let Some(t) = TaskRef::inline(path.as_str(), line_no, &src[start..end], locked) {
                return t;
            }
        }
        let title = if locked { quire_plan::timer::locked_title(path.as_str()) } else { title.to_string() };
        TaskRef::note(path.as_str(), &title)
    }

    /// The task at the caret: the inline task on its line, else the note
    /// itself when it's a task note.
    pub fn task_at_caret(&self) -> Option<TaskRef> {
        let doc = self.active_doc()?;
        let path = doc.path.clone()?;
        let locked = self.locked_folder_of(&path).is_some();
        let (line_no, _) = doc.buffer().line_col(doc.selection().head);
        let line = doc.buffer().line(line_no);
        if let Some(t) = TaskRef::inline(path.as_str(), line_no, &line, locked) {
            return Some(t);
        }
        let props = doc.extracted().front_matter.as_ref().map(|f| f.fields.clone()).unwrap_or_default();
        let title = if locked {
            quire_plan::timer::locked_title(path.as_str())
        } else {
            self.index().read().get(&path).map_or_else(|| path.stem().to_string(), |r| r.title.clone())
        };
        task_note::is_task_note(&props, &self.task_prefs().tag).then(|| TaskRef::note(path.as_str(), &title))
    }

    /// Start timing `task` (a running timer for another task stops and is
    /// recorded first). `pomodoro`: count down work and break sessions.
    pub fn start_timer(&mut self, task: TaskRef, pomodoro: bool) -> bool {
        self.timer_load();
        if let Some(other) = &self.features.tasks.timer.elsewhere {
            let msg = format!("A timer is running in another vault ({}, “{}”); stop it there first", other.vault, other.task.title);
            self.notify(NoticeLevel::Warn, msg);
            return false;
        }
        if self.timer().is_some_and(|r| r.unlogged.is_none()) {
            self.stop_timer();
        }
        if let Some(r) = self.timer().filter(|r| r.unlogged.is_some()) {
            let msg = format!(
                "A session ({}) on “{}” isn't logged yet: log it to a task or discard it first",
                time::format_duration(r.clock_seconds(now(), &self.task_prefs().pomodoro).div_euclid(60).try_into().unwrap_or(0)),
                r.task.title
            );
            self.notify(NoticeLevel::Warn, msg);
            return false;
        }
        let Some(root) = self.vault_root().map(|p| p.to_string_lossy().into_owned()) else { return false };
        let title = task.title.clone();
        let new = Running::new(root, task, now(), pomodoro);
        let before = self.features.tasks.timer.running.clone();
        if let Err(e) = self.features.tasks.timer.swap(before.as_ref(), Some(&new)) {
            self.notify(NoticeLevel::Error, e);
            self.timer_load();
            return false;
        }
        let what = if pomodoro {
            format!("Pomodoro started: {} min of work on “{title}”", self.task_prefs().pomodoro.work)
        } else {
            format!("Timing “{title}”")
        };
        self.notify(NoticeLevel::Info, what);
        true
    }

    /// Stop the timer and record its session. Returns the minutes recorded.
    ///
    /// The entry is written first and the timer cleared after; when the
    /// entry can't be written, the session is kept, unlogged, to be logged
    /// to another task or discarded (0.11 review M1).
    pub fn stop_timer(&mut self) -> Option<u32> {
        self.timer_load();
        let r = self.features.tasks.timer.running.clone()?;
        let task = r.task.clone();
        self.log_session(&r, &task)
    }

    /// Log the running (or unlogged) session `r` to `task`, then clear it.
    fn log_session(&mut self, r: &Running, task: &TaskRef) -> Option<u32> {
        let Some(entry) = r.stop(now()) else {
            // A pomodoro break: nothing to log.
            if let Err(e) = self.features.tasks.timer.swap(Some(r), None) {
                self.notify(NoticeLevel::Warn, e);
                self.timer_load();
                return None;
            }
            return Some(0);
        };
        let minutes = entry.minutes(now());
        if entry.seconds(now()) < MIN_SECONDS && r.unlogged.is_none() {
            if let Err(e) = self.features.tasks.timer.swap(Some(r), None) {
                self.notify(NoticeLevel::Warn, e);
                self.timer_load();
                return None;
            }
            self.notify(NoticeLevel::Info, format!("Stopped “{}”: under a minute isn't logged", r.task.title));
            return Some(0);
        }
        // A timer left running over a weekend: ask before logging it all (0.11 review L12).
        if r.unlogged.is_none() && entry.seconds(now()) > LONG_SECONDS {
            let why = format!("it ran {}; was it left running? “Try again” logs it all", time::format_duration(minutes));
            self.keep_unlogged(r, &entry, &why);
            return None;
        }
        // The file must still hold this timer (the CLI may have logged it).
        if self.timer_load() || self.features.tasks.timer.running.as_ref().is_none_or(|now_r| !now_r.same_timer(r)) {
            self.notify(NoticeLevel::Warn, "The timer was changed in another window or by quire-cli; nothing was logged here");
            return None;
        }
        match self.record_time(task, &entry) {
            Ok(()) => {
                if let Err(e) = self.features.tasks.timer.swap(Some(r), None) {
                    self.notify(NoticeLevel::Warn, e);
                }
                self.notify(NoticeLevel::Info, format!("Logged {} on “{}”", time::format_duration(minutes), task.title));
                Some(minutes)
            }
            Err(why) => {
                self.keep_unlogged(r, &entry, &why);
                None
            }
        }
    }

    /// Keep a session that couldn't be logged in the timer file.
    fn keep_unlogged(&mut self, r: &Running, entry: &TimeEntry, why: &str) {
        let mut kept = r.clone();
        kept.keep_unlogged(entry);
        let saved = self.features.tasks.timer.swap(Some(r), Some(&kept));
        let minutes = time::format_duration(entry.minutes(now()));
        let msg = match saved {
            Ok(()) => format!(
                "Couldn't log {minutes} to “{}”: {why}. The session is kept: put the caret on a task (or use “Log here” on one) to log it there, or discard it",
                r.task.title
            ),
            Err(e) => format!("Couldn't log {minutes} to “{}”: {why}; and couldn't keep it either: {e}", r.task.title),
        };
        self.notify(NoticeLevel::Warn, msg);
    }

    /// Log the unlogged session to `task` (`None`: try its own task again).
    pub fn log_unlogged_to(&mut self, task: Option<TaskRef>) -> bool {
        self.timer_load();
        let Some(r) = self.timer().filter(|r| r.unlogged.is_some()).cloned() else {
            self.notify(NoticeLevel::Info, "No unlogged session");
            return false;
        };
        let task = task.unwrap_or_else(|| r.task.clone());
        self.log_session(&r, &task).is_some()
    }

    /// Throw the unlogged session away.
    pub fn discard_unlogged(&mut self) -> bool {
        self.timer_load();
        let Some(r) = self.timer().filter(|r| r.unlogged.is_some()).cloned() else { return false };
        match self.features.tasks.timer.swap(Some(&r), None) {
            Ok(()) => {
                self.notify(NoticeLevel::Info, format!("Discarded the unlogged session on “{}”", r.task.title));
                true
            }
            Err(e) => {
                self.notify(NoticeLevel::Warn, e);
                self.timer_load();
                false
            }
        }
    }

    /// Start or stop the timer on the task at the caret. With an unlogged
    /// session, the caret's task is where it goes.
    pub fn toggle_timer_at_caret(&mut self, pomodoro: bool) -> bool {
        self.timer_load();
        let task = self.task_at_caret();
        if self.timer().is_some_and(|r| r.unlogged.is_some()) {
            return self.log_unlogged_to(task);
        }
        let Some(task) = task else {
            if self.timer().is_some() {
                self.stop_timer();
                return true;
            }
            self.notify(NoticeLevel::Info, "Put the caret on a task (- [ ] …) or open a task note to time it");
            return false;
        };
        if self.timer().is_some_and(|r| r.task.same_task(&task)) && !pomodoro {
            self.stop_timer();
            return true;
        }
        self.start_timer(task, pomodoro)
    }

    /// Write one session to its task (front matter or `[time:: …]`), or
    /// say why it can't be.
    pub fn record_time(&mut self, task: &TaskRef, entry: &TimeEntry) -> Result<(), String> {
        let path = NotePath::new(&task.path);
        let Some(src) = self.note_text_now(&path) else {
            return Err(if self.locked_folder_of(&path).is_some() {
                format!("{} is in a locked folder; unlock it first", path.stem())
            } else {
                format!("{} isn't there any more", task.path)
            });
        };
        let style = self.task_prefs().style();
        let Some(out) = quire_plan::timer::record_entry(&src, task, entry, style) else {
            return Err(if task.is_inline() {
                format!("the task isn't in {} any more", path.stem())
            } else {
                format!("{}'s time_entries has a form Quire won't change", path.stem())
            });
        };
        self.write_note_with_history(&path, &src, &out, crate::history_ops::now_ms()).map(|_| ())
    }

    /// [`Self::note_text_now`] for the rest of the crate.
    pub(crate) fn note_text_now_pub(&self, path: &NotePath) -> Option<String> {
        self.note_text_now(path)
    }

    /// A note's text now: the open document's, else the file's.
    fn note_text_now(&self, path: &NotePath) -> Option<String> {
        if let Some(d) = self.docs.iter().find(|d| d.path.as_ref() == Some(path)) {
            return Some(d.text().to_string());
        }
        self.vault()?.read(path).ok()
    }

    /// Seconds on the status-bar clock, and the pomodoro phase if any.
    pub fn timer_clock(&self) -> Option<(i64, Option<Phase>)> {
        let r = self.timer()?;
        let prefs = self.task_prefs().pomodoro;
        Some((r.clock_seconds(now(), &prefs), r.pomodoro.as_ref().map(|p| p.phase)))
    }

    /// Move a pomodoro on; record finished work and say what's next.
    /// Returns whether anything changed.
    pub(crate) fn timer_tick(&mut self) -> bool {
        let reread = self.timer_poll();
        let prefs = self.task_prefs().pomodoro;
        let Some(before) = self.features.tasks.timer.running.clone() else { return reread };
        let mut r = before.clone();
        let Some(t) = r.advance(now(), &prefs) else { return reread };
        let Transition { finished, next, record, stopped } = t;
        // The entry first, then the timer file.
        if let Some(e) = &record
            && let Err(why) = self.record_time(&before.task, e)
        {
            self.keep_unlogged(&before, e, &why);
            return true;
        }
        let after = (!stopped).then_some(&r);
        if let Err(e) = self.features.tasks.timer.swap(Some(&before), after) {
            self.notify(NoticeLevel::Warn, e);
            self.timer_load();
            return true;
        }
        // Never a locked note's words in an OS notification (0.11 review S-M5).
        let path = NotePath::new(&before.task.path);
        let title = if self.locked_folder_of(&path).is_some() {
            quire_plan::timer::locked_title(&before.task.path)
        } else {
            before.task.title.clone()
        };
        let (summary, body) = match (finished, stopped) {
            (Phase::Work, _) => {
                let mins = if next == Phase::LongBreak { prefs.long_break } else { prefs.short_break };
                (format!("{} done", Phase::Work.label()), format!("Take a {mins}-minute break. “{title}” logged."))
            }
            (_, true) => ("Break over".to_string(), format!("Start the next pomodoro on “{title}” when you're ready.")),
            (_, false) => ("Break over".to_string(), format!("Back to “{title}” for {} minutes.", prefs.work)),
        };
        desktop_notify(&summary, &body);
        self.notify(NoticeLevel::Info, format!("{summary}: {body}"));
        true
    }

    /// Make a task note in the vault's task folder and open it.
    pub fn new_task(&mut self, t: &NewTask, open: bool) -> Option<NotePath> {
        let folder = self.task_prefs().folder;
        self.new_task_in(t, &folder, None, open)
    }

    /// [`Self::new_task`] in `folder`. `name`: the file's name when it
    /// mustn't be the title (a task note in a locked folder: names are
    /// visible there, so they don't repeat the task's words).
    pub(crate) fn new_task_in(&mut self, t: &NewTask, folder: &str, name: Option<&str>, open: bool) -> Option<NotePath> {
        let title = quire_vault::sanitize_title(name.unwrap_or(&t.title));
        if title.trim().is_empty() {
            self.notify(NoticeLevel::Info, "A task needs a title");
            return None;
        }
        let prefs = self.task_prefs();
        let vault = self.vault()?.clone();
        let path = vault.unique_path(folder, &title).ok()?;
        let locked = self.locked_folder_of(&path).is_some();
        let mut t = t.clone();
        if name.is_none() {
            t.title = path.stem().to_string();
        }
        if t.created.is_none() {
            t.created = Some(Local::now().format("%Y-%m-%d").to_string());
        }
        let text = task_note::new_note(&t, &prefs, "");
        if open {
            self.create_note_at(&path, &text, Some(text.len()))?;
        } else {
            // Never over a file that appeared since `unique_path` looked (0.11 review L8).
            match vault.write_if(&path, None, &text) {
                Ok(true) => {}
                Ok(false) => {
                    self.notify(
                        NoticeLevel::Warn,
                        format!("{} appeared just now (a sync?); nothing was changed, try again", path.as_str()),
                    );
                    return None;
                }
                Err(e) => {
                    self.notify(NoticeLevel::Error, format!("Couldn't create {}: {e}", path.stem()));
                    return None;
                }
            }
            // A locked note's text never goes to the plain index.
            if !locked {
                self.index.upsert(path.clone(), text);
            }
            self.notify(NoticeLevel::Info, format!("Made {}", path.as_str()));
        }
        Some(path)
    }

    /// Tick or untick a task note: status and completion date, or a
    /// recurring note's next dates. Front-matter lines it doesn't change keep their bytes.
    pub fn toggle_task_note(&mut self, path: &NotePath) -> bool {
        let Some(src) = self.note_text_now(path) else { return false };
        let props = quire_markdown::frontmatter::parse(&src).map(|f| f.fields).unwrap_or_default();
        let changes = task_note::toggle_changes(&props, &self.task_prefs(), Local::now().date_naive());
        self.apply_property_changes(path, &src, &changes)
    }

    /// Set a task note's property (kanban and agenda edits), under the name the note already uses.
    pub fn set_task_property(&mut self, path: &NotePath, prop: task_note::Prop, value: Option<Value>) -> bool {
        let Some(src) = self.note_text_now(path) else { return false };
        let props = quire_markdown::frontmatter::parse(&src).map(|f| f.fields).unwrap_or_default();
        let key = task_note::key_for(&props, prop, self.task_prefs().style());
        self.apply_property_changes(path, &src, &[(key, value)])
    }

    /// Apply front-matter changes as one write with history.
    pub fn apply_property_changes(&mut self, path: &NotePath, src: &str, changes: &[(String, Option<Value>)]) -> bool {
        let mut out = src.to_string();
        for (k, v) in changes {
            out = match v {
                Some(v) => quire_markdown::frontmatter::set_field(&out, k, v).apply(&out),
                None => quire_markdown::frontmatter::remove_field(&out, k).map_or_else(|| out.clone(), |e| e.apply(&out)),
            };
        }
        if out == src {
            return true;
        }
        match self.write_note_with_history(path, src, &out, crate::history_ops::now_ms()) {
            Ok(_) => true,
            Err(e) => {
                self.notify(NoticeLevel::Error, e);
                false
            }
        }
    }

    /// Every task note, read with the vault's prefs: (path, note).
    pub fn task_notes(&self) -> Vec<(NotePath, task_note::TaskNote)> {
        let prefs = self.task_prefs();
        let offset = *Local::now().offset();
        let ix = self.index().read();
        let mut v: Vec<(NotePath, task_note::TaskNote)> =
            ix.records().filter_map(|r| task_note::read(&r.properties, &r.title, &prefs, offset).map(|n| (r.path.clone(), n))).collect();
        // Open first, then by date (due, else scheduled; undated last), then by title.
        v.sort_by(|(pa, a), (pb, b)| {
            let key = |n: &task_note::TaskNote| {
                (
                    n.task.done,
                    n.task.due.clone().or_else(|| n.task.scheduled.clone()).is_none(),
                    n.task.due.clone().or_else(|| n.task.scheduled.clone()),
                    n.task.text.to_lowercase(),
                )
            };
            key(a).cmp(&key(b)).then_with(|| pa.cmp(pb))
        });
        v
    }

    /// Time per task note and day in the week holding `day`.
    pub fn week_report(&self, day: NaiveDate) -> WeekReport {
        let notes = self.task_notes();
        let items: Vec<(String, String, Vec<TimeEntry>)> =
            notes.into_iter().map(|(p, n)| (p.as_str().to_string(), n.task.text.clone(), n.entries)).collect();
        time::week_report(items.iter().map(|(p, t, e)| (p.as_str(), t.as_str(), e.as_slice())), monday_of(day), now(), &Local)
    }
}

/// Whether two folder paths are the same folder.
fn same_folder(a: &str, b: &str) -> bool {
    let canon = |p: &str| Path::new(p).canonicalize().unwrap_or_else(|_| PathBuf::from(p));
    a == b || canon(a) == canon(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_in(dir: &Path, files: &[(&str, &str)]) -> Workspace {
        for (p, t) in files {
            let full = dir.join("vault").join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, t).unwrap();
        }
        let mut w = Workspace::new(crate::Settings::default());
        w.open_vault(&dir.join("vault")).unwrap();
        w.index().flush();
        w.features.tasks.timer = TimerState::with_file(dir.join("config").join(TIMER_FILE));
        w
    }

    /// A timer started in one session is still running in the next (a
    /// restart), and stopping it there writes one entry to the note.
    #[test]
    fn a_timer_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let note = "---\ntype: task\nstatus: doing\n---\n# Write report\n";
        let mut a = ws_in(dir.path(), &[("Tasks/Write report.md", note)]);
        let task = TaskRef::note("Tasks/Write report.md", "Write report");
        assert!(a.start_timer(task, false));
        // Pretend it started 25 minutes ago.
        let file = dir.path().join("config").join(TIMER_FILE);
        let mut r = load_running(&file).unwrap().unwrap();
        r.started = time::format_instant(now() - chrono::Duration::minutes(25));
        save_running(&file, Some(&r)).unwrap();
        drop(a);
        // A new workspace (the app restarted) finds it.
        let mut b = ws_in(dir.path(), &[]);
        b.timer_load();
        assert_eq!(b.timer().map(|r| r.task.title.as_str()), Some("Write report"));
        let (secs, phase) = b.timer_clock().unwrap();
        assert!((1500..1510).contains(&secs) && phase.is_none(), "{secs}");
        assert_eq!(b.stop_timer(), Some(25));
        assert!(!file.exists(), "the timer file goes when it stops");
        let text = std::fs::read_to_string(dir.path().join("vault/Tasks/Write report.md")).unwrap();
        assert!(text.starts_with("---\ntype: task\nstatus: doing\ntime_entries:\n  - start: "), "{text}");
        let props = quire_markdown::frontmatter::parse(&text).unwrap().fields;
        let n = task_note::read(&props, "x", &TaskPrefs::default(), *Local::now().offset()).unwrap();
        assert_eq!(n.spent(now()), 25);
        // A timer from another vault is left alone.
        let other = Running::new("/somewhere/else".into(), TaskRef::note("A.md", "A"), now(), false);
        save_running(&file, Some(&other)).unwrap();
        let mut c = ws_in(dir.path(), &[]);
        c.timer_load();
        assert!(c.timer().is_none() && c.features.tasks.timer.elsewhere.is_some());
        assert!(!c.start_timer(TaskRef::note("B.md", "B"), false));
    }

    #[test]
    fn inline_tasks_pomodoros_and_task_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ws_in(
            dir.path(),
            &[
                ("Day.md", "# Day\n- [ ] Call Bo [time:: 10m]\n"),
                ("Tasks/Gym.md", "---\ntype: task\nstatus: todo\nrecurrence: every week\ndue: 2026-11-02\n---\n# Gym\n"),
            ],
        );
        // Inline: the caret's task.
        w.open_note(&NotePath::new("Day.md")).unwrap();
        w.jump_to_byte(10);
        let t = w.task_at_caret().unwrap();
        assert_eq!(t.line.as_deref(), Some("Call Bo"));
        let e = time::session(now() - chrono::Duration::minutes(20), now());
        assert_eq!(w.record_time(&t, &e), Ok(()));
        assert!(w.active_doc().unwrap().text().contains("- [ ] Call Bo [time:: 30m]"));
        // A pomodoro whose work ended records 25 minutes and starts a break.
        w.start_timer(TaskRef::note("Tasks/Gym.md", "Gym"), true);
        // (Started 26 minutes ago, in the timer file the app reads.)
        let file = dir.path().join("config").join(TIMER_FILE);
        let mut st = load_running(&file).unwrap().unwrap();
        let earlier = time::format_instant(now() - chrono::Duration::minutes(26));
        st.started.clone_from(&earlier);
        st.pomodoro.as_mut().unwrap().phase_started = earlier;
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_running(&file, Some(&st)).unwrap();
        w.timer_load();
        assert!(w.timer_tick());
        assert_eq!(w.timer().unwrap().pomodoro.as_ref().unwrap().phase, Phase::ShortBreak);
        let gym = w.vault().unwrap().read(&NotePath::new("Tasks/Gym.md")).unwrap();
        assert!(gym.contains("time_entries:\n  - start: "), "{gym}");
        // Ticking a recurring task note moves it on.
        assert!(w.toggle_task_note(&NotePath::new("Tasks/Gym.md")));
        let gym = w.vault().unwrap().read(&NotePath::new("Tasks/Gym.md")).unwrap();
        assert!(gym.contains("due: 2026-11-09\n") && gym.contains("status: todo\n") && gym.contains("complete_instances: ["), "{gym}");
        // New task notes go to the task folder.
        let p = w.new_task(&NewTask { title: "Plan trip".into(), due: Some("2026-12-01".into()), ..NewTask::default() }, false).unwrap();
        assert_eq!(p.as_str(), "Tasks/Plan trip.md");
        w.index().flush();
        assert!(w.task_notes().iter().any(|(path, n)| *path == p && n.task.due.as_deref() == Some("2026-12-01")));
        let r = w.week_report(Local::now().date_naive());
        assert_eq!(r.rows.first().map(|x| (x.title.as_str(), x.total)), Some(("Gym", 25)));
    }

    /// The running timer started `minutes` ago, as another process would
    /// write it (later than the app's own write, so it reads it again).
    fn started_ago(file: &Path, minutes: i64) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut r = load_running(file).unwrap().unwrap();
        r.started = time::format_instant(now() - chrono::Duration::minutes(minutes));
        save_running(file, Some(&r)).unwrap();
    }

    /// 0.11 review M1: when the entry can't be written, the session is
    /// kept (unlogged) and can be logged to another task; nothing is lost.
    #[test]
    fn a_session_that_cant_be_logged_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let note = "---\ntype: task\ntime_entries: null\n---\n# A\n";
        let mut w = ws_in(dir.path(), &[("Tasks/A.md", note), ("Tasks/B.md", "---\ntype: task\n---\n# B\n")]);
        let file = dir.path().join("config").join(TIMER_FILE);
        assert!(w.start_timer(TaskRef::note("Tasks/A.md", "A"), false));
        started_ago(&file, 25);
        assert_eq!(w.stop_timer(), None, "not logged");
        let kept = load_running(&file).unwrap().unwrap();
        assert!(kept.unlogged.is_some(), "the session is kept in the timer file");
        assert_eq!(std::fs::read_to_string(dir.path().join("vault/Tasks/A.md")).unwrap(), note, "A unchanged");
        // A new timer waits until it is logged or discarded.
        assert!(!w.start_timer(TaskRef::note("Tasks/B.md", "B"), false));
        assert!(w.log_unlogged_to(Some(TaskRef::note("Tasks/B.md", "B"))));
        let b = std::fs::read_to_string(dir.path().join("vault/Tasks/B.md")).unwrap();
        assert!(b.contains("time_entries:\n  - start: "), "{b}");
        assert!(!file.exists() && w.timer().is_none());
        // A moved note: the timer follows a rename in the app.
        assert!(w.start_timer(TaskRef::note("Tasks/B.md", "B"), false));
        started_ago(&file, 10);
        w.timer_load();
        w.open_note(&NotePath::new("Tasks/B.md")).unwrap();
        w.rename_active("C").unwrap();
        assert_eq!(w.timer().map(|r| r.task.path.as_str()), Some("Tasks/C.md"));
        assert_eq!(w.stop_timer(), Some(10));
        // A note deleted behind its back: kept, then discarded on request.
        assert!(w.start_timer(TaskRef::note("Tasks/A.md", "A"), false));
        started_ago(&file, 5);
        w.timer_load();
        std::fs::remove_file(dir.path().join("vault/Tasks/A.md")).unwrap();
        assert_eq!(w.stop_timer(), None);
        assert!(w.timer().is_some_and(|r| r.unlogged.is_some()));
        assert!(w.discard_unlogged());
        assert!(!file.exists());
    }

    /// 0.11 review L12: a plain timer left running for hours isn't logged
    /// without asking; "Try again" logs it.
    #[test]
    fn a_very_long_session_asks_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ws_in(dir.path(), &[("Tasks/A.md", "---\ntype: task\n---\n# A\n")]);
        let file = dir.path().join("config").join(TIMER_FILE);
        assert!(w.start_timer(TaskRef::note("Tasks/A.md", "A"), false));
        started_ago(&file, 60 * 60);
        w.timer_load();
        assert_eq!(w.stop_timer(), None);
        assert!(w.timer().is_some_and(|r| r.unlogged.is_some()));
        assert!(w.log_unlogged_to(None));
        let a = std::fs::read_to_string(dir.path().join("vault/Tasks/A.md")).unwrap();
        assert!(a.contains("time_entries:"), "{a}");
    }

    /// 0.11 review M2: the app reads the timer file again when the CLI (or
    /// another window) changed it, and never logs a session twice or drops
    /// the other's.
    #[test]
    fn the_app_and_the_cli_share_the_timer() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ws_in(dir.path(), &[("Tasks/A.md", "---\ntype: task\n---\n# A\n"), ("Tasks/B.md", "---\ntype: task\n---\n# B\n")]);
        let file = dir.path().join("config").join(TIMER_FILE);
        assert!(w.start_timer(TaskRef::note("Tasks/A.md", "A"), false));
        // The CLI stopped it (logged A) and started B.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let root = w.vault_root().unwrap().to_string_lossy().into_owned();
        let b = Running::new(root, TaskRef::note("Tasks/B.md", "B"), now() - chrono::Duration::minutes(3), false);
        save_running(&file, Some(&b)).unwrap();
        // Stopping in the app stops B (what the file says), not A again.
        assert_eq!(w.stop_timer(), Some(3));
        let a = std::fs::read_to_string(dir.path().join("vault/Tasks/A.md")).unwrap();
        assert!(!a.contains("time_entries"), "A wasn't logged by the app: {a}");
        let bt = std::fs::read_to_string(dir.path().join("vault/Tasks/B.md")).unwrap();
        assert!(bt.contains("time_entries"), "{bt}");
        // The CLI stopped it: the app sees no timer.
        assert!(w.start_timer(TaskRef::note("Tasks/A.md", "A"), false));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_file(&file).unwrap();
        w.timer_load();
        assert!(w.timer().is_none());
    }

    /// 0.11 review L9: a damaged timer file is kept aside, not overwritten.
    #[test]
    fn a_damaged_timer_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(TIMER_FILE);
        std::fs::write(&file, "{ \"started\": \"2026-11-06T10:00").unwrap();
        let e = load_running(&file).unwrap_err();
        assert!(e.contains("damaged"), "{e}");
        assert!(!file.exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("task-timer.json.bad")).unwrap(), "{ \"started\": \"2026-11-06T10:00");
        assert_eq!(load_running(&file), Ok(None));
    }

    /// 0.11 review M7 and S-M5: a task in a locked note is timed by its
    /// line; the timer file and messages name only the note.
    #[test]
    fn a_locked_task_is_timed_without_its_words() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ws_in(dir.path(), &[("Journal/Day.md", "# Day\n- [ ] Call the clinic\n"), ("Plain.md", "x\n")]);
        let file = dir.path().join("config").join(TIMER_FILE);
        let vault = w.vault().unwrap().clone();
        quire_vault::lock::lock_folder(&vault, "Journal", Some("a passphrase for the test"), quire_vault::lock::Kdf::TEST, 1).unwrap();
        w.open_note(&NotePath::new("Journal/Day.md"));
        assert!(w.locked_folder_of(&NotePath::new("Journal/Day.md")).is_some());
        let t = w.task_ref(&NotePath::new("Journal/Day.md"), Some(8), "Call the clinic");
        assert_eq!(t.title, "a task in Day");
        assert!(w.start_timer(t, false));
        let json = std::fs::read_to_string(&file).unwrap();
        assert!(!json.contains("clinic"), "{json}");
    }
}
