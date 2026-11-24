// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Every read tool hides what `common::visible` hides, driven from the
//! registry: a tool added to `ToolRegistry::standard()` is checked here
//! without editing this file.
//!
//! Two vaults hold the same visible notes. The first also has notes an
//! agent must never see: a `deny` zone (`Private/`), hidden folders
//! (`.obsidian/`, `.trash/`) and a locked folder (`Journal/`, both locked
//! and unlocked in the process). Each read tool is called with arguments
//! built from its input schema, once with ordinary values and once per
//! argument with a value that names a hidden note. The replies of the two
//! vaults must be the same, after taking out what the call itself said: no
//! path, title, count, tag, property or snippet may tell the vaults apart.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds fixtures with std directly"
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use quire_agent::{AgentStore, DeviceInfo, FixedClock};
use quire_app::vault::Vault;
use quire_app::vault::lock::{Kdf, lock_folder};
use quire_mcp::tools::Access;
use quire_mcp::{Server, ServerConfig, ToolRegistry};
use serde_json::{Value, json};

const NOW_MS: i64 = 1_793_577_600_000;
const PASSPHRASE: &str = "pass phrase";

const POLICY: &str = "format = 1\ndefault = \"read\"\n[[zone]]\npath = \"Private/\"\nrole = \"private\"\nmode = \"deny\"\n";

/// The notes both vaults have. They link to the hidden ones, by name,
/// typed and in front matter, so a tool that follows links meets them.
const VISIBLE: &[(&str, &str)] = &[
    (
        "Ideas.md",
        "---\ntags: [shared]\nstatus: open\nup: \"[[Plan]]\"\n---\n# Ideas\n\nshared words here (Ideas.md), see [[Plan]], [[diary]], [[Day]] and [[hidden]].\n\nparent:: [[diary]]\n[supports:: [[Day]]]\n\n## Next\n\n- [ ] a visible task #shared\n- [x] done task 2026-11-01\n",
    ),
    (
        "Plan.md",
        "---\ntags: [shared, plan]\nstatus: draft\n---\n# Plan\n\nshared plan for [[Ideas]] (Ideas.md).\n\n- [ ] plan the shared work\n",
    ),
    ("wiki/Page.md", "# Page\n\nshared words in the wiki. [[Ideas]]\n"),
];

/// A hidden note: every place a tool could repeat it carries `zq`.
fn secret(name: &str) -> String {
    format!(
        "---\ntags: [secrettagzq, shared]\nsecretpropzq: secretvaluezq{name}\nstatus: secretstatuszq\nup: \"[[Ideas]]\"\n---\n# Secret title zq {name}\n\nshared words here secretbodyzq{name}, see [[Ideas]] (Ideas.md) and [[Plan]].\n\nparent:: [[Ideas]]\n[secretlinkzq:: [[Plan]]]\n\n## Secret heading zq\n\n- [ ] secrettaskzq {name} #shared #secrettagzq\n"
    )
}

/// What may tell the vaults apart if a tool lets it through.
/// (The visible notes link to `diary`, `Day` and `hidden` by name, and the
/// policy names `Private/`: those may appear.)
const MARKERS: &[&str] = &["zq", "Private/diary", ".obsidian/", ".trash/", "Journal/", ".qenc", "quire-lock"];

/// Hidden notes and names an agent may ask for.
const HIDDEN_ARGS: &[&str] = &[
    "Private/diary.md",
    "Private/diary",
    "diary",
    "Private",
    "Private/",
    ".obsidian/hidden.md",
    "hidden",
    ".obsidian",
    ".trash/20261101-120000 diary.md",
    "Journal/Day.md",
    "Day",
    "Journal",
    "Journal/Day.md.qenc",
    "Journal/quire-lock.json",
    "secretbodyzqdiary",
    "secrettagzq",
    "#secrettagzq",
    "secretpropzq",
    "secretlinkzq",
    "secretstatuszq",
    "group: secretpropzq",
    "from: #secrettagzq",
    "from: \"Private\"\ngroup: status",
    "status = secretstatuszq",
    // A condition with a line break would add view keys: sorted so a hidden
    // note comes first, then cut to one row before the caller hides it.
    "status != none\nsort: status desc\nlimit: 1",
];

/// Ordinary values, tried in order until a call succeeds in the plain vault.
const VISIBLE_ARGS: &[&str] = &[
    "Ideas.md",
    "Ideas",
    "shared words",
    "shared",
    "status",
    "group: status",
    "wiki",
    "up",
    "2026-11-01",
    "7d",
    "What are the shared words about?",
];

fn device() -> DeviceInfo {
    DeviceInfo { id: "d_vis".into(), name: "t".into() }
}

fn write(root: &Path, path: &str, text: &str) {
    let abs = root.join(path);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, text).unwrap();
}

/// Same modified time for every file, so lists sort and date the same.
fn settle_times(dir: &Path) {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_000_000);
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            settle_times(&path);
        } else {
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
        }
    }
}

/// A vault in `dir/vault`, with or without the hidden notes.
fn vault(dir: &Path, hidden: bool) -> PathBuf {
    let root = dir.join("vault");
    write(&root, ".quire/shared/agent-policy.toml", POLICY);
    for (path, text) in VISIBLE {
        write(&root, path, text);
    }
    if hidden {
        write(&root, "Private/diary.md", &secret("diary"));
        // Enough hidden notes to fill a page of hits: one that took a place
        // would push a visible note out.
        for i in 1..=10 {
            write(&root, &format!("Private/diary {i}.md"), &secret(&format!("diary{i}")));
        }
        write(&root, ".obsidian/hidden.md", &secret("hidden"));
        write(&root, ".trash/20261101-120000 diary.md", &secret("trashed"));
        write(&root, "Journal/Day.md", &secret("Day"));
        write(&root, "Journal/pic.png", "secretpicturezq");
        lock_folder(&Vault::open_local(&root).unwrap(), "Journal", Some(PASSPHRASE), Kdf::TEST, NOW_MS).unwrap();
    }
    settle_times(&root);
    root
}

/// Text of PDF pages, as the PDF reader would index it: one visible PDF in
/// both vaults, and with the hidden notes one in the deny zone and one in a
/// hidden folder, with the same words (so `search` with `kind: attachments`
/// or `all` ranks them together).
fn attachments(srv: &Server, hidden: bool) {
    use quire_app::index::attach::{AttachmentKind, AttachmentText, TextSource};
    let mut pdfs = vec![("wiki/scan.pdf", "Ideas.md shared words here, in a scanned page.")];
    if hidden {
        pdfs.push(("Private/diary.pdf", "Ideas.md shared words here secretpdfzq, shared words, Ideas.md."));
        pdfs.push((".obsidian/scan.pdf", "Ideas.md shared words here secretpdfzq, shared words, Ideas.md."));
    }
    for (path, text) in pdfs {
        srv.index().upsert_attachment(AttachmentText {
            path: quire_app::vault::NotePath::new(path),
            kind: AttachmentKind::Pdf,
            pages: vec![text.to_string()],
            source_hash: "ab".into(),
            by: TextSource::Embedded,
        });
    }
    srv.index().flush();
}

/// A read-only server over `root`, its policy approved on this device;
/// `unlock`: the locked folder is open in this process.
fn server(root: &Path, unlock: bool) -> Server {
    let state = root.parent().unwrap().join("state");
    let v = Vault::open_local(root).unwrap();
    let store = AgentStore::open(&v, device(), FixedClock::new(NOW_MS));
    store.set_state_dir(Some(state.clone()));
    let shown = store.policy_state().file_hash();
    store.approve_policy(shown.as_ref()).unwrap();
    let cfg = ServerConfig { device: Some(device()), state_dir: Some(state), clock: FixedClock::new(NOW_MS), ..ServerConfig::default() };
    let srv = if unlock {
        let v = quire_app::secure_state::open_vault(root).unwrap();
        v.unlock("Journal", PASSPHRASE).unwrap();
        Server::with_config(v, None, cfg)
    } else {
        Server::open_with(root.to_str().unwrap(), cfg).unwrap()
    };
    // A view hides notes, so it has no meaning search (`docs/design/0.14-agent-index.md` §4); the
    // plain vault, whose view hides nothing, would still have one. Off for both, so they compare.
    srv.index().set_meaning(false);
    attachments(&srv, root.join("Private").exists());
    srv
}

fn call(srv: &Server, name: &str, args: &Value) -> Value {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}});
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    serde_json::from_str(text.lines().next().unwrap()).unwrap()
}

fn failed(reply: &Value) -> bool {
    reply.get("error").is_some() || reply["result"]["isError"] == true
}

/// Every string the call itself carried: a reply may repeat those.
fn arg_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.is_empty() => {
            out.push(s.clone());
            // Also as a JSON string inside a string, and without `.md`.
            out.push(Value::String(s.clone()).to_string().trim_matches('"').to_string());
            if let Some(stem) = s.strip_suffix(".md") {
                out.push(stem.to_string());
            }
        }
        Value::Array(a) => a.iter().for_each(|x| arg_strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| arg_strings(x, out)),
        _ => {}
    }
}

/// The reply as text, with the call's own strings and the vault's folder
/// taken out. Order is not compared: notes of equal rank come out of hash
/// maps in any order, so lists are sorted, and so are the lines of a text
/// and the characters of a line. Scores are compared: `search` and `ask`
/// rank through the caller's view (`docs/design/0.14-agent-index.md`).
///
/// With a small limit (`shape`), which of several equal notes makes the cut
/// varies from run to run: only the shape of the reply is compared then,
/// how many items and lines, which still shows a hidden note taking a place.
fn normalized(reply: &Value, args: &Value, root: &Path, shape: bool) -> String {
    let mut own = Vec::new();
    arg_strings(args, &mut own);
    own.push(root.parent().unwrap().to_str().unwrap().to_string());
    own.sort_by_key(|s| std::cmp::Reverse(s.len()));
    canonical(reply, &own, shape).to_string()
}

fn canonical(v: &Value, own: &[String], shape: bool) -> Value {
    match v {
        Value::String(s) => {
            let mut text = s.clone();
            for o in own {
                text = text.replace(o.as_str(), "<arg>");
            }
            if shape {
                return json!(text.lines().count());
            }
            let mut lines: Vec<String> = text
                .lines()
                .map(|l| {
                    let mut c: Vec<char> = l.chars().collect();
                    c.sort_unstable();
                    c.into_iter().collect()
                })
                .collect();
            lines.sort_unstable();
            Value::String(lines.join("\n"))
        }
        Value::Array(a) => {
            let mut items: Vec<Value> = a.iter().map(|x| canonical(x, own, shape)).collect();
            items.sort_by_cached_key(Value::to_string);
            Value::Array(items)
        }
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), canonical(x, own, shape))).collect()),
        Value::Number(_) if shape => json!("n"),
        other => other.clone(),
    }
}

/// Whether a call asks for only a few items (`limit: 1`).
fn small_limit(args: &Value) -> bool {
    args.as_object().is_some_and(|m| m.values().any(|v| v.as_u64().is_some_and(|n| n < 10)))
}

/// The numbers of the sources in a reply (`[1] …`), in order.
fn source_numbers(reply: &Value) -> Vec<u64> {
    let text = reply.to_string();
    text.split('[').skip(1).filter_map(|part| part.split_once("] ").and_then(|(n, _)| n.parse().ok())).collect()
}

/// A value for a property of `schema`, from `s` where it takes a string.
fn value_for(schema: &Value, s: &str) -> Option<Value> {
    match schema["type"].as_str() {
        Some("string") => match schema["enum"].as_array() {
            Some(e) => e.iter().any(|x| x == s).then(|| json!(s)),
            None => Some(json!(s)),
        },
        Some("array") => value_for(&schema["items"], s).map(|v| json!([v])),
        _ => None,
    }
}

/// Other values worth trying for a property that takes no string.
fn other_values(schema: &Value) -> Vec<Value> {
    match schema["type"].as_str() {
        Some("boolean") => vec![json!(true), json!(false)],
        Some("integer" | "number") => vec![json!(1), json!(3), json!(100)],
        Some("string") => schema["enum"].as_array().cloned().unwrap_or_default(),
        Some("array") => schema["items"]["enum"].as_array().map(|e| e.iter().map(|x| json!([x])).collect()).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The ordinary arguments of a tool: each required property gets the first
/// value that works in the plain vault.
fn base_args(srv: &Server, name: &str, schema: &Value) -> Value {
    let mut args = json!({});
    let required: Vec<&str> = schema["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    for key in required {
        let prop = &schema["properties"][key];
        let candidates: Vec<Value> = VISIBLE_ARGS.iter().filter_map(|s| value_for(prop, s)).chain(other_values(prop)).collect();
        let mut chosen = candidates.first().cloned().unwrap_or_else(|| json!({}));
        for c in candidates {
            let mut a = args.clone();
            a[key] = c.clone();
            if !failed(&call(srv, name, &a)) {
                chosen = c;
                break;
            }
        }
        args[key] = chosen;
    }
    args
}

/// Every value worth trying for a property: hidden names, ordinary values
/// and the rest. An object (`search`'s `filters`) gets each of its own
/// properties in turn with every value of that.
fn values(prop: &Value) -> Vec<Value> {
    if prop["type"] == "object" {
        let Some(props) = prop["properties"].as_object() else { return Vec::new() };
        return props.iter().flat_map(|(key, sub)| values(sub).into_iter().map(move |v| json!({ key.as_str(): v }))).collect();
    }
    HIDDEN_ARGS.iter().chain(VISIBLE_ARGS).filter_map(|s| value_for(prop, s)).chain(other_values(prop)).collect()
}

/// The calls to make: the ordinary arguments, then each property in turn
/// with every hidden name and every other value it takes.
fn probes(base: &Value, schema: &Value) -> Vec<Value> {
    let mut out = vec![base.clone()];
    let Some(props) = schema["properties"].as_object() else { return out };
    for (key, prop) in props {
        for v in values(prop) {
            let mut a = base.clone();
            a[key] = v;
            out.push(a);
        }
    }
    out
}

#[test]
fn read_tools_never_tell_a_vault_with_hidden_notes_apart() {
    let plain_dir = tempfile::tempdir().unwrap();
    let plain_root = vault(plain_dir.path(), false);
    let plain = server(&plain_root, false);
    let hidden_dir = tempfile::tempdir().unwrap();
    let hidden_root = vault(hidden_dir.path(), true);
    let servers = [("locked", server(&hidden_root, false)), ("unlocked", server(&hidden_root, true))];
    // The PDF pages are searchable, so `kind: attachments` probes something.
    let pages = call(&plain, "search", &json!({"query": "Ideas.md", "filters": {"kind": "attachments"}}));
    assert!(pages.to_string().contains("scan.pdf p. 1"), "{pages}");

    let registry = ToolRegistry::standard();
    let read: Vec<_> = registry.all().map(|t| t.spec().clone()).filter(|s| matches!(s.access, Access::Read | Access::Ask)).collect();
    let names: Vec<&str> = read.iter().map(|s| s.name).collect();
    assert!(read.len() >= 19, "the registry's read tools: {names:?}");
    let mut leaks = Vec::new();
    let (mut calls, mut answered) = (0, 0);
    for spec in &read {
        let base = base_args(&plain, spec.name, &spec.input_schema);
        for args in probes(&base, &spec.input_schema) {
            let shape = small_limit(&args);
            let want = call(&plain, spec.name, &args);
            let want_text = normalized(&want, &args, &plain_root, shape);
            calls += 1;
            answered += usize::from(!failed(&want));
            for (how, srv) in &servers {
                let got = call(srv, spec.name, &args);
                let got_text = normalized(&got, &args, &hidden_root, shape);
                let marker = MARKERS.iter().find(|m| normalized(&got, &args, &hidden_root, false).contains(*m));
                let numbers = source_numbers(&got);
                let numbered = numbers.iter().enumerate().all(|(i, n)| *n == i as u64 + 1);
                if got_text != want_text || marker.is_some() || !numbered {
                    leaks.push(format!(
                        "{} {args} ({how}){}{}:\n  plain:  {want}\n  hidden: {got}",
                        spec.name,
                        marker.map(|m| format!(", says {m:?}")).unwrap_or_default(),
                        if numbered { String::new() } else { format!(", sources numbered {numbers:?}") }
                    ));
                }
            }
        }
    }
    assert!(leaks.is_empty(), "{} of {calls} calls tell the vaults apart:\n{}", leaks.len(), leaks.join("\n"));
    // Every call came from one client under one rule: one view each, built once and kept.
    for (how, srv) in [("plain", &plain), ("locked", &servers[0].1), ("unlocked", &servers[1].1)] {
        assert_eq!(srv.index().scope_count(), 1, "{how}: one rule, one view");
    }
    // Calls that name a hidden note fail as "not found" in both vaults; the rest must mostly work.
    assert!(answered * 3 > calls, "enough calls must succeed to prove anything: {answered} of {calls}");
    eprintln!("{} read tools, {calls} calls, {answered} answered", read.len());
}

/// What a client is told when it connects: the legacy `initialize` and the modern `server/discover`.
fn handshakes(srv: &Server) -> Vec<Value> {
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "probe", "version": "1"}}});
    let discover = json!({"jsonrpc": "2.0", "id": 2, "method": "server/discover", "params": {}});
    [init, discover]
        .iter()
        .map(|line| {
            let mut out = Vec::new();
            srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
            let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
            assert!(reply.get("error").is_none(), "{reply}");
            reply["result"].clone()
        })
        .collect()
}

/// `docs/design/0.14-agent-index.md` §1: the connection text counts the notes the caller may see, so a
/// vault with hidden notes tells an agent what the plain vault does.
#[test]
fn connection_info_does_not_count_hidden_notes() {
    let plain_dir = tempfile::tempdir().unwrap();
    let plain = server(&vault(plain_dir.path(), false), false);
    let hidden_dir = tempfile::tempdir().unwrap();
    let hidden_root = vault(hidden_dir.path(), true);
    let want = handshakes(&plain);
    for r in &want {
        let text = r["instructions"].as_str().unwrap();
        assert!(text.contains("(3 notes)"), "the plain vault has three notes: {text}");
    }
    for (how, srv) in [("locked", server(&hidden_root, false)), ("unlocked", server(&hidden_root, true))] {
        assert_eq!(handshakes(&srv), want, "{how}: the handshake of the vault with hidden notes differs from the plain vault's");
    }
}

/// The text of a tool's reply, with the vault's folder taken out.
fn reply_text(reply: &Value, root: &Path) -> String {
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default();
    text.replace(root.parent().unwrap().to_str().unwrap(), "<root>")
}

/// The leak itself (`docs/design/0.14-agent-index.md` §1): a word that hidden
/// notes use often is worth less in the whole vault, which would put the
/// visible note that uses it below one that doesn't. `search` and `ask` rank
/// as the plain vault does, in the same order.
#[test]
fn a_word_only_hidden_notes_use_often_does_not_reorder_search_or_ask() {
    let notes = |dir: &Path, hidden: bool| {
        let root = dir.join("vault");
        write(&root, ".quire/shared/agent-policy.toml", POLICY);
        write(&root, "Ropes.md", "# Ropes\n\nropewords ropewords ropewords treewords.\n");
        write(&root, "Trees.md", "# Trees\n\ntreewords treewords ropewords.\n");
        for (i, filler) in ["apples", "pears", "plums", "figs"].iter().enumerate() {
            write(&root, &format!("Other {i}.md"), &format!("# Other\n\n{filler} only.\n"));
        }
        if hidden {
            for i in 0..12 {
                write(&root, &format!("Private/note {i}.md"), &format!("# Note {i}\n\nropewords secretzq{i}.\n"));
            }
        }
        settle_times(&root);
        root
    };
    let plain_dir = tempfile::tempdir().unwrap();
    let plain_root = notes(plain_dir.path(), false);
    let plain = server(&plain_root, false);
    let hidden_dir = tempfile::tempdir().unwrap();
    let hidden_root = notes(hidden_dir.path(), true);
    let hidden = server(&hidden_root, false);
    for (tool, args) in [
        ("search", json!({"query": "ropewords treewords"})),
        ("search", json!({"query": "ropewords treewords", "limit": 1})),
        ("ask", json!({"question": "ropewords treewords"})),
        ("ask", json!({"question": "ropewords treewords", "k": 1})),
    ] {
        let want = reply_text(&call(&plain, tool, &args), &plain_root);
        let got = reply_text(&call(&hidden, tool, &args), &hidden_root);
        assert!(want.contains("Ropes.md"), "{tool} {args}: the plain vault ranks Ropes.md first: {want}");
        assert!(want.find("Ropes.md") < want.find("Trees.md").or(Some(usize::MAX)), "{tool} {args}: {want}");
        assert_eq!(got, want, "{tool} {args}: the hidden notes reorder the visible ones");
    }
}
