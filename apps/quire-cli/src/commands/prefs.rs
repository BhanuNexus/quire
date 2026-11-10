// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `prefs-copy`: take a vault's preferences (`.quire/shared/vault.toml`) to another vault (0.12).

use quire_app::vault::{
    VaultPrefs,
    prefs::{self, COPY_SECTIONS},
};

use super::CliCommand;

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "prefs-copy",
    rank: 52,
    usage: "  quire-cli prefs-copy <from-vault> <to-vault> [section]...
                      copy vault preferences (periodic notes, tag colours, git message and timing, lint, staleness, citations, links, tasks;
                      never git auto_commit, pull_on_start or push_after_commit)
                      to another vault; name sections to copy others (sort, order, icons, web)",
    run: prefs_copy,
}];

fn prefs_copy(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (from, to) = (arg(1)?, arg(2)?);
    let names: Vec<&str> = args.iter().skip(3).map(String::as_str).collect();
    let open = |p: &str| -> Result<quire_app::vault::Vault, String> {
        if !std::path::Path::new(p).is_dir() {
            return Err(format!("{p} is not a folder"));
        }
        quire_app::secure_state::open_vault(std::path::Path::new(p))
    };
    let (src, dst) = (open(from)?, open(to)?);
    let mut prefs = VaultPrefs::load(&dst);
    let copied = prefs.copy_from(&VaultPrefs::load(&src), &names).map_err(|e| e.to_string())?;
    if copied.is_empty() {
        println!("nothing to copy: {from} has none of {}", if names.is_empty() { COPY_SECTIONS.join(", ") } else { names.join(", ") });
        return Ok(());
    }
    prefs.save(&dst).map_err(|e| e.to_string())?;
    println!("copied {} to {to}", copied.join(", "));
    for (section, keys) in prefs::NOT_COPIED {
        if copied.iter().any(|c| c == section) {
            println!("  not copied from [{section}]: {} (they commit or push by themselves; set them per vault)", keys.join(", "));
        }
    }
    Ok(())
}
