// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tasks and metadata from the command line (design §E.1, §E.2). Owned by WS-E.
//!
//! `tags` and `props` show what a rename, merge or delete would change and
//! write it only with `--write`; each written note keeps a version before
//! and after, and the batch is in the ledger.

use std::collections::HashMap;

use quire_app::meta_refactor::{FileEdit, MetaOp, plan};
use quire_app::vault::{ContentHash, NotePath, Vault};

use super::{CliCommand, load};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "tasks",
        rank: 9,
        usage: "  quire-cli tasks     <vault> [--agenda]      open tasks across the vault (--agenda: overdue, today, next 7 days)",
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
    let (_, svc, _) = load(arg(1)?)?;
    if args.iter().any(|a| a == "--agenda") {
        let ix = svc.read();
        let all: Vec<(String, quire_app::markdown::Task)> =
            ix.records().flat_map(|r| r.tasks.iter().map(|t| (r.path.as_str().to_string(), t.clone()))).collect();
        let today = chrono::Local::now().date_naive();
        let a = quire_app::plan::agenda::build(all.iter().map(|(p, t)| (p.as_str(), t)), today, None);
        print!("{}", agenda_text(&a, today));
        return Ok(());
    }
    let tasks = svc.read().tasks(false);
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

fn agenda_text(a: &quire_app::plan::agenda::Agenda, today: chrono::NaiveDate) -> String {
    use std::fmt::Write as _;
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
    use std::fmt::Write as _;
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
}
