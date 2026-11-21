// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A symlinked note file made while the server runs is never shown to an agent
//! (`docs/design/0.14-agent-tools.md` §10). The vault listing skips links; the
//! watcher reports a link as gone and `VaultEvent::regular` turns a change to one
//! into a removal, so the index never reads through it, and a note swapped for a
//! link leaves the index. `Visibility` checks folders, not files, so this is the
//! rule that keeps a linked file out of every listing.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds fixtures with std directly"
)]
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use quire_agent::{AgentStore, DeviceInfo, FixedClock};
use quire_app::vault::{NotePath, Vault};
use quire_mcp::tools::Access;
use quire_mcp::{Server, ServerConfig, ToolRegistry};
use serde_json::{Value, json};

const NOW_MS: i64 = 1_793_577_600_000;
/// A deny zone, and a client that may see only `Notes/`.
const POLICY: &str = "format = 1\ndefault = \"read\"\n[[zone]]\npath = \"Private/\"\nrole = \"private\"\nmode = \"deny\"\n[clients.\"mcp:probe\"]\nallow_folders = [\"Notes/\"]\n";

fn device() -> DeviceInfo {
    DeviceInfo { id: "d_link".into(), name: "t".into() }
}

fn marked(marker: &str) -> String {
    format!(
        "---\ntags: [{marker}tag]\n{marker}prop: {marker}value\n---\n# {marker} title\n\nThe budget words are {marker}body here.\n\n- [ ] {marker}task\n"
    )
}

fn server(root: &Path) -> Server {
    let state = root.parent().unwrap().join("state");
    let v = Vault::open_local(root).unwrap();
    let store = AgentStore::open(&v, device(), FixedClock::new(NOW_MS));
    store.set_state_dir(Some(state.clone()));
    let shown = store.policy_state().file_hash();
    store.approve_policy(shown.as_ref()).unwrap();
    let cfg = ServerConfig {
        client_key: Some("probe".into()),
        device: Some(device()),
        state_dir: Some(state),
        clock: FixedClock::new(NOW_MS),
        ..ServerConfig::default()
    };
    let srv = Server::open_with(root.to_str().unwrap(), cfg).unwrap();
    srv.index().flush();
    srv
}

fn send(srv: &Server, line: &Value) -> Value {
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
    serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap()
}

fn call(srv: &Server, name: &str, args: &Value) -> Value {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    send(srv, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}}))
}

/// The note count of the connection text: "(N notes)".
fn handshake_count(srv: &Server) -> usize {
    let r = send(
        srv,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "probe", "version": "1"}}}),
    );
    let text = r["result"]["instructions"].as_str().unwrap();
    text.split_once(" notes)").and_then(|(a, _)| a.rsplit('(').next()).unwrap().parse().unwrap()
}

/// Every read tool of the registry, with arguments that make it work on this vault.
fn every_read_call() -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for t in ToolRegistry::standard().all() {
        let spec = t.spec();
        if !matches!(spec.access, Access::Read | Access::Ask) {
            continue;
        }
        let mut args = json!({});
        for key in spec.input_schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            args[key] = match key {
                "path" | "note" => json!("Notes/a.md"),
                "query" => json!("budget words"),
                "question" | "name" => json!("What are the budget words?"),
                "spec" => json!("fields: status"),
                "since" => json!("36500d"),
                "from" => json!("sha256:00000000000000000000000000000000"),
                other => panic!("{} needs `{other}`: add a value here", spec.name),
            };
        }
        out.push((spec.name, args));
    }
    out
}

/// What must never reach the agent.
const HIDDEN: &[&str] = &[
    "privatezq",
    "outsidezq",
    "hiddenzq",
    "otherzq",
    "Notes/to-deny.md",
    "Notes/to-outside.md",
    "Notes/to-hidden.md",
    "Notes/dangling.md",
    "Notes/to-other.md",
    "Notes/swap.md",
];

fn leaks(srv: &Server) -> Vec<String> {
    let mut found = Vec::new();
    for (tool, args) in every_read_call() {
        let reply = call(srv, tool, &args).to_string();
        found.extend(HIDDEN.iter().filter(|m| reply.contains(**m)).map(|m| format!("{tool}: {m}")));
    }
    found
}

#[test]
fn a_symlinked_note_made_while_the_server_runs_is_never_shown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("vault");
    for (path, text) in [
        (".quire/shared/agent-policy.toml", POLICY.to_string()),
        ("Private/diary.md", marked("privatezq")),
        (".hidden/x.md", marked("hiddenzq")),
        ("Other/x.md", marked("otherzq")),
        ("Notes/a.md", "# a\n\nThe budget words of a visible note. [[to-deny]] [[swap]]\n".to_string()),
        ("Notes/swap.md", "# swap\n\nswapvisible budget words.\n".to_string()),
    ] {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path), text).unwrap();
    }
    std::fs::write(root.parent().unwrap().join("outside.md"), marked("outsidezq")).unwrap();
    let srv = server(&root);
    let before = handshake_count(&srv);
    assert_eq!(before, 2, "Notes/a.md and Notes/swap.md");
    assert!(call(&srv, "list", &json!({})).to_string().contains("Notes/swap.md"));

    // While the server runs: links into a deny zone, out of the vault, into a hidden folder, to
    // nothing, into a folder this client may not see; and a listed note swapped for a link.
    let link = |target: &Path, name: &str| std::os::unix::fs::symlink(target, root.join(name)).unwrap();
    link(&root.join("Private/diary.md"), "Notes/to-deny.md");
    link(&root.parent().unwrap().join("outside.md"), "Notes/to-outside.md");
    link(&root.join(".hidden/x.md"), "Notes/to-hidden.md");
    link(&root.join("Nowhere/x.md"), "Notes/dangling.md");
    link(&root.join("Other/x.md"), "Notes/to-other.md");
    std::fs::remove_file(root.join("Notes/swap.md")).unwrap();
    link(&root.join("Private/diary.md"), "Notes/swap.md");
    // Then a plain note: once it is indexed, the watcher has reported what came before it.
    std::fs::write(root.join("Notes/flag.md"), "# flag\n").unwrap();
    let flag = NotePath::new("Notes/flag.md");
    let mut seen = false;
    for _ in 0..150 {
        std::thread::sleep(Duration::from_millis(100));
        // Each request takes in the watcher's events first.
        let _ = call(&srv, "vault_info", &json!({}));
        if srv.index().read().get(&flag).is_some() {
            seen = true;
            break;
        }
    }
    assert!(seen, "the watcher reported the new note");
    // A little longer for a rescan of anything the system merged.
    std::thread::sleep(Duration::from_millis(1_500));
    let found = leaks(&srv);
    assert!(found.is_empty(), "a symlinked note reached an agent: {found:?}");
    assert_eq!(handshake_count(&srv), before - 1 + 1, "the swapped note left, the flag came: the links count for nothing");
    let ix = srv.index().read();
    for p in ["Notes/to-deny.md", "Notes/to-outside.md", "Notes/to-hidden.md", "Notes/dangling.md", "Notes/to-other.md", "Notes/swap.md"] {
        assert!(ix.get(&NotePath::new(p)).is_none(), "{p} is in the index");
    }
}

/// `ask`'s second guard (as `search` has): a source the call's `Visibility` lets through but the
/// full check refuses is dropped before the sources are numbered. Here a note path in the index
/// that is a link into the deny zone on disk (put there directly, as nothing in Quire does now).
#[test]
fn ask_drops_a_source_the_full_check_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("vault");
    for (path, text) in [
        (".quire/shared/agent-policy.toml", POLICY.to_string()),
        ("Private/diary.md", marked("privatezq")),
        ("Notes/a.md", "# a\n\nThe budget words of a visible note.\n".to_string()),
    ] {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path), text).unwrap();
    }
    std::os::unix::fs::symlink(root.join("Private/diary.md"), root.join("Notes/link.md")).unwrap();
    let srv = server(&root);
    srv.index().upsert("Notes/link.md".into(), marked("privatezq"));
    srv.index().flush();
    let reply = call(&srv, "ask", &json!({"question": "What are the budget words?"}));
    let text = reply.to_string();
    assert!(!text.contains("privatezq") && !text.contains("Notes/link.md"), "{text}");
    assert!(text.contains("Notes/a.md"), "{text}");
    assert!(!text.contains("[2]"), "the one source left is [1]: {text}");
}

/// An agent's write to a note path that is a link: Quire's index never takes the text through the
/// link. Whether the write replaced the link with a file or left it (no change to make), the index
/// holds the path only as a regular file.
#[test]
fn a_write_to_a_link_never_indexes_through_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("vault");
    for (path, text) in [
        (".quire/shared/agent-policy.toml", "format = 1\ndefault = \"direct\"\n".to_string()),
        ("Notes/real.md", "---\nstatus: open\n---\n# real\n\nrealbody\n".to_string()),
    ] {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path), text).unwrap();
    }
    std::os::unix::fs::symlink(root.join("Notes/real.md"), root.join("Notes/link.md")).unwrap();
    let state = root.parent().unwrap().join("state");
    let v = Vault::open_local(&root).unwrap();
    let store = AgentStore::open(&v, device(), FixedClock::new(NOW_MS));
    store.set_state_dir(Some(state.clone()));
    store.approve_policy(store.policy_state().file_hash().as_ref()).unwrap();
    let cfg = ServerConfig {
        allow_write: true,
        device: Some(device()),
        state_dir: Some(state),
        clock: FixedClock::new(NOW_MS),
        ..ServerConfig::default()
    };
    let srv = Server::with_config(v, None, cfg);
    srv.index().flush();
    for (tool, args) in [
        // No change to make: the same value again.
        ("set_properties", json!({"path": "Notes/link.md", "set": {"status": "open"}})),
        ("append", json!({"path": "Notes/link.md", "text": "more"})),
    ] {
        let reply = call(&srv, tool, &args);
        let link_left = std::fs::symlink_metadata(root.join("Notes/link.md")).unwrap().file_type().is_symlink();
        let indexed = srv.index().read().get(&NotePath::new("Notes/link.md")).is_some();
        assert!(!(link_left && indexed), "{tool}: the index took a link's text: {reply}");
    }
}
