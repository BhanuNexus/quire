// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli import <source> <vault> [--from KIND] [--into Folder] [--write]`:
//! a dry run unless `--write`. `--list <vault>` shows what was imported,
//! `--undo <id|last> <vault>` moves what an import created (and nobody
//! edited since) to the trash.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use quire_import::{ImportPlan, Source};

use super::{CliCommand, bad_usage};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "import",
    rank: 15,
    usage: "  quire-cli import    <source> <vault> [--from KIND] [--into Folder] [--write]
                      import Logseq, Roam, Notion, Bear, Evernote, Google Keep, Joplin, Day One, HTML pages
                      or Markdown (an Obsidian vault is a Markdown folder; folder, .zip, .jex, .enex, .bear2bk);
                      a dry run until --write
  quire-cli import    --list <vault>      what was imported, newest first
  quire-cli import    --undo <id|last> <vault>
                      move what an import created (and nobody edited since) to the trash",
    run: command,
}];

/// The arguments that aren't flags or flag values.
fn positional(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args.iter().skip(1) {
        if skip {
            skip = false;
        } else if matches!(a.as_str(), "--into" | "--from" | "--undo") {
            skip = true;
        } else if !a.starts_with("--") {
            out.push(a.as_str());
        }
    }
    out
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn command(args: &[String]) -> Result<(), String> {
    let pos = positional(args);
    if args.iter().any(|a| a == "--list") {
        return list(pos.first().copied().ok_or_else(|| bad_usage(args))?);
    }
    if let Some(id) = flag(args, "--undo") {
        return undo(id, pos.first().copied().ok_or_else(|| bad_usage(args))?);
    }
    let [source, vault] = pos[..] else { return Err(bad_usage(args)) };
    let from = match flag(args, "--from") {
        Some(k) => Some(Source::from_id(k).ok_or_else(|| format!("unknown source “{k}”; one of: {}", kinds()))?),
        None => None,
    };
    run(source, vault, from, flag(args, "--into").unwrap_or(""), args.iter().any(|a| a == "--write"))
}

fn kinds() -> String {
    Source::ALL.iter().map(|s| s.id()).collect::<Vec<_>>().join(", ")
}

pub(crate) fn print_plan(plan: &ImportPlan, vault: Option<&quire_app::vault::Vault>, folder: &str) {
    println!("{}", plan.summary());
    for n in plan.notes.iter().take(20) {
        println!("  note  {}", n.path);
    }
    if plan.notes.len() > 20 {
        println!("  … and {} more notes", plan.notes.len() - 20);
    }
    for f in plan.files.iter().take(5) {
        println!("  file  {}", f.path);
    }
    if plan.files.len() > 5 {
        println!("  … and {} more files", plan.files.len() - 5);
    }
    if let Some(v) = vault {
        let clashes = plan.clashes(v, folder);
        for (want, saved) in clashes.iter().take(10) {
            println!("  clash {want} already exists; the imported note will be saved as {saved}");
        }
        if clashes.len() > 10 {
            println!("  … and {} more name clashes", clashes.len() - 10);
        }
    }
    for w in &plan.warnings {
        println!("  warning: {w}");
    }
}

pub(crate) fn run(source: &str, vault: &str, from: Option<Source>, folder: &str, write: bool) -> Result<(), String> {
    let plan = quire_import::plan_as(Path::new(source), from, &quire_import::Limits::default())?;
    // Importing into a new vault is a documented workflow, and making the
    // vault's folder is this command's job (only with --write: a dry run
    // writes nothing, and plans without clash checks against a folder that
    // isn't there yet).
    let root = Path::new(vault);
    if write && !root.exists() {
        // One level only: a missing parent is an unplugged drive or a typo, not something to make.
        if let Err(e) = std::fs::create_dir(root) {
            return Err(quire_mcp::require_folder(root)
                .err()
                .unwrap_or_else(|| format!("Couldn't make the folder {}: {e}", root.display())));
        }
    }
    let vault = if write || root.exists() { Some(super::open_vault(vault)?) } else { None };
    print_plan(&plan, vault.as_ref(), folder);
    let target = if folder.is_empty() { "the vault root".to_string() } else { format!("{folder}/") };
    if !write {
        println!(
            "Dry run. Re-run with --write to import into {target}{}.",
            if vault.is_none() { " (the folder will be made)" } else { "" }
        );
        return Ok(());
    }
    let Some(vault) = vault else { return Err("the vault folder couldn't be opened".into()) };
    let now = quire_app::history_ops::now_ms();
    let device = quire_app::device::this_device();
    let mut last = 0usize;
    let (report, rec) =
        quire_app::import_run::run(&vault, &plan, folder, Path::new(source), &device.id, now, Some(&AtomicBool::new(false)), &mut |p| {
            if p.total > 0 && p.done / 100 != last / 100 {
                eprintln!("  {} of {}", p.done, p.total);
            }
            last = p.done;
        })?;
    println!("Imported {} notes and {} files into {target}.", report.notes, report.files);
    for (from, to) in &report.renamed {
        println!("  {from} already existed; saved as {to}");
    }
    if report.skipped_files > 0 {
        println!("  {} attachments already existed and were kept.", report.skipped_files);
    }
    for (from, to) in &report.renamed_files {
        println!("  attachment {from} was taken by a different file; saved as {to} (links follow)");
    }
    println!("Undo with: quire-cli import --undo {} <vault>", rec.id);
    if let Some(e) = &report.error {
        return Err(format!("the import stopped early: {e}. What was written is recorded and can be undone"));
    }
    Ok(())
}

fn list(vault: &str) -> Result<(), String> {
    let vault = super::open_vault(vault)?;
    let all = quire_app::import_run::list(&vault);
    if all.is_empty() {
        println!("No imports recorded in this vault.");
    }
    for r in all {
        let state = if r.undone_ms.is_some() { " (undone)" } else { "" };
        println!("{}  {}{state}", r.id, r.title());
    }
    Ok(())
}

fn undo(id: &str, vault: &str) -> Result<(), String> {
    let vault = super::open_vault(vault)?;
    let id = if id == "last" {
        quire_app::import_run::list(&vault).into_iter().find(|r| r.undone_ms.is_none()).map(|r| r.id).ok_or("there is no import to undo")?
    } else {
        id.to_string()
    };
    let (rec, plan) = quire_app::import_run::undo(&vault, &id, quire_app::history_ops::now_ms())?;
    println!("Undid {}: {} moved to .trash.", rec.title(), plan.trash.len());
    for p in &plan.edited {
        println!("  {p} was edited since the import; it stays");
    }
    for p in &plan.missing {
        println!("  {p} isn't where the import put it (renamed, moved or deleted); not touched");
    }
    if !plan.missing.is_empty() {
        println!("  The import stays open: put those back and run the undo again.");
    }
    Ok(())
}
