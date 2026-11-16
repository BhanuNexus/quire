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
        "--help" | "-h" => println!("{}", commands::usage()),
        "help" => match args.get(1) {
            None => println!("{}", commands::usage()),
            Some(name) => {
                let c = commands::find(name).ok_or_else(|| commands::unknown_command(name))?;
                println!("{}", commands::command_usage(c));
            }
        },
        name => {
            let c = commands::find(name).ok_or_else(|| commands::unknown_command(name))?;
            // `quire-cli <command> --help` is help, whatever else follows; it
            // never opens a vault. (`serve` has a longer help of its own.)
            if c.name != "serve" && args.iter().skip(1).take(2).any(|a| matches!(a.as_str(), "--help" | "-h")) {
                println!("{}", commands::command_usage(c));
                return Ok(());
            }
            return (c.run)(args);
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            // 2: how the command was typed was wrong; 1: it ran and failed.
            if commands::was_usage_error() { ExitCode::from(2) } else { ExitCode::FAILURE }
        }
    }
}
