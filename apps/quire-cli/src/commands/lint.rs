// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli lint <vault> [--json]` (design §A.8): the vault checks the app
//! runs in the background, printed (or as JSON). Owned by WS-A.

use std::fmt::Write as _;

use quire_agent::lint::{Finding, Level, LintNote};
use quire_agent::{AgentStore, SystemClock};
use quire_app::vault::Vault;

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "lint",
    rank: 31,
    usage: "  quire-cli lint      <vault> [--json]
                      check the vault: broken links, orphans, missing pages and properties, an out-of-date index",
    run,
}];

fn run(args: &[String]) -> Result<(), String> {
    let vault = Vault::open_local(arg(args, 1)?).map_err(|e| e.to_string())?;
    let json = args.iter().any(|a| a == "--json");
    let store = AgentStore::open(&vault, quire_app::device::this_device(), std::sync::Arc::new(SystemClock));
    let notes: Vec<LintNote> = vault
        .notes()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|p| {
            let text = vault.read(&p).ok()?;
            Some(LintNote::from_source(p, text))
        })
        .collect();
    let findings = quire_app::agent_state::lint::check_vault(&vault, &store, &notes);
    print!("{}", if json { to_json(&findings, notes.len()) } else { to_text(&findings, notes.len()) });
    Ok(())
}

/// One section per check, one line per finding.
pub(crate) fn to_text(findings: &[Finding], notes: usize) -> String {
    let mut s = String::new();
    if findings.is_empty() {
        let _ = writeln!(s, "Checked {notes} notes: nothing to report.");
        return s;
    }
    let mut last = None;
    for f in findings {
        if last != Some(f.check) {
            let n = findings.iter().filter(|x| x.check == f.check).count();
            let _ = writeln!(
                s,
                "{}{} ({n}){}",
                if last.is_some() { "\n" } else { "" },
                f.check.title(),
                if f.level == Level::Warn { " — warning" } else { "" }
            );
            last = Some(f.check);
        }
        let fix = f.fix_label().map(|l| format!("  [fix: {l}]")).unwrap_or_default();
        let _ = writeln!(s, "  {}  {}{fix}", f.path, f.detail);
    }
    let _ = writeln!(s, "\n{} finding{} in {notes} notes.", findings.len(), if findings.len() == 1 { "" } else { "s" });
    s
}

/// `{"notes": n, "findings": [{check, level, path, detail, fix}]}`.
pub(crate) fn to_json(findings: &[Finding], notes: usize) -> String {
    let mut s = quire_agent::lint::to_json(findings, notes);
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_findings_and_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.md"), "# A\n[[Missing]]\n").unwrap();
        std::fs::write(dir.path().join("B.md"), "# B\n[[A]]\n").unwrap();
        let vault = Vault::open_local(dir.path()).unwrap();
        let store =
            AgentStore::open(&vault, quire_agent::DeviceInfo { id: "d".into(), name: "D".into() }, std::sync::Arc::new(SystemClock));
        let notes: Vec<LintNote> =
            vault.notes().unwrap().into_iter().map(|p| LintNote::from_source(p.clone(), vault.read(&p).unwrap())).collect();
        let f = quire_app::agent_state::lint::check_vault(&vault, &store, &notes);
        let text = to_text(&f, notes.len());
        assert!(text.starts_with("Broken links (1)\n  A.md  1 link doesn't resolve: [[Missing]]"), "{text}");
        let j = to_json(&f, notes.len());
        assert!(j.contains("\"notes\": 2") && j.contains("\"check\": \"broken-link\""), "{j}");
        assert_eq!(to_text(&[], 3), "Checked 3 notes: nothing to report.\n");
    }
}
