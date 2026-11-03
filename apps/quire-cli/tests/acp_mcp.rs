// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The MCP server Quire offers an external (ACP) agent starts with the
//! arguments the agent is given, and the agent's tool calls are recorded as
//! the same actor as its file requests (`acp:<name>`).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use quire_agent::Provenance;
use quire_app::vault::Vault;
use quire_assistant::acp::{AcpAgent, mcp_args};
use serde_json::{Value, json};

#[test]
fn the_mcp_server_offered_to_an_acp_agent_starts_and_acts_as_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    std::fs::write(root.join("Ideas.md"), "# Ideas\n").unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let agent = AcpAgent { command: "/usr/local/bin/claude-code-acp".into(), label: "Claude Code".into(), ..AcpAgent::default() };
    let key = agent.actor_id();
    assert_eq!(key, "acp:claude-code");
    let mut child = Command::new(env!("CARGO_BIN_EXE_quire-cli"))
        .args(mcp_args(&root, &key))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("APPDATA", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "1"}});
    let msgs = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "create", "arguments": {"path": "wiki/From ACP.md", "content": "hello"}, "_meta": meta}}),
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for m in &msgs {
            writeln!(stdin, "{m}").unwrap();
        }
    }
    let out: Vec<Value> =
        BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    let status = child.wait().unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err).unwrap();
    assert!(status.success(), "quire-cli mcp refused the arguments ACP builds: {err}");
    let r = out.iter().find(|m| m["id"] == 1).unwrap_or_else(|| panic!("no answer: {out:?} {err}"));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let vault = Vault::open_local(&root).unwrap();
    assert_eq!(vault.read(&"wiki/From ACP.md".into()).unwrap(), "hello");
    // Who wrote it, from the version's provenance (the CLI has its own device id).
    let h = vault.history(&"wiki/From ACP.md".into());
    let prov = Provenance::from_json(&vault.read_provenance(&h[0]).unwrap()).unwrap();
    assert_eq!(prov.actor.id, "acp:claude-code");
    assert!(!prov.actor.unverified);
}

#[test]
fn client_keys_are_checked_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    for (key, ok) in
        [("claude-code", true), ("acp:claude-code", true), ("mcp:cursor", true), ("other:x", false), ("a b", false), ("acp:", false)]
    {
        let out = Command::new(env!("CARGO_BIN_EXE_quire-cli"))
            .args(["mcp", &dir.path().to_string_lossy(), "--client", key])
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join(".config"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.success(), ok, "{key}: {}", String::from_utf8_lossy(&out.stderr));
    }
}
