// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tasks and metadata from the command line. Owned by WS-E, which adds
//! `tags rename` and `props rename` here (design §E.1).

use super::{CliCommand, load};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "tasks",
    rank: 9,
    usage: "  quire-cli tasks     <vault>                 open tasks across the vault",
    run: tasks,
}];

fn tasks(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let (_, svc, _) = load(arg(1)?)?;
    let tasks = svc.read().tasks(false);
    for (p, t) in tasks {
        println!("[ ] {}  ({})", t.text, p.stem());
    }
    Ok(())
}
