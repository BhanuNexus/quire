// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Headless frontend over the same core the desktop app uses.
//!
//! Subcommands live in `commands/`, one module per area, each with a table
//! of the commands it adds.

mod commands;

use std::process::ExitCode;

fn run(args: &[String]) -> Result<(), String> {
    match commands::arg(args, 0)? {
        "--version" | "-V" | "version" => {
            println!("quire-cli {}{}", env!("CARGO_PKG_VERSION"), if cfg!(feature = "typeset") { "" } else { " (Lite)" });
        }
        "--help" | "-h" | "help" => println!("{}", commands::usage()),
        name => return commands::find(name).map_or_else(|| Err(commands::usage()), |c| (c.run)(args)),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
