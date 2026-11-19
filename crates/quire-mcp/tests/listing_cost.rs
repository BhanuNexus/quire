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
//! today are listed in [`OVER_BUDGET`] with the step that fixes them; the test fails if the list
//! is wrong in either direction.
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
use quire_mcp::{Server, ServerConfig};
use serde_json::{Value, json};

const NOTES: usize = 2_000;
const FOLDERS: usize = 40;
/// Folder checks allowed per distinct folder, and calls allowed besides them and the notes.
const PER_FOLDER: usize = 2;
const OTHER: usize = 8;

/// The tools that note names, with arguments that make them work on this vault.
fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("list", json!({})),
        ("tags", json!({})),
        ("properties", json!({})),
        ("tasks", json!({})),
        ("links", json!({"path": "Folder 3/Note 3.md"})),
        ("vault_info", json!({})),
        ("query_view", json!({"spec": "group: status"})),
    ]
}

/// The tools that read the disk per note today, by name, and the step that stops it.
const OVER_BUDGET: &[(&str, &str)] = &[
    ("list", "step 2: the listing tools move to Visibility, one lock check per folder"),
    ("tags", "step 2: the listing tools move to Visibility, one lock check per folder"),
    ("properties", "step 2: the listing tools move to Visibility, one lock check per folder"),
    ("tasks", "step 2: the listing tools move to Visibility, one lock check per folder"),
    ("links", "step 2: the listing tools move to Visibility, one lock check per folder"),
    ("vault_info", "step 2: the listing tools move to Visibility, one lock check per folder"),
    ("query_view", "step 2: the listing tools move to Visibility, one lock check per folder"),
];

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
struct Cost {
    per_note: usize,
    per_folder: usize,
    other: usize,
}

fn cost(log: &[(&'static str, String)]) -> Cost {
    let (mut per_note, mut other) = (0, 0);
    let mut folders: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, path) in log {
        let name = path.rsplit('/').next().unwrap_or(path);
        if path.is_empty() || path.starts_with(".quire/") || name == "AGENTS.md" || name == "CLAUDE.md" {
            other += 1;
        } else if name.starts_with("quire-lock") {
            *folders.entry(path.rsplit_once('/').map_or("", |(f, _)| f)).or_default() += 1;
        } else {
            per_note += 1;
        }
    }
    Cost { per_note, per_folder: folders.values().copied().max().unwrap_or(0), other }
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

fn call(srv: &Server, name: &str, args: &Value) -> Value {
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "probe", "version": "1"}});
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}});
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
    let storage = Arc::new(Counting { inner: LocalFs::new(&root).unwrap(), log: Mutex::default() });
    let vault = Vault::new(Arc::<Counting>::clone(&storage));
    let cfg = ServerConfig { clock: FixedClock::new(1_793_577_600_000), ..ServerConfig::default() };
    let srv = Server::with_config(vault, None, cfg);
    srv.index().flush();
    let mut over = BTreeSet::new();
    for (tool, args) in calls() {
        // Twice: the second call is the steady state, after whatever a first call caches.
        let mut worst = Cost { per_note: 0, per_folder: 0, other: 0 };
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
            let c = cost(&log);
            worst = Cost {
                per_note: worst.per_note.max(c.per_note),
                per_folder: worst.per_folder.max(c.per_folder),
                other: worst.other.max(c.other),
            };
        }
        eprintln!(
            "{tool}: {} calls on note paths, at most {} on one folder's lock files, {} others",
            worst.per_note, worst.per_folder, worst.other
        );
        if worst.per_note > 0 || worst.per_folder > PER_FOLDER || worst.other > OTHER {
            over.insert(tool.to_string());
        }
    }
    let known: BTreeSet<String> = OVER_BUDGET.iter().map(|(t, _)| (*t).to_string()).collect();
    let fresh: Vec<_> = over.difference(&known).collect();
    let closed: Vec<_> = known.difference(&over).collect();
    assert!(fresh.is_empty(), "these tools read the disk per note and are not known gaps: {fresh:?}");
    assert!(closed.is_empty(), "these tools are within budget now: delete them from OVER_BUDGET: {closed:?}");
}
