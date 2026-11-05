// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli remind`: task reminders while Quire is closed (0.10).
//!
//! Shows each due reminder once as a desktop notification and notes it on
//! this device, where the app looks too. "Remind me even when Quire is
//! closed" (Settings → Planning) runs this every minute; `--install` and
//! `--uninstall` do the same from a terminal.

use std::path::PathBuf;

use quire_app::remind_bg::{self, Env, FiredLog};
use quire_app::vault::Vault;

use super::CliCommand;

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "remind",
    rank: 42,
    usage: "  quire-cli remind    <vault> [--dry-run | --install | --uninstall | --status]\n                                              show due task reminders as desktop notifications, once each; --install runs this every minute",
    run: remind,
}];

fn remind(args: &[String]) -> Result<(), String> {
    let root = PathBuf::from(super::arg(args, 1)?);
    let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
    let flag = |f: &str| args.iter().any(|a| a == f);
    if flag("--install") || flag("--uninstall") || flag("--status") {
        let env = Env::current().ok_or("no home folder")?;
        let backend = remind_bg::detect();
        if flag("--status") {
            let on = remind_bg::installed(backend, &root, &env);
            println!("{} for {}", if on { "Installed" } else { "Not installed" }, root.display());
            for l in remind_bg::describe(
                backend,
                &remind_bg::plan_install(backend, &std::env::current_exe().map_err(|e| e.to_string())?, &root, &env),
            ) {
                println!("  {l}");
            }
            return Ok(());
        }
        let plan = if flag("--install") {
            let me = std::env::current_exe().map_err(|e| e.to_string())?;
            remind_bg::install(backend, &me, &root, &env)?
        } else {
            remind_bg::uninstall(backend, &root, &env)?
        };
        println!("{} {}", if flag("--install") { "Installed" } else { "Removed" }, backend.label());
        for l in remind_bg::describe(backend, &plan) {
            println!("  {l}");
        }
        return Ok(());
    }
    let vault = Vault::open_local(&root).map_err(|e| e.to_string())?;
    let all = remind_bg::vault_reminders(&vault);
    let now = chrono::Local::now().naive_local();
    let mut log = FiredLog::load(&root);
    let first = log.fresh;
    let due = remind_bg::take_due(&all, &mut log, now);
    for r in &due {
        println!("{}  {}  ({})", r.at.format("%Y-%m-%d %H:%M"), r.text, r.path);
        if !flag("--dry-run") {
            remind_bg::notify(r);
        }
    }
    if first {
        println!("Started keeping track of reminders for {} (ones already due aren't shown).", root.display());
    }
    if !flag("--dry-run") {
        log.save(now).map_err(|e| e.to_string())?;
    }
    Ok(())
}
