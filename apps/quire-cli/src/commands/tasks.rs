// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tasks and metadata from the command line (design §E.1, §E.2). Owned by WS-E.
//!
//! `tags` and `props` show what a rename, merge or delete would change and
//! write it only with `--write`; each written note keeps a version before
//! and after, and the batch is in the ledger.

use std::collections::HashMap;

use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, FixedOffset};
use quire_app::meta_refactor::{FileEdit, MetaOp, plan};
use quire_app::plan::task_note::TaskPrefs;
use quire_app::plan::timer::{Running, TaskRef};
use quire_app::vault::{ContentHash, NotePath, Vault};

use super::{CliCommand, load};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "tasks",
        rank: 9,
        usage: "  quire-cli tasks     <vault> [--agenda]      open tasks across the vault (--agenda: overdue, today, next 7 days)\n  quire-cli tasks     <vault> start <task> [--pomodoro] | stop | status\n                                              time a task (a task note's name, or words of a task line); stop logs it\n  quire-cli tasks     <vault> log <task> <duration> | report [YYYY-MM-DD]\n                                              log time by hand (25m, 1h30m); time per task in a week",
        run: tasks,
    },
    CliCommand {
        name: "tags",
        rank: 40,
        usage: "  quire-cli tags      <vault> [rename <a> <b> | merge <a> <b>… --into <c> | delete <a>] [--write]\n                                              list tags, or rename/merge/delete one everywhere (preview unless --write)",
        run: tags,
    },
    CliCommand {
        name: "props",
        rank: 41,
        usage: "  quire-cli props     <vault> [rename <k> <new> | merge <k>… --into <k> | value <k> <from> <to> | delete <k>] [--write]\n                                              list properties, or change one everywhere (preview unless --write)",
        run: props,
    },
];

fn tasks(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    if let Some(sub @ ("start" | "stop" | "status" | "log" | "report")) = args.get(2).map(String::as_str) {
        let file = quire_app::task_timer::default_timer_path().ok_or("no config folder for the timer")?;
        return timer_cmd(arg(1)?, sub, &args[3..], &file, chrono::Local::now().fixed_offset()).map(|out| print!("{out}"));
    }
    let (_, svc, _) = load(arg(1)?)?;
    if args.iter().any(|a| a == "--agenda") {
        let ix = svc.read();
        let all: Vec<(String, quire_app::markdown::Task)> = ix
            .records()
            .flat_map(|r| {
                let note = quire_app::plan::task_note::note_task(&r.properties, &r.title);
                r.tasks.iter().cloned().chain(note).map(|t| (r.path.as_str().to_string(), t))
            })
            .collect();
        let today = chrono::Local::now().date_naive();
        let a = quire_app::plan::agenda::build(all.iter().map(|(p, t)| (p.as_str(), t)), today, None);
        print!("{}", agenda_text(&a, today));
        return Ok(());
    }
    let mut tasks = svc.read().tasks(false);
    // 0.11: open task notes too.
    tasks.extend(svc.read().records().filter_map(|r| {
        let t = quire_app::plan::task_note::note_task(&r.properties, &r.title)?;
        (!t.done).then(|| (r.path.clone(), t))
    }));
    for (p, t) in tasks {
        let mut extra = Vec::new();
        for (label, v) in [("due", &t.due), ("scheduled", &t.scheduled), ("starts", &t.start)] {
            if let Some(v) = v {
                extra.push(format!("{label} {v}"));
            }
        }
        if let Some(r) = &t.recurrence {
            extra.push(format!("repeats {r}"));
        }
        let extra = if extra.is_empty() { String::new() } else { format!("  · {}", extra.join(", ")) };
        println!("[ ] {}  ({}){extra}", t.text, p.stem());
    }
    Ok(())
}

/// The task a name means: a task note by name or path, else the one open
/// task line whose text holds all the words.
fn find_task(vault: &Vault, ix: &quire_app::index::Index, query: &str, prefs: &TaskPrefs) -> Result<TaskRef, String> {
    if let Some(p) = ix.resolve(query, None)
        && let Some(r) = ix.get(&p)
        && quire_app::plan::task_note::is_task_note(&r.properties, &prefs.tag)
    {
        return Ok(TaskRef { path: p.as_str().to_string(), line: None, title: r.title.clone() });
    }
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut hits: Vec<TaskRef> = Vec::new();
    for r in ix.records() {
        for t in r.tasks.iter().filter(|t| !t.done) {
            let low = t.text.to_lowercase();
            if words.iter().all(|w| low.contains(w)) {
                hits.push(TaskRef { path: r.path.as_str().to_string(), line: Some(t.text.clone()), title: t.text.clone() });
            }
        }
        if let Some(n) = quire_app::plan::task_note::read(&r.properties, &r.title, prefs, *chrono::Local::now().offset()) {
            let low = n.task.text.to_lowercase();
            if !n.task.done && words.iter().all(|w| low.contains(w)) {
                hits.push(TaskRef { path: r.path.as_str().to_string(), line: None, title: n.task.text });
            }
        }
    }
    let _ = vault;
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(format!("no open task matches “{query}”")),
        n => {
            let list: Vec<String> = hits.iter().take(8).map(|h| format!("  {}  ({})", h.title, h.path.trim_end_matches(".md"))).collect();
            Err(format!("{n} tasks match “{query}”; say more:\n{}", list.join("\n")))
        }
    }
}

/// Write a note with its version before and after in history (as `tags --write` does).
fn write_logged(vault: &Vault, path: &NotePath, before: &str, after: &str) -> Result<(), String> {
    let now = quire_app::history_ops::now_ms();
    vault.snapshot(path, before, now - 1, true).map_err(|e| e.to_string())?;
    vault.write(path, after).map_err(|e| e.to_string())?;
    vault.snapshot(path, after, now, true).map_err(|e| e.to_string())?;
    Ok(())
}

/// Record a session to its task.
fn record(vault: &Vault, task: &TaskRef, entry: &quire_app::plan::time::TimeEntry, prefs: &TaskPrefs) -> Result<(), String> {
    let path = NotePath::new(&task.path);
    let src = vault.read(&path).map_err(|e| e.to_string())?;
    let out = quire_app::plan::timer::record_entry(&src, task, entry, prefs.style())
        .ok_or_else(|| format!("the task “{}” isn't in {} any more", task.title, task.path))?;
    write_logged(vault, &path, &src, &out)
}

/// `tasks <vault> start|stop|status|log|report`, with the timer in `file`. Returns what to print.
fn timer_cmd(root: &str, sub: &str, rest: &[String], file: &Path, now: DateTime<FixedOffset>) -> Result<String, String> {
    use quire_app::plan::time::{self, format_clock, format_duration};
    use quire_app::task_timer::{load_running, save_running};
    let (vault, svc, _) = load(root)?;
    let prefs = quire_app::task_timer::prefs(&vault);
    let root_abs = Path::new(root).canonicalize().map_or_else(|_| root.to_string(), |p| p.to_string_lossy().into_owned());
    let running = load_running(file);
    let mut out = String::new();
    let stop = |r: &Running, out: &mut String| -> Result<(), String> {
        let here = Path::new(&r.vault).canonicalize().map_or_else(|_| r.vault.clone(), |p| p.to_string_lossy().into_owned()) == root_abs;
        if !here {
            return Err(format!("the timer is running in another vault: {}", r.vault));
        }
        save_running(file, None).map_err(|e| e.to_string())?;
        match r.stop(now) {
            Some(e) if e.seconds(now) < quire_app::task_timer::MIN_SECONDS => {
                let _ = writeln!(out, "Stopped “{}” (under a minute isn't logged)", r.task.title);
            }
            Some(e) => {
                record(&vault, &r.task, &e, &prefs)?;
                let _ = writeln!(out, "Logged {} on “{}”", format_duration(e.minutes(now)), r.task.title);
            }
            None => {
                let _ = writeln!(out, "Stopped “{}” (on a break; nothing to log)", r.task.title);
            }
        }
        Ok(())
    };
    match sub {
        "start" => {
            let query = rest.iter().filter(|a| !a.starts_with("--")).cloned().collect::<Vec<_>>().join(" ");
            if query.is_empty() {
                return Err("start needs a task".into());
            }
            let task = find_task(&vault, &svc.read(), &query, &prefs)?;
            if let Some(r) = &running {
                stop(r, &mut out)?;
            }
            let pomodoro = rest.iter().any(|a| a == "--pomodoro");
            let r = Running::new(root_abs, task, now, pomodoro);
            save_running(file, Some(&r)).map_err(|e| e.to_string())?;
            let _ = writeln!(
                out,
                "Timing “{}”{}",
                r.task.title,
                if pomodoro { format!(" ({}-minute pomodoro)", prefs.pomodoro.work) } else { String::new() }
            );
        }
        "stop" => match &running {
            Some(r) => stop(r, &mut out)?,
            None => out.push_str("No timer is running.\n"),
        },
        "status" => match &running {
            Some(r) => {
                let secs = r.clock_seconds(now, &prefs.pomodoro);
                let what =
                    r.pomodoro.as_ref().map_or_else(|| "running".to_string(), |p| format!("{} left", p.phase.label().to_lowercase()));
                let _ = writeln!(out, "{} {what}: “{}” ({})", format_clock(secs), r.task.title, r.task.path.trim_end_matches(".md"));
            }
            None => out.push_str("No timer is running.\n"),
        },
        "log" => {
            let (dur, words) = rest.split_last().ok_or("log needs a task and a duration (25m, 1h30m)")?;
            let mins = time::parse_duration(dur).filter(|m| *m > 0).ok_or_else(|| format!("“{dur}” isn't a duration (25m, 1h30m)"))?;
            let task = find_task(&vault, &svc.read(), &words.join(" "), &prefs)?;
            let e = time::session(time::trunc_seconds(now) - chrono::Duration::minutes(i64::from(mins)), time::trunc_seconds(now));
            record(&vault, &task, &e, &prefs)?;
            let _ = writeln!(out, "Logged {} on “{}”", format_duration(mins), task.title);
        }
        _ => {
            let day = match rest.first() {
                Some(d) => chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| format!("“{d}” isn't a date (YYYY-MM-DD)"))?,
                None => now.date_naive(),
            };
            let ix = svc.read();
            let notes: Vec<(String, String, Vec<time::TimeEntry>)> = ix
                .records()
                .filter_map(|r| {
                    quire_app::plan::task_note::read(&r.properties, &r.title, &prefs, *now.offset())
                        .map(|n| (r.path.as_str().to_string(), r.title.clone(), n.entries))
                })
                .collect();
            let monday = quire_app::task_timer::monday_of(day);
            let rep = time::week_report(notes.iter().map(|(p, t, e)| (p.as_str(), t.as_str(), e.as_slice())), monday, now);
            out.push_str(&report_text(&rep));
        }
    }
    Ok(out)
}

/// The weekly report as a table.
fn report_text(r: &quire_app::plan::time::WeekReport) -> String {
    use quire_app::plan::time::format_duration;
    let Some(monday) = r.monday else { return String::new() };
    let mut s = format!("Week of {}\n", monday.format("%a %-d %b %Y"));
    if r.rows.is_empty() {
        s.push_str("No time logged on task notes this week.\n");
        return s;
    }
    let cell = |m: u32| if m == 0 { "·".to_string() } else { format_duration(m) };
    let _ = writeln!(
        s,
        "{:<32} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>7}",
        "Task", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Total"
    );
    for row in &r.rows {
        let title: String = row.title.chars().take(32).collect();
        let days: Vec<String> = row.days.iter().map(|m| format!("{:>6}", cell(*m))).collect();
        let _ = writeln!(s, "{title:<32} {} {:>7}", days.join(" "), format_duration(row.total));
    }
    let days: Vec<String> = r.days.iter().map(|m| format!("{:>6}", cell(*m))).collect();
    let _ = writeln!(s, "{:<32} {} {:>7}", "All tasks", days.join(" "), format_duration(r.total));
    s
}

fn agenda_text(a: &quire_app::plan::agenda::Agenda, today: chrono::NaiveDate) -> String {
    let mut s = String::new();
    let mut section = |title: &str, items: &[quire_app::plan::agenda::AgendaItem]| {
        if items.is_empty() {
            return;
        }
        let _ = writeln!(s, "{title}");
        for i in items {
            let when = if i.date == today { String::new() } else { format!("{} ", i.date.format("%a %Y-%m-%d")) };
            let _ = writeln!(s, "  [ ] {when}{}  ({})", i.task.text, i.path.trim_end_matches(".md"));
        }
    };
    section("Overdue", &a.overdue);
    section("Today", &a.today);
    section("Next 7 days", &a.upcoming);
    if s.is_empty() {
        s.push_str("Nothing due in the next 7 days.\n");
    }
    s
}

/// `--into <name>` and the arguments before it.
fn split_into(rest: &[String]) -> Result<(Vec<String>, String), String> {
    let i = rest.iter().position(|a| a == "--into").ok_or("merge needs --into <name>")?;
    let into = rest.get(i + 1).ok_or("merge needs --into <name>")?.clone();
    Ok((rest[..i].to_vec(), into))
}

fn tags(args: &[String]) -> Result<(), String> {
    let write = args.iter().any(|a| a == "--write");
    let rest: Vec<String> = args.iter().skip(2).filter(|a| *a != "--write").cloned().collect();
    let strip = |t: &str| t.trim_start_matches('#').to_string();
    let op = match rest.first().map(String::as_str) {
        None | Some("list") => {
            let (_, svc, _) = load(super::arg(args, 1)?)?;
            let mut tags = svc.read().tags();
            tags.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for (t, n) in tags {
                println!("#{t}  {n}");
            }
            return Ok(());
        }
        Some("rename") if rest.len() == 3 => MetaOp::RenameTag { from: strip(&rest[1]), to: strip(&rest[2]) },
        Some("merge") => {
            let (from, into) = split_into(&rest[1..])?;
            MetaOp::MergeTags { from: from.iter().map(|t| strip(t)).collect(), into: strip(&into) }
        }
        Some("delete") if rest.len() == 2 => MetaOp::DeleteTag { tag: strip(&rest[1]) },
        _ => return Err(super::usage()),
    };
    refactor(super::arg(args, 1)?, &op, write)
}

fn props(args: &[String]) -> Result<(), String> {
    let write = args.iter().any(|a| a == "--write");
    let rest: Vec<String> = args.iter().skip(2).filter(|a| *a != "--write").cloned().collect();
    let op = match rest.first().map(String::as_str) {
        None | Some("list") => {
            let (_, svc, _) = load(super::arg(args, 1)?)?;
            let ix = svc.read();
            let mut counts: HashMap<String, usize> = HashMap::new();
            for r in ix.records() {
                for (k, v) in &r.properties {
                    for (sub, _) in v.leaves() {
                        let name = if sub.is_empty() { k.clone() } else { format!("{k}.{sub}") };
                        *counts.entry(name).or_default() += 1;
                    }
                }
            }
            let mut v: Vec<(String, usize)> = counts.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            for (k, n) in v {
                println!("{k}  {n}");
            }
            return Ok(());
        }
        Some("rename") if rest.len() == 3 => MetaOp::RenameKey { from: rest[1].clone(), to: rest[2].clone() },
        Some("merge") => {
            let (from, into) = split_into(&rest[1..])?;
            MetaOp::MergeKeys { from, into }
        }
        Some("value") if rest.len() == 4 => MetaOp::RenameValue { key: rest[1].clone(), from: rest[2].clone(), to: rest[3].clone() },
        Some("delete") if rest.len() == 2 => MetaOp::DeleteKey { key: rest[1].clone() },
        _ => return Err(super::usage()),
    };
    refactor(super::arg(args, 1)?, &op, write)
}

/// Every note's edit, read from disk.
fn plan_vault(vault: &Vault, op: &MetaOp) -> Result<Vec<FileEdit>, String> {
    let notes = vault.notes().map_err(|e| e.to_string())?;
    let texts: Vec<(NotePath, String)> = notes.into_iter().filter_map(|p| vault.read(&p).ok().map(|t| (p, t))).collect();
    Ok(plan(texts.iter().map(|(p, t)| (p, t.as_str())), op))
}

fn refactor(root: &str, op: &MetaOp, write: bool) -> Result<(), String> {
    if let Some(why) = op.problem() {
        return Err(why);
    }
    let vault = Vault::open_local(root).map_err(|e| e.to_string())?;
    let edits = plan_vault(&vault, op)?;
    print!("{}", preview_text(op, &edits));
    if !write {
        if !edits.is_empty() {
            println!("Nothing written. Run again with --write to make these changes.");
        }
        return Ok(());
    }
    let device = quire_app::device::this_device();
    let store = quire_app::agent::AgentStore::open(&vault, device.clone(), std::sync::Arc::new(quire_app::agent::SystemClock));
    let now = quire_app::history_ops::now_ms();
    let batch = quire_app::agent::BatchId::new(now).as_str().to_string();
    let mut written = 0;
    for e in &edits {
        // Re-read: the note may have changed since the plan.
        let current = vault.read(&e.path).map_err(|err| err.to_string())?;
        let Some(fresh) = quire_app::meta_refactor::apply_to(&e.path, &current, op) else { continue };
        vault.snapshot(&e.path, &current, now - 1, true).map_err(|err| err.to_string())?;
        vault.write(&e.path, &fresh.after).map_err(|err| err.to_string())?;
        vault.snapshot(&e.path, &fresh.after, now, true).map_err(|err| err.to_string())?;
        let mut entry =
            quire_app::agent::LedgerEntry::new(quire_app::agent::Actor::human(&device.id), quire_app::agent::Op::Write, now).path(&e.path);
        entry.before = Some(ContentHash::of(&current));
        entry.after = Some(ContentHash::of(&fresh.after));
        entry.snap_before = Some(now - 1);
        entry.snap_after = Some(now);
        entry.batch = Some(batch.clone());
        entry.other.insert("title".into(), serde_json::Value::String(op.title()));
        store.record(&entry);
        written += 1;
    }
    println!(
        "Changed {written} note{} (batch {batch}; each note's earlier version is in its history).",
        if written == 1 { "" } else { "s" }
    );
    Ok(())
}

fn preview_text(op: &MetaOp, edits: &[FileEdit]) -> String {
    let mut s = format!("{}: {} note{}\n", op.title(), edits.len(), if edits.len() == 1 { "" } else { "s" });
    for e in edits {
        let _ = writeln!(s, "  {}", e.path.as_str());
        for (gone, new) in e.diff(4) {
            for l in gone {
                let _ = writeln!(s, "    - {l}");
            }
            for l in new {
                let _ = writeln!(s, "    + {l}");
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_rename_previews_then_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.md"), "# A #old\n").unwrap();
        std::fs::write(dir.path().join("B.md"), "---\ntags: [old, x]\n---\nB\n").unwrap();
        let root = dir.path().to_str().unwrap();
        let op = MetaOp::RenameTag { from: "old".into(), to: "new".into() };
        let vault = Vault::open_local(root).unwrap();
        let edits = plan_vault(&vault, &op).unwrap();
        let text = preview_text(&op, &edits);
        assert!(text.starts_with("Rename tag #old to #new: 2 notes\n  A.md\n    - # A #old\n    + # A #new\n"), "{text}");
        refactor(root, &op, false).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("A.md")).unwrap(), "# A #old\n", "preview only");
        refactor(root, &op, true).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("A.md")).unwrap(), "# A #new\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("B.md")).unwrap(), "---\ntags: [new, x]\n---\nB\n");
        assert_eq!(vault.history(&"B.md".into()).len(), 2, "before and after");
        let agenda = agenda_text(&quire_app::plan::agenda::Agenda::default(), chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap());
        assert_eq!(agenda, "Nothing due in the next 7 days.\n");
    }

    /// `start`, a restart of the CLI (a new process reads the file), `stop`,
    /// `log` and `report` against a real vault folder.
    #[test]
    fn timer_from_the_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("v");
        std::fs::create_dir_all(v.join("Tasks")).unwrap();
        std::fs::write(v.join("Tasks/Write report.md"), "---\ntype: task\nstatus: doing\n---\n# Write report\n").unwrap();
        std::fs::write(v.join("Day.md"), "# Day\n- [ ] Call Bo about the venue\n- [ ] Call Al\n").unwrap();
        let root = v.to_str().unwrap();
        let file = dir.path().join("task-timer.json");
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap();
        let out = timer_cmd(root, "start", &["Write report".into()], &file, at("2026-11-06T10:00:00+01:00")).unwrap();
        assert_eq!(out, "Timing “Write report”\n");
        let out = timer_cmd(root, "status", &[], &file, at("2026-11-06T10:01:05+01:00")).unwrap();
        assert_eq!(out, "01:05 running: “Write report” (Tasks/Write report)\n");
        // Starting another task stops and logs the first.
        let out = timer_cmd(root, "start", &["call".into(), "venue".into()], &file, at("2026-11-06T10:25:00+01:00")).unwrap();
        assert_eq!(out, "Logged 25m on “Write report”\nTiming “Call Bo about the venue”\n");
        let note = std::fs::read_to_string(v.join("Tasks/Write report.md")).unwrap();
        assert_eq!(
            note,
            "---\ntype: task\nstatus: doing\ntime_entries:\n  - start: 2026-11-06T10:00:00+01:00\n    end: 2026-11-06T10:25:00+01:00\n---\n# Write report\n"
        );
        let out = timer_cmd(root, "stop", &[], &file, at("2026-11-06T10:40:00+01:00")).unwrap();
        assert_eq!(out, "Logged 15m on “Call Bo about the venue”\n");
        assert_eq!(
            std::fs::read_to_string(v.join("Day.md")).unwrap(),
            "# Day\n- [ ] Call Bo about the venue [time:: 15m]\n- [ ] Call Al\n"
        );
        assert!(timer_cmd(root, "start", &["call".into()], &file, at("2026-11-06T11:00:00+01:00")).unwrap_err().contains("2 tasks match"));
        let out =
            timer_cmd(root, "log", &["Write".into(), "report".into(), "1h5m".into()], &file, at("2026-11-07T09:00:00+01:00")).unwrap();
        assert_eq!(out, "Logged 1h5m on “Write report”\n");
        let out = timer_cmd(root, "report", &["2026-11-04".into()], &file, at("2026-11-08T09:00:00+01:00")).unwrap();
        assert!(out.starts_with("Week of Mon 2 Nov 2026\nTask"), "{out}");
        assert!(out.contains("\nWrite report                          ·      ·      ·      ·    25m   1h5m      ·   1h30m\n"), "{out}");
        assert_eq!(timer_cmd(root, "stop", &[], &file, at("2026-11-08T09:00:00+01:00")).unwrap(), "No timer is running.\n");
        assert!(Vault::open_local(root).unwrap().history(&"Day.md".into()).len() >= 2, "before and after kept");
    }
}
