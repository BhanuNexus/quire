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
//! Not in the table yet, because they need a note with history or a write grant: `diff`
//! (`from`/`to` and `from_version`/`to_version`), `edit_section` (`heading` and `section`),
//! `append` (`under_heading` and `section`), `proposals` (`mine` and `actor: "me"`) and `revert`
//! (`actor`). The step that adds those names adds their rows.
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
const NOT_YET: &[(&str, &str)] = &[
    ("ask k->limit (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("ask k->limit (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("list tag->tags (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("list tag->tags (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("query_view max_rows->limit (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("query_view max_rows->limit (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.folder->folder (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.folder->folder (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.modified_after->since (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.modified_after->since (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.modified_before->until (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.modified_before->until (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.tags->tags (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.tags->tags (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.where->where (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("search filters.where->where (same value)", "step 4: aliases and the one convention (§3.2)"),
    ("tasks tag->tags (canonical wins)", "step 4: aliases and the one convention (§3.2)"),
    ("tasks tag->tags (same value)", "step 4: aliases and the one convention (§3.2)"),
];

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
