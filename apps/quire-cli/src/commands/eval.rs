// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli eval <suite> …` (design §B.6), built only with the `eval`
//! feature and never in releases. The suites and metrics are in
//! `quire_mcp::eval`; this writes `target/eval/<suite>-<date>.json`.

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "eval",
    rank: 90,
    usage: "  quire-cli eval      synthetic-edits [--seed N] [--k 6] [--json] | longmemeval-s <dataset.json> [--limit N] [--k 6] [--json]
                      measure retrieval: recall@k, citation spans, abstention, tokens, latency (writes target/eval/)",
    run,
}];

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn num(args: &[String], name: &str, default: u64) -> Result<u64, String> {
    flag(args, name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("{name} takes a number, not “{v}”")))
}

fn run(args: &[String]) -> Result<(), String> {
    let k = usize::try_from(num(args, "--k", 6)?).unwrap_or(6).clamp(1, 50);
    let suite = match arg(args, 1)? {
        "synthetic-edits" => quire_mcp::eval::synthetic(num(args, "--seed", 42)?, 300, 200),
        "longmemeval-s" => {
            let path = arg(args, 2)?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            quire_mcp::eval::longmemeval(&text, usize::try_from(num(args, "--limit", 500)?).unwrap_or(500))?
        }
        other => return Err(format!("unknown suite “{other}” (synthetic-edits, longmemeval-s)")),
    };
    let report = quire_mcp::eval::run(&suite, k);
    let dir = std::path::Path::new("target").join("eval");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("{}-{}.json", suite.name, chrono_date()));
    let pretty = serde_json_pretty(&report.json);
    std::fs::write(&file, &pretty).map_err(|e| format!("{}: {e}", file.display()))?;
    if args.iter().any(|a| a == "--json") {
        println!("{pretty}");
    } else {
        print!("{}", report.text);
        println!("  written to {}", file.display());
    }
    Ok(())
}

fn chrono_date() -> String {
    quire_app::chrono_fmt(quire_app::history_ops::now_ms()).split_whitespace().next().unwrap_or("today").to_string()
}

fn serde_json_pretty(v: &quire_mcp::eval::Json) -> String {
    quire_mcp::eval::pretty(v)
}
