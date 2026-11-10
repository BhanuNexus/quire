// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.11 security over MCP: a client limited to tags and folders never sees
//! anything else (a seeded property test over every read tool), locked
//! folders are invisible, a revoked client is refused, and "AI off" refuses
//! everyone.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fmt::Write as _;

use quire_agent::{AgentStore, DeviceInfo, FixedClock};
use quire_app::vault::Vault;
use quire_app::vault::lock::{Kdf, lock_folder};
use quire_mcp::{Server, ServerConfig};
use serde_json::{Value, json};

fn device() -> DeviceInfo {
    DeviceInfo { id: "d_conn".into(), name: "t".into() }
}

/// A server for `root` whose policy file is approved on this device, for
/// the stdio client `--client probe` (or unnamed).
fn server(root: &std::path::Path, client: Option<&str>) -> Server {
    let state = root.parent().unwrap().join("state");
    let cfg = ServerConfig {
        allow_write: true,
        client_key: client.map(str::to_string),
        device: Some(device()),
        state_dir: Some(state.clone()),
        ..ServerConfig::default()
    };
    let vault = Vault::open_local(root).unwrap();
    let store = AgentStore::open(&vault, device(), FixedClock::new(1_793_577_600_000));
    store.set_state_dir(Some(state));
    let shown = store.policy_state().file_hash();
    store.approve_policy(shown.as_ref()).unwrap();
    Server::open_with(root.to_str().unwrap(), cfg).unwrap()
}

fn req(id: u64, method: &str, params: &Value) -> Value {
    let mut p = params.clone();
    p["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": p})
}

fn call(name: &str, args: &Value) -> (String, Value) {
    ("tools/call".into(), json!({"name": name, "arguments": args}))
}

fn run(srv: &Server, calls: &[(String, Value)]) -> Vec<Value> {
    let lines: Vec<String> = calls.iter().enumerate().map(|(i, (m, p))| req(i as u64 + 1, m, p).to_string()).collect();
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(lines.join("\n")), &mut out).unwrap();
    String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

const FOLDERS: [&str; 4] = ["", "Work", "Home", "Work/Deep"];
const TAGS: [&str; 5] = ["work", "home", "work/q4", "misc", "workshop"];

struct Note {
    path: String,
    marker: String,
    tags: Vec<&'static str>,
}

/// Whether the client may see a note: under an allowed folder, or with an
/// allowed tag (a tag `work/q4` counts as `work`; `workshop` doesn't).
fn expected(n: &Note, tags: Option<&[&str]>, folders: Option<&[&str]>) -> bool {
    let in_folder = folders.is_some_and(|fs| fs.iter().any(|f| n.path.starts_with(&format!("{f}/"))));
    let tagged = tags.is_some_and(|ts| ts.iter().any(|t| n.tags.iter().any(|x| x == t || x.starts_with(&format!("{t}/")))));
    in_folder || tagged
}

#[test]
fn a_client_limited_to_tags_and_folders_never_sees_the_rest() {
    let mut leaks_checked = 0;
    for seed in 1..=12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
        let n = 10 + rng.below(8);
        let mut notes: Vec<Note> = Vec::new();
        for i in 0..n {
            let folder = FOLDERS[rng.below(FOLDERS.len())];
            let path = if folder.is_empty() { format!("Note{i}.md") } else { format!("{folder}/Note{i}.md") };
            let mut tags = Vec::new();
            for _ in 0..rng.below(3) {
                tags.push(TAGS[rng.below(TAGS.len())]);
            }
            notes.push(Note { path, marker: format!("zqx{i}secret"), tags });
        }
        for (i, note) in notes.iter().enumerate() {
            // A property holding the marker too: views group by it (`query_view`).
            let tags_line = note.tags.first().map(|t| format!("tags: [{t}]\n")).unwrap_or_default();
            let fm = format!("---\n{tags_line}owner: {}\n---\n", note.marker);
            let mut inline = String::new();
            for t in note.tags.iter().skip(1) {
                let _ = write!(inline, " #{t}");
            }
            let mut links = String::new();
            for _ in 0..2 {
                let _ = write!(links, " [[Note{}]]", rng.below(n));
            }
            // 0.12: typed links too, inline and in front matter, and a field
            // named after the marker (a hidden note's field names are hidden).
            let fm = fm.replacen("---\n", &format!("---\nup: \"[[Note{}]]\"\n", rng.below(n)), 1);
            let typed = format!(
                "parent:: [[Note{}]]\n[supports:: [[Note{}]]] and [{}:: [[Note{}]]]\n",
                rng.below(n),
                rng.below(n),
                note.marker,
                rng.below(n)
            );
            let text =
                format!("{fm}# Note{i}\nshared words here {}{inline}\nSee{links}.\n{typed}- [ ] task {}\n", note.marker, note.marker);
            let abs = root.join(&note.path);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(abs, text).unwrap();
        }
        let tags: Option<Vec<&str>> = rng.coin().then(|| (0..=rng.below(2)).map(|_| TAGS[rng.below(TAGS.len())]).collect());
        let folders: Option<Vec<&str>> = (tags.is_none() || rng.coin()).then(|| vec![FOLDERS[1 + rng.below(3)]]);
        let list = |v: &Option<Vec<&str>>, suffix: &str| {
            v.as_ref().map(|v| format!("[{}]", v.iter().map(|x| format!("\"{x}{suffix}\"")).collect::<Vec<_>>().join(", ")))
        };
        let mut policy = String::from("format = 1\ndefault = \"read\"\n[clients.\"mcp:probe\"]\n");
        if let Some(t) = list(&tags, "") {
            let _ = writeln!(policy, "allow_tags = {t}");
        }
        if let Some(f) = list(&folders, "/") {
            let _ = writeln!(policy, "allow_folders = {f}");
        }
        std::fs::write(root.join(".quire/shared/agent-policy.toml"), &policy).unwrap();
        let srv = server(&root, Some("probe"));
        let allowed: Vec<bool> = notes.iter().map(|n| expected(n, tags.as_deref(), folders.as_deref())).collect();
        // Each call, and the note it names (whose path it may echo back in "not found").
        let mut calls = vec![
            call("vault_info", &json!({})),
            call("search", &json!({"query": "shared words"})),
            call("search", &json!({"query": "task"})),
            call("list", &json!({})),
            call("list", &json!({"folder": "Work"})),
            call("tags", &json!({})),
            call("tasks", &json!({})),
            call("ask", &json!({"question": "what are the shared words?"})),
            call("search_notes", &json!({"query": "shared"})),
            call("list_notes", &json!({})),
            call("list_tags", &json!({})),
            call("ask_notes", &json!({"question": "shared words"})),
            call("query_view", &json!({"spec": "group: owner"})),
            call("query_view", &json!({"spec": "from: #work\ngroup: owner\nlayout: board"})),
            ("resources/list".into(), json!({})),
        ];
        let mut named: Vec<Vec<usize>> = vec![Vec::new(); calls.len()];
        for (i, note) in notes.iter().enumerate() {
            // 0.12: a typed link to another note (that may be hidden) names both.
            let j = rng.below(n);
            calls.push(call("add_link", &json!({"path": note.path, "type": "related", "target": format!("Note{j}")})));
            named.push(vec![i, j]);
            for c in [
                call("search", &json!({"query": note.marker})),
                call("view", &json!({"path": note.path})),
                call("outline", &json!({"path": note.path})),
                call("neighbors", &json!({"path": note.path, "depth": 2})),
                call("backlinks", &json!({"name": format!("Note{i}")})),
                call("links", &json!({"path": note.path})),
                call("links", &json!({"path": note.path, "type": ["up", "supports", "untyped"], "direction": "in"})),
                call("hierarchy", &json!({"path": note.path, "depth": 10})),
                call("neighbors", &json!({"path": note.path, "type": ["parent", "up"]})),
                call("read_note", &json!({"name": format!("Note{i}")})),
                ("resources/read".into(), json!({"uri": format!("quire://note/{}", note.path.replace(' ', "%20"))})),
            ] {
                calls.push(c);
                named.push(vec![i]);
            }
        }
        let mut replies = run(&srv, &calls);
        replies.sort_by_key(|v| v["id"].as_u64().unwrap_or(u64::MAX));
        assert_eq!(replies.len(), calls.len());
        let all: String = replies.iter().map(Value::to_string).collect();
        for (n, (note, ok)) in notes.iter().zip(&allowed).enumerate() {
            if *ok {
                assert!(all.contains(&note.marker), "seed {seed}: an allowed note ({}) should be readable", note.path);
                continue;
            }
            assert!(!all.contains(&note.marker), "seed {seed}: the text of {} leaked (policy {policy})", note.path);
            for (r, who) in replies.iter().zip(&named) {
                // A call that named this note may say "no note at <its path>"; any other mention is a leak.
                if !who.contains(&n) {
                    let text = r.to_string();
                    assert!(!text.contains(&note.path), "seed {seed}: the path of {} leaked in {text} (policy {policy})", note.path);
                }
            }
            leaks_checked += 1;
        }
    }
    assert!(leaks_checked > 40, "enough excluded notes were probed ({leaks_checked})");
}

#[test]
fn locked_folders_are_invisible_to_agents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join("Journal")).unwrap();
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::write(root.join("Journal/Day.md"), "# Day\nlockedmarker shared\n").unwrap();
    std::fs::write(root.join("Journal/pic.png"), "lockedpicture").unwrap();
    std::fs::write(root.join("Open.md"), "# Open\nshared [[Day]]\nup:: [[Day]]\n").unwrap();
    std::fs::write(root.join(".quire/shared/agent-policy.toml"), "format = 1\ndefault = \"direct\"\n").unwrap();
    let v = Vault::open_local(&root).unwrap();
    lock_folder(&v, "Journal", Some("pass phrase"), Kdf::TEST, 1).unwrap();
    // Even unlocked in some process: agents get nothing.
    let srv = server(&root, Some("probe"));
    let replies = run(
        &srv,
        &[
            call("list", &json!({})),
            call("search", &json!({"query": "shared"})),
            call("view", &json!({"path": "Journal/Day.md"})),
            call("view", &json!({"path": "Journal/Day.md.qenc"})),
            call("view", &json!({"path": "Journal/quire-lock.json"})),
            call("neighbors", &json!({"path": "Open.md"})),
            ("resources/read".into(), json!({"uri": "quire://note/Journal/Day.md"})),
            ("resources/read".into(), json!({"uri": "quire://file/Journal/pic.png.qenc"})),
            ("resources/list".into(), json!({})),
            call("create_note", &json!({"title": "Sneak", "folder": "Journal", "content": "x"})),
            call("write", &json!({"path": "Journal/New.md", "text": "plain"})),
            // 0.12: typed links and the hierarchy don't reach into it either.
            call("links", &json!({"path": "Open.md"})),
            call("hierarchy", &json!({"path": "Open.md"})),
        ],
    );
    let all: String = replies.iter().map(Value::to_string).collect();
    assert!(!all.contains("lockedmarker") && !all.contains("lockedpicture"), "{all}");
    // Lists, search and links never mention the folder (calls that named a path may echo it in "not found").
    for r in replies.iter().filter(|r| [1, 2, 6, 9, 12, 13].contains(&r["id"].as_u64().unwrap_or(0))) {
        let t = r.to_string();
        assert!(!t.contains("Journal") && !t.contains(".qenc") && !t.contains("quire-lock"), "{t}");
    }
    for r in replies.iter().filter(|r| [3, 4, 5, 7, 8].contains(&r["id"].as_u64().unwrap_or(0))) {
        let t = r.to_string();
        assert!(t.contains("not_found") || t.contains("not found") || t.contains("Not found"), "{t}");
    }
    assert!(all.contains("Open.md"));
    assert!(!root.join("Journal/New.md").exists() && !root.join("Journal/Sneak.md").exists(), "nothing plain written into it");
}

#[test]
fn a_revoked_client_is_refused_by_key_and_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::write(root.join("Plan.md"), "# Plan\n").unwrap();
    std::fs::write(root.join(".quire/shared/agent-policy.toml"), "format = 1\n[clients.\"mcp:probe\"]\nrevoked = true\nlog_reads = true\n")
        .unwrap();
    let srv = server(&root, Some("probe"));
    assert!(srv.refusal().unwrap().contains("revoked"), "the CLI exits with this at start");
    let r = run(&srv, &[call("view", &json!({"path": "Plan.md"}))]);
    assert!(r[0]["error"]["message"].as_str().unwrap().contains("revoked"), "{r:?}");
    // Without --client it reports the name "probe": refused the same way.
    let unnamed = server(&root, None);
    let r = run(&unnamed, &[call("view", &json!({"path": "Plan.md"}))]);
    assert!(r[0]["error"]["message"].as_str().unwrap().contains("revoked"), "{r:?}");
    // Another client is fine.
    let other = server(&root, Some("other"));
    assert!(other.refusal().is_none());
    let r = run(&other, &[call("view", &json!({"path": "Plan.md"}))]);
    assert!(r[0].get("error").is_none(), "{r:?}");
}

#[test]
fn ai_off_refuses_every_client() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::write(root.join("Plan.md"), "# Plan\n").unwrap();
    std::fs::write(root.join(".quire/shared/agent-policy.toml"), "format = 1\nai = \"off\"\n").unwrap();
    let srv = server(&root, Some("probe"));
    assert!(srv.refusal().unwrap().contains("AI is off"));
    let r = run(&srv, &[call("search", &json!({"query": "plan"})), ("resources/list".into(), json!({}))]);
    assert!(r.iter().all(|x| x["error"]["message"].as_str().is_some_and(|m| m.contains("AI is off"))), "{r:?}");
}

/// Security review S-M2: a view's groups and total count only the notes the
/// client may see.
#[test]
fn query_view_groups_and_totals_count_only_visible_notes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::create_dir_all(root.join("Public")).unwrap();
    std::fs::create_dir_all(root.join("Private")).unwrap();
    std::fs::write(root.join("Public/A.md"), "---\nclient: Open Co\n---\n# A\n").unwrap();
    std::fs::write(root.join("Private/B.md"), "---\nclient: zqxSecretClient\n---\n# B\n").unwrap();
    std::fs::write(
        root.join(".quire/shared/agent-policy.toml"),
        "format = 1\ndefault = \"read\"\n[clients.\"mcp:probe\"]\nallow_folders = [\"Public/\"]\n",
    )
    .unwrap();
    let srv = server(&root, Some("probe"));
    let replies = run(
        &srv,
        &[call("query_view", &json!({"spec": "group: client"})), call("query_view", &json!({"spec": "lanes: client\ngroup: client"}))],
    );
    let all: String = replies.iter().map(Value::to_string).collect();
    assert!(!all.contains("zqxSecretClient"), "{all}");
    assert!(all.contains("Open Co"));
    let r = replies.iter().find(|r| r["id"] == 1).unwrap();
    assert_eq!(r["result"]["structuredContent"]["total"], 1, "{r}");
}

/// 0.12 typed links: `links`, `hierarchy` and `add_link` see only what the
/// client may, never walk through a hidden note, and write through review.
#[test]
fn typed_links_and_hierarchy_for_a_limited_client() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    let files = [
        ("Maths.md", "# Maths #ok\n"),
        ("wiki/Calculus.md", "---\nup: \"[[Maths]]\"\ntags: [ok]\n---\n# Calculus\n"),
        ("Limits.md", "# Limits #ok\nup:: [[Calculus]]\n[supports:: [[Claim]]]\nnext:: [[Series]]\n"),
        ("Series.md", "# Series #ok\nup:: [[Hidden]]\n"),
        ("Hidden.md", "# Hidden zqxhidden\nup:: [[Calculus]]\n[zqxfield:: [[Limits]]]\n"),
        ("Claim.md", "# Claim #ok\n"),
    ];
    for (p, t) in files {
        std::fs::write(root.join(p), t).unwrap();
    }
    std::fs::write(
        root.join(".quire/shared/agent-policy.toml"),
        "format = 1\ndefault = \"propose\"\n[[zone]]\npath = \"wiki/\"\nrole = \"agent\"\nmode = \"direct\"\n[clients.\"mcp:probe\"]\nallow_tags = [\"ok\"]\n",
    )
    .unwrap();
    let srv = server(&root, Some("probe"));
    let replies = run(
        &srv,
        &[
            call("links", &json!({"path": "Limits.md"})),
            call("links", &json!({"path": "Claim.md", "direction": "in", "type": "supports"})),
            call("hierarchy", &json!({"path": "wiki/Calculus.md", "depth": 5})),
            call("hierarchy", &json!({"path": "Series.md", "depth": 5})),
            call("neighbors", &json!({"path": "Limits.md", "type": ["up"]})),
            call("add_link", &json!({"path": "wiki/Calculus.md", "type": "related", "target": "Claim"})),
            call("add_link", &json!({"path": "Limits.md", "type": "source", "target": "Maths"})),
            call("add_link", &json!({"path": "Limits.md", "type": "source", "target": "Hidden"})),
        ],
    );
    let by_id = |id: u64| replies.iter().find(|r| r["id"] == id).unwrap()["result"].clone();
    // The call that names Hidden may say "no note at Hidden.md"; no other reply mentions it.
    let all: String = replies.iter().filter(|r| r["id"] != 8).map(Value::to_string).collect();
    assert!(!all.contains("zqx") && !all.contains("Hidden"), "a hidden note never shows: {all}");

    let links = &by_id(1)["structuredContent"]["links"];
    let has =
        |dir: &str, t: &str, p: &str| links.as_array().unwrap().iter().any(|l| l["direction"] == dir && l["type"] == t && l["path"] == p);
    assert!(has("out", "up", "wiki/Calculus.md") && has("out", "supports", "Claim.md") && has("out", "next", "Series.md"), "{links}");
    let into_claim = by_id(2)["structuredContent"]["links"].clone();
    assert_eq!(
        into_claim,
        json!([{"direction": "in", "type": "supports", "hierarchy": null, "implied": false, "path": "Limits.md", "title": "Limits #ok"}])
    );

    let calc = &by_id(3)["structuredContent"];
    assert_eq!(calc["ancestors"][0]["path"], "Maths.md");
    let kids: Vec<&str> = calc["descendants"].as_array().unwrap().iter().filter_map(|d| d["path"].as_str()).collect();
    assert_eq!(kids, ["Limits.md"], "Hidden is skipped, and Series (below it) isn't reached through it");
    let series = &by_id(4)["structuredContent"];
    assert_eq!(series["ancestors"], json!([]));
    assert_eq!(series["trails"], json!([]), "a trail through a hidden note is dropped");
    assert_eq!(series["previous"], "Limits.md");
    let n = &by_id(5)["structuredContent"]["edges"];
    assert!(n.as_array().unwrap().iter().all(|e| e["type"] == "up"), "{n}");

    // In the agent's zone the link is written; elsewhere it is a suggestion; a hidden target is "not found".
    assert_eq!(by_id(6)["structuredContent"]["outcome"], "applied", "{}", by_id(6));
    assert!(std::fs::read_to_string(root.join("wiki/Calculus.md")).unwrap().contains("related: \"[[Claim]]\"\n"));
    assert_eq!(by_id(7)["structuredContent"]["outcome"], "proposed", "{}", by_id(7));
    assert_eq!(std::fs::read_to_string(root.join("Limits.md")).unwrap(), files[2].1, "a suggestion changes nothing yet");
    let hidden = replies.iter().find(|r| r["id"] == 8).unwrap().to_string();
    assert!(hidden.contains("not_found"), "{hidden}");
}

/// Two visible notes that share only a hidden parent are not reported as
/// siblings, and `next`/`previous` don't step through that parent either.
#[test]
fn siblings_under_a_hidden_parent_are_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    let files = [
        ("Hidden.md", "# Hidden zqxhidden\n"),
        ("Apple.md", "# Apple #ok\nup:: [[Hidden]]\n"),
        ("Berry.md", "# Berry #ok\nup:: [[Hidden]]\n"),
        ("Shown.md", "# Shown #ok\n"),
        ("Cherry.md", "# Cherry #ok\nup:: [[Shown]]\n"),
        ("Date.md", "# Date #ok\nup:: [[Shown]]\nsame:: [[Apple]]\n"),
    ];
    for (p, t) in files {
        std::fs::write(root.join(p), t).unwrap();
    }
    std::fs::write(root.join(".quire/shared/agent-policy.toml"), "format = 1\n[clients.\"mcp:probe\"]\nallow_tags = [\"ok\"]\n").unwrap();
    let srv = server(&root, Some("probe"));
    let replies = run(&srv, &[call("hierarchy", &json!({"path": "Apple.md"})), call("hierarchy", &json!({"path": "Cherry.md"}))]);
    let by_id = |id: u64| replies.iter().find(|r| r["id"] == id).unwrap()["result"]["structuredContent"].clone();
    let apple = by_id(1);
    // Only the explicit `same` link (between two visible notes) is a sibling.
    let sibs: Vec<&str> = apple["siblings"].as_array().unwrap().iter().filter_map(|d| d["path"].as_str()).collect();
    assert_eq!(sibs, ["Date.md"], "{apple}");
    assert!(apple["next"].is_null() && apple["previous"].is_null(), "{apple}");
    // Under a visible parent, siblings are as before.
    let cherry = by_id(2);
    let sibs: Vec<&str> = cherry["siblings"].as_array().unwrap().iter().filter_map(|d| d["path"].as_str()).collect();
    assert_eq!(sibs, ["Date.md"], "{cherry}");
    assert_eq!(cherry["next"], "Date.md");
    assert!(!replies.iter().map(Value::to_string).collect::<String>().contains("zqx"));
}
