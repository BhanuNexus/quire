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

/// The timer file under the config folder.
pub const TIMER_FILE: &str = "task-timer.json";

/// The timer file: `None` in tests unless one is set.
pub fn default_timer_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::settings::config_dir().map(|d| d.join(TIMER_FILE))
}

/// Read the timer file; a missing or unreadable one is no timer.
pub fn load_running(path: &Path) -> Option<Running> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
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

/// Show a desktop notification (not in tests, nor with `QUIRE_NO_NOTIFICATIONS`).
fn desktop_notify(summary: &str, body: &str) {
    if cfg!(test) || std::env::var_os("QUIRE_NO_NOTIFICATIONS").is_some() {
        return;
    }
    let _ = notify_rust::Notification::new().summary(summary).body(body).appname("Quire").show();
}

/// The timer and task prefs, kept in `ws.features.tasks.timer`.
#[derive(Debug, Default)]
pub struct TimerState {
    loaded: bool,
    /// Where the timer lives; `None` keeps it in memory (tests).
    pub file: Option<PathBuf>,
    running: Option<Running>,
    prefs: Option<TaskPrefs>,
    /// A timer from another vault (it can't be stopped from here).
    pub elsewhere: Option<Running>,
}

impl TimerState {
    /// A state that keeps its timer in `file` (tests, and the CLI).
    pub fn with_file(file: PathBuf) -> Self {
        Self { file: Some(file), ..Self::default() }
    }

    fn save(&self) -> Result<(), String> {
        match &self.file {
            Some(f) => save_running(f, self.running.as_ref()).map_err(|e| format!("Couldn't save the timer: {e}")),
            None => Ok(()),
        }
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

    /// Load the timer file once per vault (and the vault's `[tasks]`).
    pub(crate) fn timer_load(&mut self) {
        let root = self.vault_root().map(|p| p.to_string_lossy().into_owned());
        let st = &mut self.features.tasks.timer;
        if st.loaded {
            return;
        }
        st.loaded = true;
        if st.file.is_none() {
            st.file = default_timer_path();
        }
        let found = st.file.as_deref().and_then(load_running);
        match (found, root) {
            (Some(r), Some(root)) if same_folder(&r.vault, &root) => st.running = Some(r),
            (Some(r), _) => st.elsewhere = Some(r),
            (None, _) => {}
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

    /// Whether `task` is the one being timed.
    pub fn is_timing(&self, path: &NotePath, line: Option<&str>) -> bool {
        self.timer().is_some_and(|r| r.task.path == path.as_str() && r.task.line.as_deref() == line)
    }

    /// The task at the caret: the inline task on its line, else the note
    /// itself when it's a task note.
    pub fn task_at_caret(&self) -> Option<TaskRef> {
        let doc = self.active_doc()?;
        let path = doc.path.clone()?;
        let (line_no, _) = doc.buffer().line_col(doc.selection().head);
        let line = doc.buffer().line(line_no);
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some((_, t)) = quire_plan::tasks::parse_line(line) {
            return Some(TaskRef { path: path.as_str().to_string(), title: t.text.clone(), line: Some(t.text) });
        }
        let props = doc.extracted().front_matter.as_ref().map(|f| f.fields.clone()).unwrap_or_default();
        let title = self.index().read().get(&path).map_or_else(|| path.stem().to_string(), |r| r.title.clone());
        task_note::is_task_note(&props, &self.task_prefs().tag).then(|| TaskRef { path: path.as_str().to_string(), line: None, title })
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
        if self.timer().is_some() {
            self.stop_timer();
        }
        let Some(root) = self.vault_root().map(|p| p.to_string_lossy().into_owned()) else { return false };
        let title = task.title.clone();
        self.features.tasks.timer.running = Some(Running::new(root, task, now(), pomodoro));
        if let Err(e) = self.features.tasks.timer.save() {
            self.notify(NoticeLevel::Error, e);
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
    pub fn stop_timer(&mut self) -> Option<u32> {
        self.timer_load();
        let r = self.features.tasks.timer.running.take()?;
        if let Err(e) = self.features.tasks.timer.save() {
            self.notify(NoticeLevel::Error, e);
        }
        let entry = r.stop(now())?;
        let minutes = entry.minutes(now());
        if entry.seconds(now()) < MIN_SECONDS {
            self.notify(NoticeLevel::Info, format!("Stopped “{}”: under a minute isn't logged", r.task.title));
            return Some(0);
        }
        if self.record_time(&r.task, &entry) {
            self.notify(NoticeLevel::Info, format!("Logged {} on “{}”", time::format_duration(minutes), r.task.title));
        }
        Some(minutes)
    }

    /// Start or stop the timer on the task at the caret.
    pub fn toggle_timer_at_caret(&mut self, pomodoro: bool) -> bool {
        let Some(task) = self.task_at_caret() else {
            if self.timer().is_some() {
                self.stop_timer();
                return true;
            }
            self.notify(NoticeLevel::Info, "Put the caret on a task (- [ ] …) or open a task note to time it");
            return false;
        };
        let path = NotePath::new(&task.path);
        if self.is_timing(&path, task.line.as_deref()) && !pomodoro {
            self.stop_timer();
            return true;
        }
        self.start_timer(task, pomodoro)
    }

    /// Write one session to its task (front matter or `[time:: …]`).
    pub fn record_time(&mut self, task: &TaskRef, entry: &TimeEntry) -> bool {
        let path = NotePath::new(&task.path);
        let Some(src) = self.note_text_now(&path) else {
            self.notify(NoticeLevel::Warn, format!("Couldn't find {} to log time to", task.path));
            return false;
        };
        let style = self.task_prefs().style();
        let Some(out) = quire_plan::timer::record_entry(&src, task, entry, style) else {
            self.notify(NoticeLevel::Warn, format!("Couldn't find the task “{}” in {} to log time to", task.title, path.stem()));
            return false;
        };
        match self.write_note_with_history(&path, &src, &out, crate::history_ops::now_ms()) {
            Ok(_) => true,
            Err(e) => {
                self.notify(NoticeLevel::Error, e);
                false
            }
        }
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
        self.timer_load();
        let prefs = self.task_prefs().pomodoro;
        let Some(r) = self.features.tasks.timer.running.as_mut() else { return false };
        let Some(t) = r.advance(now(), &prefs) else { return false };
        let task = r.task.clone();
        let Transition { finished, next, record, stopped } = t;
        if stopped {
            self.features.tasks.timer.running = None;
        }
        if let Err(e) = self.features.tasks.timer.save() {
            self.notify(NoticeLevel::Error, e);
        }
        if let Some(e) = &record {
            self.record_time(&task, e);
        }
        let (summary, body) = match (finished, stopped) {
            (Phase::Work, _) => {
                let mins = if next == Phase::LongBreak { prefs.long_break } else { prefs.short_break };
                (format!("{} done", Phase::Work.label()), format!("Take a {mins}-minute break. “{}” logged.", task.title))
            }
            (_, true) => ("Break over".to_string(), format!("Start the next pomodoro on “{}” when you're ready.", task.title)),
            (_, false) => ("Break over".to_string(), format!("Back to “{}” for {} minutes.", task.title, prefs.work)),
        };
        desktop_notify(&summary, &body);
        self.notify(NoticeLevel::Info, format!("{summary}: {body}"));
        true
    }

    /// Make a task note in the vault's task folder and open it.
    pub fn new_task(&mut self, t: &NewTask, open: bool) -> Option<NotePath> {
        let title = quire_vault::sanitize_title(&t.title);
        if title.trim().is_empty() {
            self.notify(NoticeLevel::Info, "A task needs a title");
            return None;
        }
        let prefs = self.task_prefs();
        let vault = self.vault()?.clone();
        let path = vault.unique_path(&prefs.folder, &title).ok()?;
        let mut t = t.clone();
        t.title = path.stem().to_string();
        if t.created.is_none() {
            t.created = Some(Local::now().format("%Y-%m-%d").to_string());
        }
        let text = task_note::new_note(&t, &prefs, "");
        if open {
            self.create_note_at(&path, &text, Some(text.len()))?;
        } else {
            if let Err(e) = vault.write(&path, &text) {
                self.notify(NoticeLevel::Error, format!("Couldn't create {}: {e}", path.stem()));
                return None;
            }
            self.index.upsert(path.clone(), text);
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
        time::week_report(items.iter().map(|(p, t, e)| (p.as_str(), t.as_str(), e.as_slice())), monday_of(day), now())
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
        let task = TaskRef { path: "Tasks/Write report.md".into(), line: None, title: "Write report".into() };
        assert!(a.start_timer(task, false));
        // Pretend it started 25 minutes ago.
        let file = dir.path().join("config").join(TIMER_FILE);
        let mut r = load_running(&file).unwrap();
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
        let other = Running::new("/somewhere/else".into(), TaskRef { path: "A.md".into(), line: None, title: "A".into() }, now(), false);
        save_running(&file, Some(&other)).unwrap();
        let mut c = ws_in(dir.path(), &[]);
        c.timer_load();
        assert!(c.timer().is_none() && c.features.tasks.timer.elsewhere.is_some());
        assert!(!c.start_timer(TaskRef { path: "B.md".into(), line: None, title: "B".into() }, false));
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
        assert!(w.record_time(&t, &e));
        assert!(w.active_doc().unwrap().text().contains("- [ ] Call Bo [time:: 30m]"));
        // A pomodoro whose work ended records 25 minutes and starts a break.
        w.start_timer(TaskRef { path: "Tasks/Gym.md".into(), line: None, title: "Gym".into() }, true);
        let st = w.features.tasks.timer.running.as_mut().unwrap();
        let earlier = time::format_instant(now() - chrono::Duration::minutes(26));
        st.started.clone_from(&earlier);
        st.pomodoro.as_mut().unwrap().phase_started = earlier;
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
}
