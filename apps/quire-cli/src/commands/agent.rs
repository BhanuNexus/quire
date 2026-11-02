// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli agent …`: what agents did and suggested (design §B.4).
//! Owned by WS-B; WS-A provides `accept`, `reject` and `revert` through the
//! agent store.

use std::fmt::Write as _;

use quire_agent::{AgentStore, LedgerQuery, ProposalFilter, ProposalId, SystemClock};
use quire_app::vault::Vault;

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "agent",
    rank: 30,
    usage: "  quire-cli agent     proposals <vault> [--all] | show <vault> <id> | log <vault> [--actor ID] [--since UNIX_MS]
                      suggested changes waiting for review, one in detail, and what agents did on this device",
    run,
}];

fn store(vault: &str) -> Result<AgentStore, String> {
    let v = Vault::open_local(vault).map_err(|e| e.to_string())?;
    Ok(AgentStore::open(&v, quire_app::device::this_device(), std::sync::Arc::new(SystemClock)))
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn run(args: &[String]) -> Result<(), String> {
    let store = store(arg(args, 2)?)?;
    let out = match arg(args, 1)? {
        "proposals" => proposals(&store, args.iter().any(|a| a == "--all")),
        "show" => show(&store, arg(args, 3)?)?,
        "log" => {
            let since = flag(args, "--since")
                .map(|s| s.parse::<i64>().map_err(|_| format!("--since takes Unix milliseconds, not “{s}”")))
                .transpose()?;
            log(&store, &LedgerQuery { since, actor: flag(args, "--actor").map(str::to_string), run: None })
        }
        other => return Err(format!("unknown agent command “{other}”\n\n{}", super::usage())),
    };
    print!("{out}");
    Ok(())
}

/// One line per proposal: id, status, who, when, title and files.
pub(crate) fn proposals(store: &AgentStore, all: bool) -> String {
    let filter = if all { ProposalFilter::default() } else { ProposalFilter::pending() };
    let list = store.proposals(&filter);
    if list.is_empty() {
        return if all { "No proposals.\n".into() } else { "No suggested changes waiting for review.\n".into() };
    }
    let mut s = String::new();
    for p in list {
        let files: Vec<&str> = p.files.iter().map(quire_app::vault::NotePath::as_str).collect();
        let status = format!("{:?}", p.status).to_lowercase();
        let _ = writeln!(
            s,
            "{}  {status:<9} {:<16} {}  {}  ({})",
            p.id,
            p.actor.label,
            quire_app::chrono_fmt(p.created),
            p.title,
            files.join(", ")
        );
    }
    s
}

fn show(store: &AgentStore, id: &str) -> Result<String, String> {
    let id = ProposalId::parse(id).ok_or_else(|| format!("“{id}” isn't a proposal id (p_…)"))?;
    let p = store.proposal(&id).ok_or_else(|| format!("no proposal {id}"))?;
    let mut s = format!("{}\n{} · {} · {:?}\n", p.title, p.actor.label, quire_app::chrono_fmt(p.created), p.status);
    if !p.rationale.is_empty() {
        let _ = writeln!(s, "Why (as the agent wrote it): {}", p.rationale);
    }
    for c in &p.changes {
        let what = format!("{:?}", c.op).to_lowercase();
        let paths: Vec<String> = c.paths().iter().map(ToString::to_string).collect();
        let _ = writeln!(s, "\n{what} {}", paths.join(" → "));
        if let (Some(old), Some(new)) = (c.base_text(), c.new_text.as_deref()) {
            for l in quire_app::history_ops::diff(&old, new, 2) {
                let mark = match l.kind {
                    quire_app::history_ops::DiffKind::Added => "+",
                    quire_app::history_ops::DiffKind::Removed => "-",
                    quire_app::history_ops::DiffKind::Same => " ",
                    quire_app::history_ops::DiffKind::Gap => "…",
                };
                let _ = writeln!(s, "{mark} {}", l.text);
            }
        }
    }
    Ok(s)
}

fn log(store: &AgentStore, q: &LedgerQuery) -> String {
    let entries = store.log(q);
    if entries.is_empty() {
        return "Nothing logged on this device.\n".into();
    }
    let mut s = String::new();
    for e in entries {
        let path = e.path.as_ref().map(quire_app::vault::NotePath::as_str).unwrap_or_default();
        let to = e.to.as_ref().map(|t| format!(" → {t}")).unwrap_or_default();
        let _ = writeln!(s, "{}  {:<8} {:<20} {path}{to}", quire_app::chrono_fmt(e.at), format!("{:?}", e.op).to_lowercase(), e.actor.id);
    }
    s
}

#[cfg(test)]
mod tests {
    use quire_agent::{Actor, Change, ChangeSet, SubmitOutcome, Via};
    use quire_app::vault::NotePath;

    use super::*;

    #[test]
    fn proposals_written_through_submit_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Rope Notes.md"), "# Rope Notes\n").unwrap();
        let st = store(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(proposals(&st, false), "No suggested changes waiting for review.\n");
        let agent = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, &st.device().id);
        let cs = ChangeSet::one(Change::Edit { path: NotePath::new("Rope Notes.md"), new_text: "# Rope Notes\nUndo is cheap.\n".into() })
            .titled("Add undo details");
        let SubmitOutcome::Proposed { id, .. } = st.submit(&agent, cs).unwrap() else { panic!("a human note gets a proposal") };
        // A fresh store, as a second `quire-cli` run would open.
        let again = store(dir.path().to_str().unwrap()).unwrap();
        let listed = proposals(&again, false);
        assert!(listed.contains(id.as_str()) && listed.contains("Add undo details") && listed.contains("Rope Notes.md"), "{listed}");
        let shown = show(&again, id.as_str()).unwrap();
        assert!(shown.contains("+ Undo is cheap."), "{shown}");
        assert!(log(&again, &LedgerQuery::default()).contains("propose"));
        assert!(show(&again, "nope").is_err());
    }
}
