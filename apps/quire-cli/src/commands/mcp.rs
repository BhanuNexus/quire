// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli mcp`: the MCP server on stdio (`quire-mcp`). Owned by WS-B,
//! which adds `--client`, `serve-http`, `approve` and `token` (design §B.4).

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "mcp",
    rank: 14,
    usage: "  quire-cli mcp       <vault> [--allow-write] MCP server on stdio for AI assistants (read-only by default)",
    run,
}];

fn run(args: &[String]) -> Result<(), String> {
    let srv = quire_mcp::Server::open(arg(args, 1)?, args.iter().any(|a| a == "--allow-write"))?;
    let stdin = std::io::stdin();
    srv.serve(stdin.lock(), std::io::stdout()).map_err(|e| e.to_string())
}
