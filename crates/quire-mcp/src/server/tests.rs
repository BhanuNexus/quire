// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recorded sessions: 0.8-era clients get what 0.8 sent; modern clients
//! get the 2026-07-28 shapes.

use quire_app::vault::NotePath;

use super::*;

fn vault() -> Vault {
    Vault::in_memory(&[
        ("Rope Notes.md", "# Rope Notes\n## Undo\nRopes make undo cheap because subtrees are shared. #cs\n"),
        ("Ideas.md", "# Ideas\nWrite about [[Rope Notes]].\n"),
    ])
}

fn session(allow_write: bool, lines: &[Value]) -> (Vec<Value>, Vault) {
    let vault = vault();
    let srv = Server::with_vault(vault.clone(), None, allow_write);
    let input = lines.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(input), &mut out).unwrap();
    let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    (replies, vault)
}

fn call(id: u64, name: &str, args: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

fn modern(id: u64, method: &str, params: &Value) -> Value {
    let mut p = params.clone();
    p["_meta"] = json!({ protocol::META_VERSION: MODERN, "clientInfo": {"name": "modern-client", "version": "2"} });
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": p})
}

fn body(v: &Value) -> &str {
    v["result"]["content"][0]["text"].as_str().unwrap_or_default()
}

#[test]
fn read_only_session() {
    let (r, _) = session(
        false,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            call(3, "search_notes", &json!({"query": "undo"})),
            call(4, "read_note", &json!({"note": "Rope Notes"})),
            call(5, "backlinks", &json!({"note": "Rope Notes.md"})),
            call(6, "ask_notes", &json!({"question": "why is undo cheap?"})),
            call(7, "append_to_note", &json!({"note": "Ideas", "text": "x"})),
            json!({"jsonrpc": "2.0", "id": 8, "method": "bogus"}),
        ],
    );
    assert_eq!(r.len(), 8, "the notification gets no reply");
    assert_eq!(r[0]["result"]["protocolVersion"], "2025-06-18");
    let names: Vec<&str> = r[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"search_notes") && !names.contains(&"create_note"));
    assert!(body(&r[2]).contains("Rope Notes.md"));
    assert!(body(&r[3]).contains("subtrees are shared"));
    assert!(body(&r[4]).contains("Ideas.md"));
    assert!(body(&r[5]).contains("[[Rope Notes#Undo]]"));
    assert_eq!(r[6]["result"]["isError"], true);
    assert_eq!(r[7]["error"]["code"], -32601);
}

#[test]
fn writes_are_gated_and_versioned() {
    let (r, vault) = session(
        true,
        &[
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "claude-code", "version": "1"}}}),
            call(1, "create_note", &json!({"title": "From AI", "content": "hello", "folder": "Inbox"})),
            call(2, "append_to_note", &json!({"note": "Ideas", "text": "- added by AI"})),
            call(3, "replace_in_note", &json!({"note": "Ideas", "find": "Write", "replace": "Draft"})),
            call(4, "search_notes", &json!({"query": "hello"})),
            call(5, "create_note", &json!({"title": "sneaky", "content": "x", "folder": ".quire/shared"})),
        ],
    );
    assert!(body(&r[1]).contains("Inbox/From AI.md"));
    let ideas = vault.read(&NotePath::new("Ideas.md")).unwrap();
    assert!(ideas.contains("- added by AI") && ideas.starts_with("# Ideas\nDraft about"));
    let h = vault.history(&NotePath::new("Ideas.md"));
    assert!(h.len() >= 2);
    assert_eq!(vault.read_snapshot(h.last().unwrap()).unwrap(), "# Ideas\nWrite about [[Rope Notes]].\n", "original kept");
    assert!(body(&r[4]).contains("From AI.md"), "index updated after writes");
    // New in 0.9: who wrote it is recorded with the version and in the ledger.
    let prov = quire_agent::Provenance::from_json(&vault.read_provenance(&h[0]).unwrap()).unwrap();
    assert_eq!(prov.actor.id, "mcp:claude-code");
    assert!(prov.actor.client.as_ref().is_some_and(|c| c.self_reported));
    let store = AgentStore::open(&vault, quire_app::device::this_device(), std::sync::Arc::new(SystemClock));
    assert_eq!(store.log(&quire_agent::LedgerQuery::default()).len(), 3);
    assert_eq!(r[5]["result"]["isError"], true, "never under .quire/");
}

/// What 0.8.0 answered, recorded from its implementation.
#[test]
fn answers_0_8_clients_exactly_as_0_8_did() {
    for (allow_write, fixture) in
        [(false, include_str!("../../tests/fixtures/mcp-0.8-read.jsonl")), (true, include_str!("../../tests/fixtures/mcp-0.8-write.jsonl"))]
    {
        let (r, _) = session(
            allow_write,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
                call(4, "list_tags", &json!({})),
                call(5, "nope", &json!({})),
                json!({"jsonrpc": "2.0", "id": 6, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}),
            ],
        );
        let mut expected: Vec<Value> = fixture.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        for e in &mut expected {
            if let Some(v) = e.pointer_mut("/result/serverInfo/version") {
                *v = env!("CARGO_PKG_VERSION").into();
            }
        }
        assert_eq!(r, expected, "allow_write = {allow_write}");
    }
}

#[test]
fn modern_clients_discover_list_and_call() {
    let (r, _) = session(
        false,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}),
            modern(2, "tools/list", &json!({})),
            modern(3, "tools/call", &json!({"name": "read_note", "arguments": {"note": "Rope Notes"}})),
            modern(4, "tools/call", &json!({"name": "read_note", "arguments": {"note": "Nope"}})),
            modern(5, "ping", &json!({})),
            json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {"_meta": {protocol::META_VERSION: "2030-01-01"}}}),
        ],
    );
    let d = &r[0]["result"];
    assert_eq!(d["supportedVersions"][0], MODERN);
    assert_eq!(d["resultType"], "complete");
    assert_eq!(d["_meta"][protocol::META_SERVER_INFO]["name"], "quire");
    assert_eq!(d["cacheScope"], "private");
    let list = &r[1]["result"];
    assert_eq!(list["ttlMs"], 300_000);
    assert_eq!(list["tools"][0]["title"], "Search notes");
    assert_eq!(list["tools"][0]["annotations"]["readOnlyHint"], true);
    assert_eq!(r[2]["result"]["isError"], false);
    assert_eq!(r[2]["result"]["resultType"], "complete");
    assert_eq!(r[3]["result"]["isError"], true);
    assert_eq!(r[3]["result"]["structuredContent"]["error"], "not_found");
    assert_eq!(r[4]["error"]["code"], code::METHOD_NOT_FOUND, "ping is gone in 2026-07-28");
    assert_eq!(r[5]["error"]["code"], code::INVALID_PARAMS, "no version and no initialize");
    assert_eq!(r[6]["error"]["code"], code::UNSUPPORTED_VERSION);
    assert_eq!(r[6]["error"]["data"]["requested"], "2030-01-01");
}

#[test]
fn a_legacy_session_may_still_probe_and_name_versions() {
    let (r, _) = session(
        false,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "server/discover", "params": {}}),
            modern(3, "tools/list", &json!({})),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        ],
    );
    assert_eq!(r[1]["result"]["supportedVersions"][0], MODERN);
    assert_eq!(r[2]["result"]["resultType"], "complete", "a request naming 2026-07-28 is modern");
    assert!(r[3]["result"].get("resultType").is_none() && r[3]["result"]["tools"][0].get("title").is_none());
}
