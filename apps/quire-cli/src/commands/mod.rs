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

/// Where the literal operands start in the argument list: everything from
/// this index on came after a `--` and is text, never an option. `usize::MAX`
/// means there was no `--`. Set once by `main`.
static LITERAL_FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

pub(crate) fn set_literal_from(i: usize) {
    LITERAL_FROM.store(i, std::sync::atomic::Ordering::Relaxed);
}

/// The part of `args` that may hold options (flags and their values). Every
/// command reads flags from here only, so words after `--` stay literal.
pub(crate) fn options(args: &[String]) -> &[String] {
    &args[..LITERAL_FROM.load(std::sync::atomic::Ordering::Relaxed).min(args.len())]
}

/// Whether the word at `i` of `args` may be an option (it is before any `--`).
pub(crate) fn is_option(args: &[String], i: usize) -> bool {
    i < options(args).len()
}

/// The words of `args` from `from` on, without the options in `drop` (the
/// same words after `--` are text and kept).
pub(crate) fn words_without(args: &[String], from: usize, drop: &[&str]) -> Vec<String> {
    args.iter()
        .enumerate()
        .skip(from)
        .filter(|(i, a)| !(drop.contains(&a.as_str()) && is_option(args, *i)))
        .map(|(_, a)| a.clone())
        .collect()
}

/// Whether the flag `name` is among the options.
pub(crate) fn has_flag(args: &[String], name: &str) -> bool {
    options(args).iter().any(|a| a == name)
}

/// The word after the flag `name`, if the flag is among the options.
pub(crate) fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let o = options(args);
    o.iter().position(|a| a == name).and_then(|i| o.get(i + 1)).map(String::as_str)
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
    s.push_str("  quire-cli <command> --help                  that command's usage (put -- before text that starts with a dash)\n");
    s.push_str("  quire-cli --version                         print the version");
    s
}

/// Set when a failure is a mistake in how the command was typed (exit code 2).
static USAGE_ERROR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark the failure being returned as a usage error.
pub(crate) fn usage_error(message: String) -> String {
    USAGE_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
    message
}

/// Whether a failure was a usage error (see [`usage_error`]).
pub(crate) fn was_usage_error() -> bool {
    USAGE_ERROR.load(std::sync::atomic::Ordering::Relaxed)
}

/// The help for one command: its usage lines (`serve` has a longer text).
pub(crate) fn command_usage(c: &CliCommand) -> String {
    if c.name == "serve" { serve::HELP.to_string() } else { format!("USAGE:\n{}", c.usage) }
}

/// A command typed wrongly (arguments missing or not understood): one line
/// and a hint, and exit code 2.
pub(crate) fn bad_usage(args: &[String]) -> String {
    let name = args.first().map_or("quire-cli", String::as_str);
    usage_error(format!("quire-cli {name}: the arguments aren't right. Run `quire-cli {name} --help` for how to use it."))
}

/// A word that is not one of a command's subcommands: one line, exit code 2.
pub(crate) fn unknown_subcommand(args: &[String], what: &str, word: &str) -> String {
    let name = args.first().map_or("quire-cli", String::as_str);
    usage_error(format!("quire-cli {name}: unknown {what} “{word}”. Run `quire-cli {name} --help` for the commands."))
}

/// The name of argument `i` (`<vault>`) when the command's usage says it.
fn arg_name(c: &CliCommand, i: usize) -> Option<String> {
    let mut lines = c.usage.lines().map(str::trim).filter(|l| l.starts_with("quire-cli ") && l.split_whitespace().nth(1) == Some(c.name));
    let line = lines.next()?;
    if lines.next().is_some() {
        return None; // several forms: which one is meant isn't known
    }
    line.split_whitespace().nth(i + 1).filter(|t| t.starts_with('<')).map(|t| t.replace('…', ""))
}

/// Argument `i`, or a one-line error naming what is missing.
pub(crate) fn arg(args: &[String], i: usize) -> Result<&str, String> {
    args.get(i).map(String::as_str).ok_or_else(|| {
        let Some(name) = args.first() else { return usage_error(usage()) };
        let missing = find(name).and_then(|c| arg_name(c, i)).unwrap_or_else(|| "a required argument".to_string());
        usage_error(format!("quire-cli {name}: missing {missing}. Run `quire-cli {name} --help` for how to use it."))
    })
}

/// The vault at `path`, which must be an existing folder: a mistyped path is
/// an error, never a new empty vault.
pub(crate) fn open_vault(path: &str) -> Result<Vault, String> {
    let root = std::path::Path::new(path);
    quire_mcp::require_folder(root)?;
    quire_app::secure_state::open_vault(root)
}

/// The closest command name to a mistyped one.
pub(crate) fn suggest(name: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        let b: Vec<char> = b.chars().collect();
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.chars().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let old = row[j + 1];
                row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(old + 1);
                prev = old;
            }
        }
        row[b.len()]
    }
    all().into_iter().map(|c| (distance(name, c.name), c.name)).filter(|(d, _)| *d <= 2).min().map(|(_, n)| n)
}

/// The error for a command name that doesn't exist.
pub(crate) fn unknown_command(name: &str) -> String {
    let hint = suggest(name).map_or_else(String::new, |s| format!(" Did you mean “{s}”?"));
    usage_error(format!("quire-cli: unknown command “{name}”.{hint} Run `quire-cli --help` to see the commands."))
}

pub(crate) fn load(vault: &str) -> Result<(Vault, IndexService, f64), String> {
    let v = open_vault(vault)?;
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
    fn a_missing_argument_is_one_line_and_an_unknown_command_gets_a_hint() {
        let e = arg(&["search".to_string(), "/v".to_string()], 2).unwrap_err();
        assert_eq!(e, "quire-cli search: missing <query>. Run `quire-cli search --help` for how to use it.");
        let e = arg(&["sync".to_string()], 1).unwrap_err();
        assert!(e.starts_with("quire-cli sync: missing") && e.lines().count() == 1, "{e}");
        let e = unknown_command("searc");
        assert!(e.contains("Did you mean “search”?") && e.contains("quire-cli --help") && e.lines().count() == 1, "{e}");
        assert!(!unknown_command("zzzzzz").contains("Did you mean"));
        assert!(was_usage_error());
    }

    #[test]
    fn help_text_has_no_stray_backslashes() {
        for c in all() {
            assert!(!c.usage.contains('\\'), "{}: {}", c.name, c.usage);
        }
    }

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
