// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A listing tool makes no file system call per note (goal G4 of the 0.14 agent-tools design, and
//! its risk "A listing reads the disk per note again": `docs/design/0.14-agent-tools.md` §2, §6),
//! written before the step that makes it true (the first build step).
//!
//! A `Storage` wrapper counts `exists`, `modified`, `read` (and `read_bytes`) and `absolute` (the
//! call that precedes a `canonicalize`) for a vault of 2,000 notes in 40 folders. A call may make
//! none per note, at most two per distinct folder (the locked-folder check: its key file and its
//! unlocking file) and at most eight more in all (policy file, ledger). Tools over that budget
//! are listed in [`OVER_BUDGET`] with the step that fixes them (none now); the test fails if the
//! list is wrong in either direction.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds a vault with std directly"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use quire_agent::FixedClock;
use quire_app::vault::{LocalFs, NotePath, Storage, Vault};
use quire_mcp::tools::Access;
use quire_mcp::{Server, ServerConfig, ToolRegistry};
use serde_json::{Value, json};

const NOTES: usize = 2_000;
const FOLDERS: usize = 40;
/// Folder checks allowed per distinct folder, and calls allowed besides them and the notes.
const PER_FOLDER: usize = 2;
const OTHER: usize = 8;

/// The note the tools that take one are given.
const NAMED: &str = "Folder 3/Note 3.md";

/// Arguments that make a tool work on this vault; any other tool is called with `{}`, or with
/// [`NAMED`] as its `path`.
fn overrides(tool: &str) -> Option<Value> {
    Some(match tool {
        "search" => json!({"query": "topic 3"}),
        "ask" => json!({"question": "What are the words about topic 3?"}),
        "query_view" => json!({"spec": "group: status"}),
        "recent_activity" => json!({"since": "3650d"}),
        "canvas_read" => json!({"path": "Board.canvas"}),
        _ => return None,
    })
}

/// Every read tool of the registry that returns a list (an array at the top of its output), with
/// [`overrides`], then the [`HANDSHAKES`]: a tool added to the registry is checked here without
/// editing this file (`docs/design/0.14-agent-tools.md` §6).
fn calls() -> Vec<(&'static str, Value)> {
    let registry = ToolRegistry::standard();
    let mut out = Vec::new();
    for t in registry.all() {
        let spec = t.spec();
        let lists =
            spec.output_schema.as_ref().and_then(|o| o["properties"].as_object()).is_some_and(|p| p.values().any(|v| v["type"] == "array"));
        if !matches!(spec.access, Access::Read | Access::Ask) || !lists {
            continue;
        }
        // A tool that takes a note gets [`NAMED`].
        let takes_note = spec.input_schema["required"].as_array().is_some_and(|r| r.iter().any(|k| k == "path"));
        let args = overrides(spec.name).unwrap_or_else(|| if takes_note { json!({ "path": NAMED }) } else { json!({}) });
        for key in spec.input_schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            assert!(args.get(key).is_some(), "{} needs `{key}`: add its arguments to overrides()", spec.name);
        }
        out.push((spec.name, args));
    }
    assert!(out.len() >= 14, "the registry's list tools: {:?}", out.iter().map(|(t, _)| *t).collect::<Vec<_>>());
    out.extend(HANDSHAKES.iter().map(|h| (*h, json!({}))));
    out
}

/// What a client sends before it calls a tool; the connection text counts the notes it may see
/// (`docs/design/0.14-agent-index.md` §1).
const HANDSHAKES: &[&str] = &["initialize", "server/discover"];

/// The tools that read the disk per note today, by name, and the step that stops it. Empty since
/// the listing tools moved to `Visibility`; a tool that reads per note again fails the test.
const OVER_BUDGET: &[(&str, &str)] = &[];

/// One storage call: which, and on what.
type Log = Mutex<Vec<(&'static str, String)>>;

/// A [`LocalFs`] that remembers what it was asked about.
struct Counting {
    inner: LocalFs,
    log: Log,
}

impl Counting {
    fn note(&self, what: &'static str, path: &NotePath) {
        self.log.lock().unwrap_or_else(PoisonError::into_inner).push((what, path.as_str().to_string()));
    }

    fn take(&self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Storage for Counting {
    fn list(&self) -> quire_app::vault::Result<Vec<NotePath>> {
        self.inner.list()
    }

    fn read_bytes(&self, path: &NotePath) -> quire_app::vault::Result<Vec<u8>> {
        self.note("read", path);
        self.inner.read_bytes(path)
    }

    fn read(&self, path: &NotePath) -> quire_app::vault::Result<String> {
        self.note("read", path);
        self.inner.read(path)
    }

    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> quire_app::vault::Result<()> {
        self.inner.write_bytes_atomic(path, bytes)
    }

    fn rename(&self, from: &NotePath, to: &NotePath) -> quire_app::vault::Result<()> {
        self.inner.rename(from, to)
    }

    fn trash(&self, path: &NotePath) -> quire_app::vault::Result<()> {
        self.inner.trash(path)
    }

    fn exists(&self, path: &NotePath) -> bool {
        self.note("exists", path);
        self.inner.exists(path)
    }

    fn modified(&self, path: &NotePath) -> quire_app::vault::Result<SystemTime> {
        self.note("modified", path);
        self.inner.modified(path)
    }

    fn absolute(&self, path: &NotePath) -> Option<PathBuf> {
        self.note("absolute", path);
        self.inner.absolute(path)
    }

    fn list_under(&self, folder: &str) -> quire_app::vault::Result<Vec<NotePath>> {
        self.inner.list_under(folder)
    }

    fn remove(&self, path: &NotePath) -> quire_app::vault::Result<()> {
        self.inner.remove(path)
    }

    fn append_line(&self, path: &NotePath, line: &str) -> quire_app::vault::Result<()> {
        self.inner.append_line(path, line)
    }

    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> quire_app::vault::Result<bool> {
        self.inner.write_bytes_if(path, still, bytes)
    }

    fn take_listing_notice(&self) -> Option<String> {
        self.inner.take_listing_notice()
    }

    fn set_locked_folders(&self, folders: &[String]) {
        self.inner.set_locked_folders(folders);
    }

    fn trash_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool) -> quire_app::vault::Result<bool> {
        self.inner.trash_if(path, still)
    }
}

/// What a call's storage traffic comes to: calls per note, the most calls on one folder's lock
/// files, and every other call.
///
/// The notes a call names or returns are its own, not notes of the listing: what it does for each
/// of them (`existing` for the named note, the second guard and the trust label of a `search`
/// hit, a version of the named note) is bounded by the page, not by the vault (the design's §3.5,
/// "bounded by the page or by the folder count"). Their calls, and up to [`PER_FOLDER`] lock-file
/// calls each on their folders beyond the folder's own two, count with the others, which may grow
/// by [`PER_ROW`] for each such note.
struct Cost {
    per_note: usize,
    per_folder: usize,
    other: usize,
    /// The notes the call named or returned.
    own: usize,
}

/// Calls allowed for each note a call names or returns: its guard (the vault root, the policy file
/// and its folder's two lock files) and its trust label (the vault root, the policy file).
const PER_ROW: usize = 6;

fn cost(log: &[(&'static str, String)], own: &BTreeSet<String>) -> Cost {
    let (mut per_note, mut other) = (0, 0);
    let mut folders: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, path) in log {
        let name = path.rsplit('/').next().unwrap_or(path);
        if path.is_empty() || path.starts_with(".quire/") || name == "AGENTS.md" || name == "CLAUDE.md" || own.contains(path) {
            other += 1;
        } else if name.starts_with("quire-lock") {
            *folders.entry(path.rsplit_once('/').map_or("", |(f, _)| f)).or_default() += 1;
        } else {
            per_note += 1;
        }
    }
    // A folder's own two checks stay its own; checks beyond them on the folder of a note the call
    // names or returns (that note's guard) count with the others.
    for (folder, n) in &mut folders {
        let mine =
            own.iter().filter_map(|p| p.rsplit_once('/')).filter(|(f, _)| *f == *folder || f.starts_with(&format!("{folder}/"))).count();
        let theirs = n.saturating_sub(PER_FOLDER).min(PER_FOLDER * mine);
        *n -= theirs;
        other += theirs;
    }
    Cost { per_note, per_folder: folders.values().copied().max().unwrap_or(0), other, own: own.len() }
}

/// Every string in a reply: the notes it names are among them.
fn strings(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            out.insert(s.clone());
        }
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| strings(x, out)),
        _ => {}
    }
}

fn note_text(i: usize) -> String {
    format!(
        "---\ntags: [t{}, shared]\nstatus: s{}\n---\n# Note {i}\n\nwords about topic {} and [[Note {}]].\n\n- [ ] task {i}\n",
        i % 7,
        i % 3,
        i % 11,
        (i + 1) % NOTES
    )
}

/// A tool call, or with a name in [`HANDSHAKES`] that handshake.
fn call(srv: &Server, name: &str, args: &Value) -> Value {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let line = match name {
        "initialize" => {
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "probe", "version": "1"}}})
        }
        "server/discover" => json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}),
        _ => json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}}),
    };
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(line.to_string()), &mut out).unwrap();
    serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap()
}

#[test]
fn listings_make_no_storage_call_per_note_or_are_known_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    for i in 0..NOTES {
        let path = root.join(format!("Folder {}/Note {i}.md", i % FOLDERS));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, note_text(i)).unwrap();
    }
    std::fs::write(
        root.join("Board.canvas"),
        r#"{"nodes":[{"id":"a","type":"text","text":"a card","x":0,"y":0,"width":200,"height":80}],"edges":[]}"#,
    )
    .unwrap();
    let storage = Arc::new(Counting { inner: LocalFs::new(&root).unwrap(), log: Mutex::default() });
    let vault = Vault::new(Arc::<Counting>::clone(&storage));
    let cfg = ServerConfig { clock: FixedClock::new(1_793_577_600_000), ..ServerConfig::default() };
    let srv = Server::with_config(vault, None, cfg);
    srv.index().flush();
    let mut over = BTreeSet::new();
    for (tool, args) in calls() {
        // Twice: the second call is the steady state, after whatever a first call caches.
        let mut worst = Cost { per_note: 0, per_folder: 0, other: 0, own: 0 };
        for _ in 0..2 {
            storage.take();
            let reply = call(&srv, tool, &args);
            assert!(reply.get("error").is_none() && reply["result"]["isError"] != true, "{tool} {args}: {reply}");
            let log = storage.take();
            if std::env::var_os("QUIRE_SHOW_GAPS").is_some() {
                let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
                for (what, path) in &log {
                    let shape = path.rsplit('/').next().unwrap_or(path).chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
                    *kinds.entry(format!("{what} {shape}")).or_default() += 1;
                }
                eprintln!("{tool}: {kinds:?}");
            }
            // The notes of this vault the reply names, and the one the call named.
            let mut own = BTreeSet::new();
            strings(&reply["result"]["structuredContent"], &mut own);
            own.retain(|s| s.starts_with("Folder ") && std::path::Path::new(s).extension().is_some_and(|e| e.eq_ignore_ascii_case("md")));
            own.extend(args["path"].as_str().map(str::to_string));
            let c = cost(&log, &own);
            worst = Cost {
                per_note: worst.per_note.max(c.per_note),
                per_folder: worst.per_folder.max(c.per_folder),
                other: worst.other.max(c.other),
                own: worst.own.max(c.own),
            };
        }
        eprintln!(
            "{tool}: {} calls on note paths, at most {} on one folder's lock files, {} others ({} notes named or returned)",
            worst.per_note, worst.per_folder, worst.other, worst.own
        );
        if worst.per_note > 0 || worst.per_folder > PER_FOLDER || worst.other > OTHER + PER_ROW * worst.own {
            over.insert(tool.to_string());
        }
    }
    let known: BTreeSet<String> = OVER_BUDGET.iter().map(|(t, _)| (*t).to_string()).collect();
    let fresh: Vec<_> = over.difference(&known).collect();
    let closed: Vec<_> = known.difference(&over).collect();
    assert!(fresh.is_empty(), "these tools read the disk per note and are not known gaps: {fresh:?}");
    assert!(closed.is_empty(), "these tools are within budget now: delete them from OVER_BUDGET: {closed:?}");
}
