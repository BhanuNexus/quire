// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The shape of the tool registry that `docs/design/0.14-agent-tools.md` §2 and §3.3 fix, written
//! before the steps that reach it (the first build step).
//!
//! Each test states the target, computes what the registry does today and compares it with a list
//! of known gaps. A gap the registry still has must be in the list; a gap that is in the list but
//! gone fails the test too, so the step that removes it also deletes its row. A new tool, or a
//! change that opens a new gap, fails at once. The tests that pin a number the later steps change
//! (the tool counts, the size of `tools/list`) pin today's number and keep the target in an
//! `#[ignore]`d test that names the step which turns it on.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::collections::BTreeSet;

use quire_app::vault::Vault;
use quire_mcp::tools::{Access, Grant};
use quire_mcp::{Server, ServerConfig, ToolRegistry};
use serde_json::{Value, json};

/// The 0.14 contract of every tool that returns a list or a long text (the table of 3.3): its `limit`
/// (default, maximum), whether it pages (`cursor` in, `next_cursor` out) and whether it reports a
/// `total`. `None` for `limit`: no row limit (a text read in lines, or one answer).
struct Row {
    tool: &'static str,
    limit: Option<(u64, u64)>,
    cursor: bool,
    total: bool,
    /// Takes `max_tokens` (all but `vault_info`, whose `zones` are capped at 50 instead).
    tokens: bool,
}

const fn row(tool: &'static str, limit: Option<(u64, u64)>, cursor: bool, total: bool) -> Row {
    Row { tool, limit, cursor, total, tokens: true }
}

/// The table of `docs/design/0.14-agent-tools.md` §3.3. Every tool here says `truncated`.
const TABLE: &[Row] = &[
    row("search", Some((10, 50)), true, false),
    row("ask", Some((6, 20)), false, false),
    row("list", Some((100, 500)), true, true),
    row("tasks", Some((200, 500)), true, true),
    row("query_view", Some((100, 500)), true, true),
    row("recent_activity", Some((100, 500)), true, true),
    row("history", Some((20, 200)), true, true),
    row("proposals", Some((50, 200)), true, true),
    row("tags", Some((200, 1000)), true, true),
    row("properties", Some((100, 500)), true, true),
    row("outline", Some((500, 2000)), true, true),
    row("neighbors", Some((50, 200)), false, true),
    row("links", Some((100, 500)), true, true),
    row("hierarchy", Some((200, 500)), false, true),
    row("diff", None, true, false),
    row("view", None, true, false),
    row("read_version", None, true, false),
    row("canvas_read", None, false, false),
    // `zones` is capped at 50 and says `truncated`; there is no `limit` and no `max_tokens`.
    Row { tool: "vault_info", limit: None, cursor: false, total: false, tokens: false },
];

/// Today's gaps against [`TABLE`], each with the step that closes it (`docs/design/0.14-agent-tools.md` §8).
const BOUNDS_GAPS: &[(&str, &str)] = &[
    ("ask: no `limit`", "step 4: `limit` is the one row cap, `k` and `max_rows` stay as aliases"),
    ("query_view: no `limit`", "step 4: `limit` is the one row cap, `k` and `max_rows` stay as aliases"),
];

fn props(schema: &Value) -> &serde_json::Map<String, Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();
    schema["properties"].as_object().unwrap_or_else(|| EMPTY.get_or_init(serde_json::Map::new))
}

/// Every gap of the registry against [`TABLE`], as `tool: what`.
fn bounds_gaps() -> BTreeSet<String> {
    let registry = ToolRegistry::standard();
    let mut gaps = BTreeSet::new();
    for r in TABLE {
        let spec = registry.get(r.tool).unwrap_or_else(|| panic!("{} is in the registry", r.tool)).spec();
        let input = props(&spec.input_schema);
        let out = spec.output_schema.as_ref().unwrap_or_else(|| panic!("{} has an output schema", r.tool));
        let output = props(out);
        let mut gap = |what: &str| {
            gaps.insert(format!("{}: {what}", r.tool));
        };
        if let Some((default, max)) = r.limit {
            match input.get("limit") {
                None => gap("no `limit`"),
                Some(l) => {
                    if l["default"].as_u64() != Some(default) {
                        gap(&format!("`limit` default is {}, not {default}", l["default"]));
                    }
                    if l["maximum"].as_u64() != Some(max) {
                        gap(&format!("`limit` maximum is {}, not {max}", l["maximum"]));
                    }
                    if l["minimum"].as_u64().is_none() {
                        gap("`limit` has no minimum");
                    }
                }
            }
        }
        if r.tokens && !input.contains_key("max_tokens") {
            gap("no `max_tokens`");
        }
        if !output.contains_key("truncated") {
            gap("no `truncated` in the output");
        }
        if r.cursor {
            if !input.contains_key("cursor") {
                gap("no `cursor`");
            }
            if !output.contains_key("next_cursor") {
                gap("no `next_cursor` in the output");
            }
        }
        if r.total && !output.contains_key("total") {
            gap("no `total` in the output");
        }
    }
    gaps
}

/// The risk "A tool is unbounded again" (`docs/design/0.14-agent-tools.md` §6): every list tool is in [`TABLE`], takes `limit` with a minimum,
/// a maximum and a default, takes `max_tokens`, and says `truncated`; a paged tool takes `cursor`
/// and returns `next_cursor`.
#[test]
fn every_list_tool_is_bounded_or_a_known_gap() {
    let registry = ToolRegistry::standard();
    // A tool that returns an array at the top level, and one with a cursor, must be in the table:
    // a new one is checked by adding its row.
    for t in registry.all() {
        let spec = t.spec();
        let Some(out) = &spec.output_schema else { continue };
        // Write tools answer with an outcome, not a list.
        if !matches!(spec.access, Access::Read | Access::Ask) {
            continue;
        }
        // A list (an array at the top), or anything bounded or paged like one: a long read too.
        let lists = props(out).values().any(|p| p["type"] == "array");
        let input = props(&spec.input_schema);
        let bounded = ["cursor", "limit", "max_tokens", "max_rows", "k"].iter().any(|k| input.contains_key(*k))
            || ["next_cursor", "truncated", "total"].iter().any(|k| props(out).contains_key(*k));
        if lists || bounded {
            assert!(TABLE.iter().any(|r| r.tool == spec.name), "{} returns a list or pages: add its row to TABLE (§3.3)", spec.name);
        }
    }
    let now = bounds_gaps();
    let known: BTreeSet<String> = BOUNDS_GAPS.iter().map(|(g, _)| (*g).to_string()).collect();
    let fresh: Vec<_> = now.difference(&known).collect();
    let closed: Vec<_> = known.difference(&now).collect();
    assert!(fresh.is_empty(), "new gaps against the §3.3 table:\n{}", fresh.iter().map(|g| g.as_str()).collect::<Vec<_>>().join("\n"));
    assert!(
        closed.is_empty(),
        "these gaps are closed: delete them from BOUNDS_GAPS:\n{}",
        closed.iter().map(|g| g.as_str()).collect::<Vec<_>>().join("\n")
    );
    if std::env::var_os("QUIRE_SHOW_GAPS").is_some() {
        for g in &now {
            eprintln!("    (\"{g}\", \"step 3\"),");
        }
    }
}

// ------------------------------------------------------------------ counts and size

const ERAS: &[&str] = &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The tools `tools/list` offers a stdio client of `era`, with or without a write grant.
fn listed(era: &str, write: bool) -> Vec<Value> {
    let srv =
        Server::with_config(Vault::in_memory(&[("A.md", "# A\n")]), None, ServerConfig { allow_write: write, ..ServerConfig::default() });
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    let lines: Vec<Value> = if era == "2026-07-28" {
        let meta = json!({"io.modelcontextprotocol/protocolVersion": era, "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
        vec![json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta}})]
    } else {
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": era, "capabilities": {}, "clientInfo": {"name": "probe", "version": "1"}}});
        vec![init, list]
    };
    let input = lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(input), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let reply: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    reply["result"]["tools"].as_array().unwrap_or_else(|| panic!("{era}: {reply}")).clone()
}

/// The tools of the 0.8 set, which step 5 removes.
const LEGACY_NINE: usize = 9;

/// G1 today: 42 tools with a write grant, 25 without, in every era. The target (33 and 19) is in
/// [`target_tool_counts`].
#[test]
fn tool_counts_today() {
    for era in ERAS {
        assert_eq!(listed(era, true).len(), 42, "{era}, write grant");
        assert_eq!(listed(era, false).len(), 25, "{era}, read-only");
    }
    let registry = ToolRegistry::standard();
    assert_eq!(registry.offered_for(Grant::stdio(true)).count(), 42);
    assert_eq!(registry.offered_for(Grant::READ).count(), 25);
}

/// G1: 33 tools to a client with a write grant and 19 to a read-only one, in every era.
#[test]
#[ignore = "removed by step 5: the nine 0.8 tools are removed from the registry"]
fn target_tool_counts() {
    for era in ERAS {
        assert_eq!(listed(era, true).len(), 42 - LEGACY_NINE, "{era}, write grant");
        assert_eq!(listed(era, false).len(), 25 - 6, "{era}, read-only");
    }
}

/// Names that clients accept everywhere: `^[a-z][a-z0-9_]{0,63}$` (G2).
#[test]
fn tool_names_match_the_portable_pattern() {
    for t in ToolRegistry::standard().all() {
        let name = t.spec().name;
        let mut chars = name.chars();
        let ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && name.len() <= 64
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        assert!(ok, "{name} does not match ^[a-z][a-z0-9_]{{0,63}}$");
    }
}

/// The bytes of the `tools` array a write-grant client gets in the newest era.
fn list_bytes() -> usize {
    Value::Array(listed("2026-07-28", true)).to_string().len()
}

/// G2: `tools/list` for a write-grant client, measured when the bounds and cursors of §3.3 landed
/// (step 3): 43,327 bytes, of which the nine 0.8 tools take 3,054 (before: 41,315 by the design,
/// 41,264 measured on main at the start of 0.14). It may not grow by more than 5%.
const BYTES_MEASURED: usize = 43_327;
/// The nine 0.8 tools' entries in that list, with their commas.
const BYTES_LEGACY_NINE: usize = 3_054;

#[test]
fn tools_list_does_not_grow() {
    let now = list_bytes();
    assert!(now * 100 <= BYTES_MEASURED * 105, "tools/list is {now} bytes, over the {BYTES_MEASURED} measured at step 3 plus 5%");
}

/// G2 target: once step 5 removes the nine 0.8 tools, the list measured at step 3 without them,
/// plus 5%.
#[test]
#[ignore = "removed by step 5: the nine 0.8 tools leave tools/list"]
fn target_tools_list_size() {
    let budget = (BYTES_MEASURED - BYTES_LEGACY_NINE) * 105 / 100;
    let now = list_bytes();
    assert!(now <= budget, "tools/list is {now} bytes, over {budget}");
}

// ------------------------------------------------------------------ paging

/// A tools/call on a stdio connection in the newest era.
fn call(srv: &Server, name: &str, args: &Value) -> Value {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}});
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
    let reply: Value = serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
    assert!(reply["result"]["isError"] != true, "{name} {args}: {reply}");
    reply["result"]["structuredContent"].clone()
}

/// A note with its own tag, property and task, so each list has a row per note.
fn row_note(i: usize) -> String {
    format!("---\ntags: [t{i:03}]\np{i:03}: x\n---\n# Note {i}\n\n- [ ] task {i}\n")
}

/// Every row of a list, following `next_cursor`; `between` runs after the first page.
fn all_pages(srv: &Server, tool: &str, array: &str, id: &dyn Fn(&Value) -> String, between: &dyn Fn()) -> (Vec<String>, usize) {
    let (mut ids, mut pages, mut cursor) = (Vec::new(), 0, None::<String>);
    loop {
        let args = cursor.as_ref().map_or_else(|| json!({}), |c| json!({ "cursor": c }));
        let r = call(srv, tool, &args);
        pages += 1;
        ids.extend(r[array].as_array().unwrap().iter().map(id));
        if pages == 1 {
            assert_eq!(r["truncated"], true, "{tool}: 600 rows do not fit one page: {}", r["total"]);
            assert!(r["total"].as_u64().unwrap() >= 600, "{tool}: total {}", r["total"]);
            between();
        }
        let Some(c) = r["next_cursor"].as_str() else {
            assert_eq!(r["truncated"], false, "{tool}: the last page says it is the last");
            return (ids, pages);
        };
        cursor = Some(c.to_string());
    }
}

/// A list tool, its array, a row's id, and the id note `i` gives.
type ListCase<'a> = (&'a str, &'a str, &'a dyn Fn(&Value) -> String, &'a dyn Fn(usize) -> String);

/// The risk "A tool is unbounded again" (`docs/design/0.14-agent-tools.md` §6): 600 rows, a call
/// with no arguments, then `next_cursor` to the end: no row repeats or is missing. Again with a
/// note added before the cursor and one removed after it between pages (keyset: still no repeat
/// and no gap, and the removed one is gone).
#[test]
fn lists_page_to_the_end_without_repeats_or_gaps() {
    let notes: Vec<(String, String)> = (0..600).map(|i| (format!("N/Note {i:03}.md"), row_note(i))).collect();
    let files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let lists: [ListCase<'_>; 4] = [
        ("list", "notes", &|r| r["path"].as_str().unwrap().to_string(), &|i| format!("N/Note {i:03}.md")),
        ("tasks", "tasks", &|r| r["path"].as_str().unwrap().to_string(), &|i| format!("N/Note {i:03}.md")),
        ("tags", "tags", &|r| r["tag"].as_str().unwrap().to_string(), &|i| format!("t{i:03}")),
        ("properties", "keys", &|r| r["key"].as_str().unwrap().to_string(), &|i| format!("p{i:03}")),
    ];
    for (tool, array, id, expect) in lists {
        let srv = Server::with_config(Vault::in_memory(&files), None, ServerConfig::default());
        srv.index().flush();
        let (ids, pages) = all_pages(&srv, tool, array, id, &|| {});
        assert!(pages > 1, "{tool}");
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{tool}: a row repeats");
        for i in 0..600 {
            assert!(unique.contains(&expect(i)), "{tool}: {} is missing", expect(i));
        }

        // Between the first and second page: a note that sorts first is added, the last one removed.
        let srv = Server::with_config(Vault::in_memory(&files), None, ServerConfig::default());
        srv.index().flush();
        let change = || {
            srv.index().upsert("A/First.md".into(), "---\ntags: [a000]\na000: x\n---\n# First\n\n- [ ] first\n".into());
            srv.index().remove("N/Note 599.md".into());
            srv.index().flush();
        };
        let (ids, _) = all_pages(&srv, tool, array, id, &change);
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{tool}: a row repeats after a change between pages");
        for i in 0..599 {
            assert!(unique.contains(&expect(i)), "{tool}: {} is missing after a change between pages", expect(i));
        }
        assert!(!unique.contains(&expect(599)), "{tool}: a row removed before its page is not listed");
    }
}
