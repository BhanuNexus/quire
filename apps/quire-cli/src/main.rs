// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Headless frontend over the same core the desktop app uses.
//!
//! Subcommands live in `commands/`, one module per area, each with a table
//! of the commands it adds.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line program: its results go to stdout, its errors and progress to stderr"
)]

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
            // A standalone `--help` or `-h` anywhere before a `--` is help, and
            // nothing runs (nothing is opened, nothing is written). After `--`,
            // words are text, so `capture <vault> -- -h` captures "-h".
            if args.iter().skip(1).take_while(|a| *a != "--").any(|a| matches!(a.as_str(), "--help" | "-h")) {
                println!("{}", commands::command_usage(c));
                return Ok(());
            }
            // The first `--` splits options from literal operands. It is not
            // an argument itself; commands read flags only from the options
            // part (`commands::options`), so after it every word is text for
            // every command.
            let mut rest = args.to_vec();
            if let Some(i) = rest.iter().skip(1).position(|a| a == "--") {
                rest.remove(i + 1);
                commands::set_literal_from(i + 1);
            }
            return (c.run)(&rest);
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
