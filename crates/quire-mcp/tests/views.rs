// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! An agent's view follows the vault's locked folders as each caller sees
//! them (`docs/design/0.14-agent-index.md` §3.5; Fable's review, S3).
#![allow(
    clippy::unwrap_used,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap to fail the test"
)]

use quire_app::vault::lock::KEY_FILE;
use quire_app::vault::{NotePath, Vault};
use quire_mcp::Server;
use serde_json::{Value, json};

fn search(srv: &Server, query: &str) -> Vec<String> {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "search", "arguments": {"query": query}, "_meta": meta}});
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
    let hits = reply["result"]["structuredContent"]["hits"].as_array().cloned().unwrap_or_default();
    hits.iter().map(|h| h["path"].as_str().unwrap_or_default().to_string()).collect()
}

/// A folder the view found plain and that was locked since (Fable's review,
/// S3). The caller who finds it locked has another fingerprint, so another
/// view: the old one is never served to it. (The other way round, a folder
/// a view's predicate found locked and that was unlocked since, is
/// `scope_tests.rs` `the_worker_never_asks_for_a_storage_call`: here a
/// locked folder's notes reach the index only through a load, which builds
/// every view again.)
#[test]
fn a_folder_locked_since_the_view_met_it_is_hidden() {
    let vault = Vault::in_memory(&[("A/x.md", "# X\n\nalpha words\n"), ("F/y.md", "# Y\n\nbeta words\n")]);
    let srv = Server::with_vault(vault.clone(), None, false);
    srv.index().set_meaning(false);
    assert_eq!(search(&srv, "beta"), ["F/y.md"]);
    vault.storage().write_bytes_atomic(&NotePath::new(format!("F/{KEY_FILE}")), b"{}").unwrap();
    // The index still has the note (no watcher here): only the view can hide it.
    assert!(search(&srv, "beta").is_empty());
    assert_eq!(search(&srv, "words"), ["A/x.md"]);
    assert_eq!(srv.index().scope_count(), 2, "a new view for the new fingerprint");
}
