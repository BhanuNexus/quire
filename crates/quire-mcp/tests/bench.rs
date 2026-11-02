// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The MCP read-tool budget (design §9): on 10,000 notes, `view` within
//! 10 ms and `search` p95 within 50 ms, and results within their token
//! bound. Budgets are for release builds, so this is ignored by default:
//! `cargo test -p quire-mcp --release --test bench -- --ignored` (set
//! `QUIRE_BENCH_ENFORCE=1` to fail when over budget).
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use quire_agent::DeviceInfo;
use quire_app::intel::chat::CancelFlag;
use quire_app::vault::Vault;
use quire_mcp::{Conn, Reply, Server, ServerConfig};
use serde_json::{Value, json};

fn call(srv: &Server, conn: &Conn, tool: &str, args: &Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args,
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
    match srv.handle(&msg, conn, &CancelFlag::new()) {
        Reply::Response(v) => v["result"].clone(),
        _ => Value::Null,
    }
}

#[test]
#[ignore = "a benchmark; run in release"]
fn read_tools_meet_their_budget_on_10k_notes() {
    let words = ["rope", "undo", "tree", "editor", "buffer", "cursor", "merge", "sync", "note", "index", "query", "vault"];
    let notes: Vec<(String, String)> = (0..10_000)
        .map(|i| {
            let mut t = format!("# Note {i}\n\n## Part one\n");
            for j in 0..30 {
                let _ = write!(t, "{} {} {} {i} ", words[(i + j) % words.len()], words[(i * 7 + j) % words.len()], words[j % words.len()]);
            }
            t.push_str("\n\n## Part two\nMore text about ropes and undo.\n");
            (format!("Folder {}/Note {i}.md", i % 50), t)
        })
        .collect();
    let files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let srv = Server::with_config(
        Vault::in_memory(&files),
        None,
        ServerConfig { device: Some(DeviceInfo { id: "d".into(), name: "b".into() }), ..ServerConfig::default() },
    );
    let conn = Conn::stdio(srv.new_conn());
    let mut view = Vec::new();
    let mut search = Vec::new();
    for i in 0..50 {
        let t = Instant::now();
        let r = call(&srv, &conn, "view", &json!({"path": format!("Folder {}/Note {}.md", (i * 197) % 50, i * 197), "max_tokens": 4000}));
        view.push(t.elapsed());
        assert!(quire_mcp::bounds::est_tokens(r["content"][0]["text"].as_str().unwrap()) <= 4_200);
        let t = Instant::now();
        let r = call(
            &srv,
            &conn,
            "search",
            &json!({"query": format!("{} {}", words[i % words.len()], words[(i + 3) % words.len()]), "max_tokens": 2000}),
        );
        search.push(t.elapsed());
        assert!(quire_mcp::bounds::est_tokens(r["content"][0]["text"].as_str().unwrap()) <= 2_200);
    }
    view.sort();
    search.sort();
    let (v50, s95) = (view[view.len() / 2], search[search.len() * 95 / 100]);
    eprintln!("view p50 {v50:?}, search p95 {s95:?}");
    if std::env::var_os("QUIRE_BENCH_ENFORCE").is_some() {
        assert!(v50 <= Duration::from_millis(10), "view {v50:?}");
        assert!(s95 <= Duration::from_millis(50), "search {s95:?}");
    }
}
