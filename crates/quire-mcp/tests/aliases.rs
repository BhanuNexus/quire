// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! One parameter name per idea, every old spelling still works (goal G6 and the convention table
//! of `docs/design/0.14-agent-tools.md` §3.2; the risk "An alias disagrees with its canonical
//! name"), written before the step that adds the canonical names (the first build step; the
//! aliases are the fourth).
//!
//! The table holds each (old, new) pair of the read tools with two values that give different
//! results. Three checks per row: the old spelling gives different results for the two values (the
//! fixture can tell them apart), the canonical spelling gives the same whole result as the old
//! one, and with both given and different, the canonical one wins. A row that fails today is in
//! [`NOT_YET`] with the step that fixes it; the test fails if that list is wrong in either
//! direction.
//!
//! The names that need a note with history or a write grant are checked by
//! [`write_and_history_aliases_agree`]: `diff` (`from`/`to` and `from_version`/`to_version`),
//! `edit_section` (`heading` and `section`), `append` (`under_heading` and `section`) and
//! `proposals` (`mine` and `actor`). `revert` takes `actor` as before (no old spelling).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::collections::BTreeSet;

use quire_app::vault::Vault;
use quire_mcp::Server;
use serde_json::{Value, json};

const NOTES: &[(&str, &str)] = &[
    ("Ideas.md", "---\ntags: [shared]\nstatus: open\n---\n# Ideas\n\nshared words, see [[Plan]].\n\n- [ ] an idea task #shared\n"),
    ("Plan.md", "---\ntags: [shared, plan]\nstatus: draft\n---\n# Plan\n\nshared plan for [[Ideas]].\n\n- [ ] plan the work #plan\n"),
    ("wiki/Page.md", "---\ntags: [wiki]\nstatus: draft\n---\n# Page\n\nshared words in the wiki.\n\n- [ ] page task #wiki\n"),
    ("Archive/Old.md", "---\ntags: [old]\nstatus: done\n---\n# Old\n\nshared words, long ago.\n"),
];

/// One pair of spellings. `base` is the rest of the arguments; `canon` and `old` are a dotted path
/// into the arguments and two values each (`filters.folder` is `{"filters": {"folder": …}}`).
struct Pair {
    tool: &'static str,
    base: Value,
    canon: (&'static str, [Value; 2]),
    old: (&'static str, [Value; 2]),
}

fn pairs() -> Vec<Pair> {
    let q = json!({"query": "shared"});
    vec![
        Pair {
            tool: "search",
            base: q.clone(),
            canon: ("folder", [json!("wiki/"), json!("Archive/")]),
            old: ("filters.folder", [json!("wiki/"), json!("Archive/")]),
        },
        Pair {
            tool: "search",
            base: q.clone(),
            canon: ("tags", [json!(["plan"]), json!(["wiki"])]),
            old: ("filters.tags", [json!(["plan"]), json!(["wiki"])]),
        },
        Pair {
            tool: "search",
            base: q.clone(),
            canon: ("where", [json!("status = draft"), json!("status = done")]),
            old: ("filters.where", [json!("status = draft"), json!("status = done")]),
        },
        Pair {
            tool: "search",
            base: q.clone(),
            canon: ("since", [json!("2999-01-01"), json!("2000-01-01")]),
            old: ("filters.modified_after", [json!("2999-01-01"), json!("2000-01-01")]),
        },
        Pair {
            tool: "search",
            base: q,
            canon: ("until", [json!("2000-01-01"), json!("2999-01-01")]),
            old: ("filters.modified_before", [json!("2000-01-01"), json!("2999-01-01")]),
        },
        Pair {
            tool: "list",
            base: json!({}),
            canon: ("tags", [json!(["plan"]), json!(["wiki"])]),
            old: ("tag", [json!("plan"), json!("wiki")]),
        },
        Pair {
            tool: "tasks",
            base: json!({}),
            canon: ("tags", [json!(["plan"]), json!(["wiki"])]),
            old: ("tag", [json!("plan"), json!("wiki")]),
        },
        Pair {
            tool: "ask",
            base: json!({"question": "shared words", "mode": "extract"}),
            canon: ("limit", [json!(1), json!(4)]),
            old: ("k", [json!(1), json!(4)]),
        },
        Pair {
            tool: "query_view",
            base: json!({"spec": "sort: status"}),
            canon: ("limit", [json!(1), json!(3)]),
            old: ("max_rows", [json!(1), json!(3)]),
        },
    ]
}

/// The rows that do not hold yet, as `tool old->canonical (check)`, and the step that makes them.
const NOT_YET: &[(&str, &str)] = &[];

fn with(base: &Value, path: &str, value: &Value) -> Value {
    let mut args = base.clone();
    let mut at = &mut args;
    let keys: Vec<&str> = path.split('.').collect();
    for key in &keys[..keys.len() - 1] {
        at = &mut at[*key];
        if !at.is_object() {
            *at = json!({});
        }
    }
    at[keys[keys.len() - 1]] = value.clone();
    args
}

/// Both spellings at once.
fn with_both(base: &Value, a: (&str, &Value), b: (&str, &Value)) -> Value {
    with(&with(base, a.0, a.1), b.0, b.1)
}

fn call(srv: &Server, name: &str, args: &Value) -> Value {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}});
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
    assert!(reply.get("error").is_none() && reply["result"]["isError"] != true, "{name} {args}: {reply}");
    reply["result"].clone()
}

#[test]
fn aliases_agree_with_their_canonical_names_or_are_known_gaps() {
    let srv = Server::with_vault(Vault::in_memory(NOTES), None, false);
    srv.index().flush();
    let mut failing = BTreeSet::new();
    for p in pairs() {
        let label = |check: &str| format!("{} {}->{} ({check})", p.tool, p.old.0, p.canon.0);
        let [old_a, old_b] =
            [call(&srv, p.tool, &with(&p.base, p.old.0, &p.old.1[0])), call(&srv, p.tool, &with(&p.base, p.old.0, &p.old.1[1]))];
        assert_ne!(old_a, old_b, "{}: the fixture must tell the two values apart", label("fixture"));
        // The same value through the canonical name gives the same whole result.
        let canon_a = call(&srv, p.tool, &with(&p.base, p.canon.0, &p.canon.1[0]));
        if canon_a != old_a {
            failing.insert(label("same value"));
        }
        // Both given and different: the canonical one wins.
        let both = call(&srv, p.tool, &with_both(&p.base, (p.canon.0, &p.canon.1[0]), (p.old.0, &p.old.1[1])));
        if both != canon_a || both == old_b {
            failing.insert(label("canonical wins"));
        }
    }
    let known: BTreeSet<String> = NOT_YET.iter().map(|(r, _)| (*r).to_string()).collect();
    let fresh: Vec<_> = failing.difference(&known).collect();
    let closed: Vec<_> = known.difference(&failing).collect();
    assert!(
        fresh.is_empty(),
        "these pairs disagree and are not known gaps:\n{}",
        fresh.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    );
    assert!(
        closed.is_empty(),
        "these pairs hold now: delete them from NOT_YET:\n{}",
        closed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    );
    if std::env::var_os("QUIRE_SHOW_GAPS").is_some() {
        for g in &failing {
            eprintln!("    (\"{g}\", \"step 4\"),");
        }
    }
}

/// A server that may write, over a fresh copy of [`NOTES`] (with a `wiki/` note it writes directly).
fn writer() -> Server {
    let mut notes: Vec<(&str, &str)> = NOTES.to_vec();
    notes.push(("wiki/Log.md", "# Log\n\n## Done\n\n- one\n\n## Next\n\n- two\n"));
    let srv = Server::with_vault(Vault::in_memory(&notes), None, true);
    srv.index().flush();
    srv
}

fn text_of(srv: &Server, path: &str) -> String {
    call(srv, "view", &json!({"path": path}))["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

/// The aliases of write tools, of `diff` and of `proposals`, each on fresh servers: the old and the
/// canonical spelling give the same note and the same result, and with both the canonical one wins.
#[test]
fn write_and_history_aliases_agree() {
    // edit_section: `heading` → `section`; append: `under_heading` → `section`.
    for (tool, base, canon, old) in [
        ("edit_section", json!({"path": "wiki/Log.md", "content": "- new", "mode": "append"}), "section", "heading"),
        ("append", json!({"path": "wiki/Log.md", "text": "- new"}), "section", "under_heading"),
    ] {
        let run = |args: &Value| {
            let srv = writer();
            let _ = call(&srv, tool, args);
            text_of(&srv, "wiki/Log.md")
        };
        let (old_done, old_next) = (run(&with(&base, old, &json!("Done"))), run(&with(&base, old, &json!("Next"))));
        assert_ne!(old_done, old_next, "{tool}: the fixture tells the sections apart");
        assert_eq!(run(&with(&base, canon, &json!("Done"))), old_done, "{tool} {old}->{canon}: same value");
        assert_eq!(run(&with_both(&base, (canon, &json!("Done")), (old, &json!("Next")))), old_done, "{tool}: {canon} wins");
    }

    // diff: `from`/`to` → `from_version`/`to_version`.
    let srv = writer();
    let v0 = call(&srv, "view", &json!({"path": "wiki/Log.md"}))["structuredContent"]["version"].clone();
    let _ = call(&srv, "append", &json!({"path": "wiki/Log.md", "text": "- three"}));
    let v1 = call(&srv, "view", &json!({"path": "wiki/Log.md"}))["structuredContent"]["version"].clone();
    let base = json!({"path": "wiki/Log.md"});
    let old = call(&srv, "diff", &with(&with(&base, "from", &v0), "to", &v1));
    assert_ne!(old["structuredContent"]["unified"], "", "the fixture has a change: {old}");
    assert_eq!(call(&srv, "diff", &with(&with(&base, "from_version", &v0), "to_version", &v1)), old, "diff: same value");
    let both = call(&srv, "diff", &with(&with(&with(&base, "from_version", &v0), "to_version", &v1), "from", &v1));
    assert_eq!(both, old, "diff: from_version wins");

    // proposals: `mine: false` → `actor: "all"`, `mine: true` → `actor: "me"`.
    let srv = writer();
    let _ = call(&srv, "append", &json!({"path": "Ideas.md", "text": "a suggestion"}));
    let mine = call(&srv, "proposals", &json!({"mine": true}));
    let all = call(&srv, "proposals", &json!({"mine": false, "status": "all"}));
    assert_eq!(call(&srv, "proposals", &json!({"actor": "me"})), mine, "proposals: actor me");
    assert_eq!(call(&srv, "proposals", &json!({"actor": "all", "status": "all"})), all, "proposals: actor all");
    let nobody = call(&srv, "proposals", &json!({"actor": "mcp:nobody", "mine": true}));
    assert_eq!(nobody["structuredContent"]["total"], 0, "proposals: actor wins over mine: {nobody}");
    assert_eq!(mine["structuredContent"]["total"], 1, "the fixture has one suggestion: {mine}");
}

/// `lines` as text (`"2-3"`) is the same range as `[2, 3]`, in the tool.
#[test]
fn lines_as_text_or_array() {
    let srv = Server::with_vault(Vault::in_memory(NOTES), None, false);
    srv.index().flush();
    let array = call(&srv, "view", &json!({"path": "Ideas.md", "lines": [2, 3]}));
    assert_eq!(call(&srv, "view", &json!({"path": "Ideas.md", "lines": "2-3"})), array);
    assert_eq!(array["structuredContent"]["lines"], json!([2, 3]));
}

/// `str_replace` with `all: true` replaces every occurrence and says how many (the successor of the
/// 0.8 `replace_in_note`); without it, two occurrences are `not_unique` as before.
#[test]
fn str_replace_all_replaces_every_occurrence() {
    let srv = writer();
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let raw = |args: Value| {
        let line =
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "str_replace", "arguments": args, "_meta": meta}});
        let mut out = Vec::new();
        srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
        serde_json::from_str::<Value>(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap()["result"].clone()
    };
    let one = raw(json!({"path": "wiki/Log.md", "old_str": "- ", "new_str": "* "}));
    assert_eq!(one["structuredContent"]["error"], "not_unique", "{one}");
    let all = raw(json!({"path": "wiki/Log.md", "old_str": "- ", "new_str": "* ", "all": true}));
    assert_eq!(all["structuredContent"]["outcome"], "applied", "{all}");
    assert_eq!(all["structuredContent"]["replacements"], 2, "{all}");
    let text = call(&srv, "view", &json!({"path": "wiki/Log.md"}))["content"][0]["text"].as_str().unwrap().to_string();
    assert!(text.contains("* one") && text.contains("* two") && !text.contains("- "), "{text}");
}
