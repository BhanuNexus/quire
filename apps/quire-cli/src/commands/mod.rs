// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The subcommands, one module per area (design §6.1 F0.10). Each module
//! lists its commands in a `COMMANDS` table; [`all`] joins the tables and
//! the help text is built from them. Owners: `search`, `notes`, `import`,
//! `bench` (frozen after F0); `capture`, `mcp`, `agent`, `eval` (WS-B);
//! `lint` (WS-A); `ai` (WS-D); `tasks` (WS-E); `sync`, `serve` (0.10 serve);
//! `lock`, `unlock`, `status` (0.11 security); `links`, `tree` (0.12 typed links); `format` (0.12 switching tools).

mod agent;
mod ai;
mod bench;
mod capture;
#[cfg(feature = "eval")]
mod eval;
mod format;
mod import;
mod lint;
// 0.12: typed links (`links`, `tree`).
mod links;
mod lock;
mod mcp;
mod notes;
// 0.12: `prefs-copy`.
mod prefs;
mod remind;
mod scope_check;
mod search;
mod serve;
mod sync;
mod tasks;

use std::time::Instant;

use quire_app::index::{Index, IndexService};
use quire_app::vault::{NotePath, Vault};

/// One subcommand.
pub(crate) struct CliCommand {
    pub name: &'static str,
    /// Its line(s) in `quire-cli --help`.
    pub usage: &'static str,
    /// Where it appears in the help (0.8 order first).
    pub rank: u16,
    /// Runs with the whole argument list (`args[0]` is the command name).
    pub run: fn(&[String]) -> Result<(), String>,
}

/// Every subcommand, in help order.
pub(crate) fn all() -> Vec<&'static CliCommand> {
    let tables: &[&[CliCommand]] = &[
        search::COMMANDS,
        notes::COMMANDS,
        capture::COMMANDS,
        tasks::COMMANDS,
        remind::COMMANDS,
        mcp::COMMANDS,
        import::COMMANDS,
        bench::COMMANDS,
        agent::COMMANDS,
        lint::COMMANDS,
        ai::COMMANDS,
        sync::COMMANDS,
        serve::COMMANDS,
        lock::COMMANDS,
        links::COMMANDS,
        prefs::COMMANDS,
        format::COMMANDS,
        #[cfg(feature = "eval")]
        eval::COMMANDS,
    ];
    let mut v: Vec<&CliCommand> = tables.iter().flat_map(|t| t.iter()).collect();
    v.sort_by_key(|c| c.rank);
    v
}

pub(crate) fn find(name: &str) -> Option<&'static CliCommand> {
    all().into_iter().find(|c| c.name == name)
}

/// `quire-cli --help`.
pub(crate) fn usage() -> String {
    let mut s = String::from("quire-cli — headless Quire\n\nUSAGE:\n");
    for c in all() {
        s.push_str(c.usage);
        s.push('\n');
    }
    s.push_str("  quire-cli --version                         print the version");
    s
}

/// Argument `i`, or the help text as the error.
pub(crate) fn arg(args: &[String], i: usize) -> Result<&str, String> {
    args.get(i).map(String::as_str).ok_or_else(usage)
}

pub(crate) fn load(vault: &str) -> Result<(Vault, IndexService, f64), String> {
    let v = quire_app::secure_state::open_vault(std::path::Path::new(vault))?;
    let t = Instant::now();
    let svc = IndexService::spawn();
    svc.load(v.clone());
    svc.flush();
    Ok((v, svc, t.elapsed().as_secs_f64() * 1000.0))
}

pub(crate) fn find_note(ix: &Index, name: &str) -> Result<NotePath, String> {
    ix.resolve(name, None)
        .or_else(|| ix.quick_open(name, 1).into_iter().next().map(|h| h.path))
        .ok_or_else(|| format!("no note matches “{name}”"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_every_0_8_command_in_0_8_order() {
        let names: Vec<&str> = all().iter().map(|c| c.name).collect();
        let old = [
            "index",
            "search",
            "ask",
            "replace",
            "open",
            "backlinks",
            "related",
            "suggest",
            "tasks",
            "capture",
            "export",
            "pdf",
            "publish",
            "mcp",
            "import",
            "history",
            "calc",
            "bench",
        ];
        assert_eq!(&names[..old.len()], old);
        let u = usage();
        assert!(u.starts_with("quire-cli — headless Quire\n\nUSAGE:\n  quire-cli index     <vault>"));
        assert!(u.contains("  quire-cli agent"));
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "names are unique");
    }
}
