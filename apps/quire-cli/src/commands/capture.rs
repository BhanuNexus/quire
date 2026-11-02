// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli capture`: add a line to today's note. WS-B adds `--url`
//! (web capture into `raw/`, design §B.3) here.

use quire_app::vault::Vault;

use super::CliCommand;

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "capture",
    rank: 10,
    usage: "  quire-cli capture   <vault> <text…>         add a timestamped line to today's daily note (\\\"-\\\" reads stdin)",
    run: capture,
}];

fn capture(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let v = Vault::open_local(arg(1)?).map_err(|e| e.to_string())?;
    let mut text = args.get(2..).map(|a| a.join(" ")).unwrap_or_default();
    if text == "-" {
        text.clear();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map_err(|e| e.to_string())?;
    }
    let p = quire_app::capture::capture_now(&v, &text)?;
    println!("added to {p}");
    Ok(())
}
