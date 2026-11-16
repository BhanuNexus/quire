// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli format <vault> [note-or-folder] [--write]`: the Markdown
//! linter and formatter with the vault's `[lint.format]` rules. A dry run
//! prints a diff per note; `--write` writes the notes (history kept).

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;

use quire_app::switch_state::{plan_folder, unified_diff, write_tidied};
use quire_app::vault::NotePath;

use super::{CliCommand, bad_usage};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "format",
    rank: 33,
    usage: "  quire-cli format    <vault> [note-or-folder] [--write]
                      tidy Markdown with the vault's [lint.format] rules; a diff first, notes written only with --write",
    run,
}];

fn run(args: &[String]) -> Result<(), String> {
    let pos: Vec<&str> = args
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(i, a)| !super::is_option(args, *i) || !a.starts_with("--"))
        .map(|(_, a)| a.as_str())
        .collect();
    let (Some(root), at) = (pos.first(), pos.get(1).copied().unwrap_or("")) else { return Err(bad_usage(args)) };
    let write = super::has_flag(args, "--write");
    let vault = super::open_vault(root)?;
    let opts = quire_app::switch_state::lint_options(&vault);
    // A note's path (with or without `.md`) or a folder.
    let note = NotePath::new(if at.to_ascii_lowercase().ends_with(".md") { at.to_string() } else { format!("{at}.md") });
    let folder = if !at.is_empty() && vault.exists(&note) { None } else { Some(at.trim_matches('/')) };
    let mut plan = plan_folder(&vault, folder.unwrap_or(""), &opts, &HashSet::new(), &AtomicBool::new(false));
    if folder.is_none() {
        plan.files.retain(|f| f.path == note);
    }
    for f in &plan.files {
        print!("{}", unified_diff(&f.path, &f.before, &f.after));
    }
    for (p, r) in &plan.reverted {
        eprintln!("  {p}: the {r} rule was left out; it would have changed more than whitespace and markup");
    }
    for (p, n) in &plan.notes {
        eprintln!("  {p}: {n}");
    }
    println!(
        "Checked {} notes: {} would change{}.",
        plan.checked,
        plan.files.len(),
        if plan.skipped > 0 { format!(", {} skipped", plan.skipped) } else { String::new() }
    );
    if !write {
        if !plan.files.is_empty() {
            println!("Dry run. Re-run with --write to write them.");
        }
        return Ok(());
    }
    let now = quire_app::history_ops::now_ms();
    let device = quire_app::device::this_device();
    let mut written = 0;
    for f in &plan.files {
        if write_tidied(&vault, &f.path, &f.before, &f.after, &device.id, now)? {
            written += 1;
        } else {
            eprintln!("  {} changed just now; left as it is", f.path);
        }
    }
    println!("Wrote {written} notes.");
    Ok(())
}
