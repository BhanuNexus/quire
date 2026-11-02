// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli import <source> <vault> [--into Folder] [--write]`: dry run by default.

use std::path::Path;

use quire_app::vault::Vault;

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "import",
    rank: 15,
    usage: "  quire-cli import    <source> <vault> [--into Folder] [--write]
                      import Logseq, Bear, Notion, Evernote or Markdown (folder, .zip, .enex, .bear2bk; dry run first)",
    run: command,
}];

fn command(args: &[String]) -> Result<(), String> {
    let into = args.iter().position(|a| a == "--into").and_then(|i| args.get(i + 1)).map_or("", String::as_str);
    run(arg(args, 1)?, arg(args, 2)?, into, args.iter().any(|a| a == "--write"))
}

pub(crate) fn run(source: &str, vault: &str, folder: &str, write: bool) -> Result<(), String> {
    let plan = quire_import::plan(Path::new(source))?;
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
    for w in &plan.warnings {
        println!("  warning: {w}");
    }
    let target = if folder.is_empty() { "the vault root".to_string() } else { format!("{folder}/") };
    if !write {
        println!("Dry run. Re-run with --write to import into {target}.");
        return Ok(());
    }
    let vault = Vault::open_local(vault).map_err(|e| e.to_string())?;
    let report = quire_import::apply(&plan, &vault, folder)?;
    println!("Imported {} notes and {} files into {target}.", report.notes, report.files);
    for (from, to) in &report.renamed {
        println!("  {from} already existed; saved as {to}");
    }
    if report.skipped_files > 0 {
        println!("  {} attachments already existed and were kept.", report.skipped_files);
    }
    Ok(())
}
