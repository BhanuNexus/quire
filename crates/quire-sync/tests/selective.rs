// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Selective sync: a device that holds only some folders of a vault never
//! sends the notes it doesn't hold as deletes, whatever happens to them.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::{BTreeMap, BTreeSet};

use quire_sync::engine::fingerprint;
use quire_sync::{Content, DeviceId, Engine, Keys, LocalStore, MemoryServer, MemoryStore, SpaceId, SyncState, VaultKey};

const TODAY: &str = "2026-11-05";

/// A folder on disk of which only some top-level folders sync.
#[derive(Default)]
struct Scoped {
    disk: BTreeMap<String, String>,
    /// Folder prefixes (`Work/`); empty: everything.
    only: Vec<String>,
    /// The files outside the selection after the last pass, as the app's
    /// store remembers them: only a file that appeared since can be where
    /// a note was moved out to.
    before: Option<BTreeSet<String>>,
}

impl Scoped {
    fn inside(&self, p: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|o| p.starts_with(o.as_str()))
    }

    fn remember(&mut self) {
        self.before = Some(self.disk.keys().filter(|p| !self.inside(p)).cloned().collect());
    }

    fn sync(&mut self, e: &mut Engine, server: &MemoryServer) -> quire_sync::Report {
        let r = e.sync(self, server, TODAY).unwrap();
        self.remember();
        r
    }
}

impl LocalStore for Scoped {
    fn list(&self) -> Vec<String> {
        self.disk.keys().filter(|p| self.inside(p)).cloned().collect()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.disk.get(path).filter(|_| self.inside(path)).cloned()
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        assert!(self.inside(path), "the engine wrote {path}, outside the selection");
        self.disk.insert(path.into(), text.into());
        Ok(())
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        assert!(self.inside(path), "the engine removed {path}, outside the selection");
        self.disk.remove(path);
        Ok(())
    }
    fn in_scope(&self, path: &str) -> bool {
        self.inside(path)
    }
    fn moved_out(&self, base: &str, skip: &BTreeSet<String>) -> Option<(String, String)> {
        self.disk
            .iter()
            .find(|(p, t)| {
                !self.inside(p) && !skip.contains(*p) && self.before.as_ref().is_none_or(|b| !b.contains(*p)) && fingerprint(p, t) == base
            })
            .map(|(p, t)| (p.clone(), t.clone()))
    }
}

struct World {
    server: MemoryServer,
    keys: Keys,
    space: SpaceId,
    full: (Engine, MemoryStore),
    part: (Engine, Scoped),
}

impl World {
    fn new(only: &[&str]) -> Self {
        let key = VaultKey::generate().unwrap();
        let space = SpaceId::random().unwrap();
        let engine = |name: &str| Engine::new(space.clone(), DeviceId::random().unwrap(), name, key.keys(), SyncState::default());
        Self {
            server: MemoryServer::new(),
            keys: key.keys(),
            full: (engine("desktop"), MemoryStore::default()),
            part: (engine("server"), Scoped { only: only.iter().map(|s| (*s).to_string()).collect(), ..Scoped::default() }),
            space,
        }
    }

    fn settle(&mut self) {
        for _ in 0..4 {
            self.full.0.sync(&mut self.full.1, &self.server, TODAY).unwrap();
            self.part.1.sync(&mut self.part.0, &self.server);
        }
    }

    /// Every revision the server holds, opened.
    fn log(&self) -> Vec<Content> {
        self.server.dump().iter().map(|r| Content::open(&self.keys, &self.space, r).unwrap()).collect()
    }

    fn deletes(&self) -> Vec<String> {
        self.log().into_iter().filter(|c| c.deleted).map(|c| c.path).collect()
    }

    fn full_has(&self, p: &str) -> Option<&str> {
        self.full.1.0.get(p).map(String::as_str)
    }

    fn part_has(&self, p: &str) -> Option<&str> {
        self.part.1.disk.get(p).map(String::as_str)
    }
}

#[test]
fn notes_outside_the_selection_are_never_sent_as_deletes() {
    let mut w = World::new(&["Work/"]);
    for (p, t) in [("Work/a.md", "a\n"), ("Home/b.md", "b\n"), ("c.md", "c\n"), ("Home/pic.png", "iVBORw0K")] {
        w.full.1.write(p, t).unwrap();
    }
    // A file the partial device has on disk outside its selection: never sent.
    w.part.1.disk.insert("Home/local only.md".into(), "mine\n".into());
    w.settle();
    assert_eq!(w.part.1.disk.keys().cloned().collect::<Vec<_>>(), ["Home/local only.md", "Work/a.md"]);
    assert!(w.log().iter().all(|c| c.path != "Home/local only.md"));

    // The other device edits notes outside the selection, many passes go by.
    w.full.1.write("Home/b.md", "b, edited\n").unwrap();
    w.full.1.write("c.md", "c, edited\n").unwrap();
    w.settle();
    w.settle();
    assert_eq!(w.full_has("Home/b.md"), Some("b, edited\n"));
    assert_eq!(w.full_has("c.md"), Some("c, edited\n"));
    assert_eq!(w.full_has("Home/pic.png"), Some("iVBORw0K"));
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());
    assert!(w.part.0.state.files.values().filter(|t| !t.path.starts_with("Work/")).all(|t| t.outside));

    // A note moved out of the selection elsewhere leaves this device, and
    // stays on the other one.
    w.full.1.0.remove("Work/a.md");
    w.full.1.write("Home/a.md", "a\n").unwrap();
    w.settle();
    assert_eq!(w.part_has("Work/a.md"), None);
    assert_eq!(w.full_has("Home/a.md"), Some("a\n"));
    // One moved in arrives.
    w.full.1.0.remove("Home/b.md");
    w.full.1.write("Work/b.md", "b, edited\n").unwrap();
    w.settle();
    assert_eq!(w.part_has("Work/b.md"), Some("b, edited\n"));
    assert!(w.deletes().is_empty(), "renames are never deletes: {:?}", w.deletes());

    // Moved here into a folder this device doesn't sync: sent as a rename.
    let text = w.part.1.disk.remove("Work/b.md").unwrap();
    w.part.1.disk.insert("Home/b moved.md".into(), text);
    w.settle();
    assert_eq!(w.full_has("Home/b moved.md"), Some("b, edited\n"));
    assert_eq!(w.full_has("Work/b.md"), None);
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());

    // A real delete inside the selection still travels.
    w.part.1.write("Work/new.md", "new\n").unwrap();
    w.settle();
    assert_eq!(w.full_has("Work/new.md"), Some("new\n"));
    w.part.1.remove("Work/new.md").unwrap();
    w.settle();
    assert_eq!(w.full_has("Work/new.md"), None);
    assert_eq!(w.deletes(), ["Work/new.md"]);
}

#[test]
fn edits_here_survive_a_move_out_of_the_selection_elsewhere() {
    let mut w = World::new(&["Work/"]);
    w.full.1.write("Work/y.md", "one\ntwo\n").unwrap();
    w.settle();
    // Both change it before seeing each other: moved out there, edited here.
    w.full.1.0.remove("Work/y.md");
    w.full.1.write("Home/y.md", "one\ntwo\n").unwrap();
    w.full.0.sync(&mut w.full.1, &w.server, TODAY).unwrap();
    w.part.1.disk.insert("Work/y.md".into(), "one\ntwo\nthree (typed on the server)\n".into());
    w.settle();
    // What was typed here follows the note.
    assert_eq!(w.full_has("Home/y.md"), Some("one\ntwo\nthree (typed on the server)\n"));
    assert_eq!(w.full_has("Work/y.md"), None);
    assert_eq!(w.part_has("Work/y.md"), None);
    assert!(w.deletes().is_empty());

    // Changes that don't merge: this device's text is kept as a note of its own.
    w.full.1.write("Work/z.md", "one\n").unwrap();
    w.settle();
    w.full.1.0.remove("Work/z.md");
    w.full.1.write("Home/z.md", "one\n").unwrap();
    w.full.0.sync(&mut w.full.1, &w.server, TODAY).unwrap();
    w.full.1.write("Home/z.md", "one, there\n").unwrap();
    w.full.0.sync(&mut w.full.1, &w.server, TODAY).unwrap();
    w.part.1.disk.insert("Work/z.md".into(), "one, here\n".into());
    w.settle();
    assert_eq!(w.full_has("Home/z.md"), Some("one, there\n"));
    assert_eq!(w.full_has("Work/z.md"), Some("one, here\n"), "kept as a note of its own");
    assert!(w.deletes().is_empty());
}

#[test]
fn widening_the_selection_fetches_and_narrowing_never_deletes() {
    let mut w = World::new(&["Work/"]);
    for (p, t) in [("Work/a.md", "a\n"), ("Home/b.md", "b\n"), ("c.md", "c\n")] {
        w.full.1.write(p, t).unwrap();
    }
    w.settle();
    w.full.1.write("Home/b.md", "b2\n").unwrap();
    w.settle();
    // A stale copy on disk outside the selection, from before.
    w.part.1.disk.insert("c.md".into(), "c (old copy)\n".into());

    w.part.1.only.clear();
    w.settle();
    assert_eq!(w.part_has("Home/b.md"), Some("b2\n"));
    assert_eq!(w.part_has("c.md"), Some("c\n"));
    assert!(w.part.1.disk.values().any(|t| t == "c (old copy)\n"), "the old copy is kept beside it");
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());

    w.part.1.only = vec!["Home/".into()];
    w.settle();
    w.settle();
    assert_eq!(w.full_has("Work/a.md"), Some("a\n"));
    assert_eq!(w.full_has("c.md"), Some("c\n"));
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());
    // Still in step for what it holds.
    w.full.1.write("Home/b.md", "b3\n").unwrap();
    w.settle();
    assert_eq!(w.part_has("Home/b.md"), Some("b3\n"));
}

fn names(m: &BTreeMap<String, String>) -> Vec<&str> {
    m.keys().map(String::as_str).collect()
}

/// A name the engine gives a second note it filed at the same path.
fn numbered(p: &str) -> bool {
    p.contains(" (2)") || p.contains(" (3)") || p.contains("(moved)") || p.contains("(unsynced)")
}

/// Review finding 2: narrowing then widening on a server transport (every
/// revision comes back from `pull(0)`) copied every note edited more than
/// once to `Name (2).md` on all devices.
#[test]
fn narrow_then_widen_never_duplicates_notes() {
    let mut w = World::new(&[]);
    w.full.1.write("Work/a.md", "a\n").unwrap();
    w.full.1.write("Home/b.md", "b\n").unwrap();
    w.settle();
    for t in ["b2\n", "b3\n"] {
        w.full.1.write("Home/b.md", t).unwrap();
        w.settle();
    }
    w.part.1.only = vec!["Work/".into()];
    w.settle();
    w.part.1.only.clear();
    w.settle();
    assert_eq!(names(&w.full.1.0), ["Home/b.md", "Work/a.md"]);
    assert_eq!(names(&w.part.1.disk), ["Home/b.md", "Work/a.md"]);
    assert_eq!(w.part_has("Home/b.md"), Some("b3\n"));
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());

    // Changed elsewhere while left out: the copy here is an older version
    // of the same note, so it is brought up to date, not kept beside it.
    w.part.1.only = vec!["Work/".into()];
    w.settle();
    w.full.1.write("Home/b.md", "b4\n").unwrap();
    w.full.1.write("Home/new.md", "new\n").unwrap();
    w.settle();
    w.part.1.only.clear();
    w.settle();
    assert_eq!(names(&w.full.1.0), ["Home/b.md", "Home/new.md", "Work/a.md"]);
    assert_eq!(w.part.1.disk, w.full.1.0);
    assert_eq!(w.part_has("Home/b.md"), Some("b4\n"));
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());
}

/// Review finding 2, the other ways to a fresh state over a synced folder:
/// a state file lost or switched (`config.json` edited by hand), or a
/// version that doesn't read the state file. Nothing is copied, nothing
/// deleted, and an old copy of a note that moved on isn't sent back.
#[test]
fn a_fresh_state_over_a_synced_folder_adopts_the_notes() {
    let mut w = World::new(&[]);
    for (p, t) in [("Work/a.md", "a\n"), ("Home/b.md", "b\n"), ("Home/c.md", "c\n")] {
        w.full.1.write(p, t).unwrap();
    }
    w.settle();
    w.full.1.write("Home/b.md", "b2\n").unwrap();
    w.full.1.write("Work/a.md", "a2\n").unwrap();
    w.settle();
    // While this device is away, a note is renamed and another edited.
    let c = w.full.1.0.remove("Home/c.md").unwrap();
    w.full.1.write("Archive/c.md", &c).unwrap();
    w.full.1.write("Home/b.md", "b3\n").unwrap();
    w.full.0.sync(&mut w.full.1, &w.server, TODAY).unwrap();
    // It comes back with no state.
    w.part.0.state = SyncState::default();
    w.settle();
    assert_eq!(names(&w.full.1.0), ["Archive/c.md", "Home/b.md", "Work/a.md"]);
    assert_eq!(w.part.1.disk, w.full.1.0);
    assert!(w.deletes().is_empty(), "{:?}", w.deletes());
}

/// Review finding 3: widening replayed a note's history, so it briefly
/// claimed its old path and moved the note living there now to
/// `Name (2).md` on every device.
#[test]
fn widening_never_renames_an_unrelated_note() {
    // Which note keeps a contested name depends on the random ids.
    for attempt in 0..16 {
        let mut w = World::new(&["Work/"]);
        w.full.1.write("Work/Todo.md", "- old\n").unwrap();
        w.settle();
        w.full.1.write("Work/Todo.md", "- old\n- done\n").unwrap();
        w.settle();
        let t = w.full.1.0.remove("Work/Todo.md").unwrap();
        w.full.1.write("Archive/Todo 2025.md", &t).unwrap();
        w.full.1.write("Work/Todo.md", "- new\n").unwrap();
        w.settle();
        assert_eq!(names(&w.part.1.disk), ["Work/Todo.md"]);
        w.part.1.only.clear();
        w.settle();
        assert_eq!(names(&w.full.1.0), ["Archive/Todo 2025.md", "Work/Todo.md"], "attempt {attempt}");
        assert_eq!(w.full_has("Work/Todo.md"), Some("- new\n"));
        assert_eq!(w.part.1.disk, w.full.1.0, "attempt {attempt}");
        assert!(w.deletes().is_empty(), "{:?}", w.deletes());
    }
}

/// Moved on elsewhere while left out here: the copy left at the old path
/// isn't sent back as a note of its own when the selection grows.
#[test]
fn widening_drops_old_copies_of_notes_that_moved_on() {
    let mut w = World::new(&[]);
    w.full.1.write("Home/x.md", "x\n").unwrap();
    w.full.1.write("Home/gone.md", "gone\n").unwrap();
    w.full.1.write("Home/mine.md", "mine\n").unwrap();
    w.settle();
    w.part.1.only = vec!["Work/".into()];
    w.settle();
    let x = w.full.1.0.remove("Home/x.md").unwrap();
    w.full.1.write("Work/x.md", &x).unwrap();
    w.full.1.0.remove("Home/gone.md");
    let m = w.full.1.0.remove("Home/mine.md").unwrap();
    w.full.1.write("Home/Sub/mine.md", &m).unwrap();
    w.settle();
    assert_eq!(w.part_has("Work/x.md"), Some("x\n"));
    // Typed here while it was left out: follows the note to where it went.
    w.part.1.disk.insert("Home/mine.md".into(), "mine, edited here\n".into());
    w.part.1.only.clear();
    w.settle();
    assert_eq!(names(&w.full.1.0), ["Home/Sub/mine.md", "Work/x.md"]);
    assert_eq!(w.full_has("Home/Sub/mine.md"), Some("mine, edited here\n"));
    assert_eq!(w.part.1.disk, w.full.1.0);
    assert_eq!(w.deletes(), ["Home/gone.md"]);
}

/// Review finding 4: a note deleted inside the selection was sent as a
/// rename onto another note outside it that has the same text.
#[test]
fn a_delete_inside_the_selection_is_never_a_rename_onto_another_note() {
    let tpl = "## Standup\n- \n";
    let mut w = World::new(&[]);
    w.full.1.write("Home/Template.md", tpl).unwrap();
    w.full.1.write("Work/Standup.md", tpl).unwrap();
    w.full.1.write("Work/Empty.md", "").unwrap();
    w.full.1.write("Home/Also empty.md", "").unwrap();
    w.settle();
    w.part.1.only = vec!["Work/".into()];
    w.settle();
    // A file of this device's own, outside the selection, with the same text.
    w.part.1.disk.insert("Home/mine.md".into(), tpl.into());
    w.settle();
    w.part.1.disk.remove("Work/Standup.md");
    w.part.1.disk.remove("Work/Empty.md");
    w.settle();
    assert_eq!(names(&w.full.1.0), ["Home/Also empty.md", "Home/Template.md"]);
    let mut deletes = w.deletes();
    deletes.sort();
    assert_eq!(deletes, ["Work/Empty.md", "Work/Standup.md"]);
    assert!(w.log().iter().all(|c| c.path != "Home/mine.md"));
}

// ------------------------------------------------------------------ random selections

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
}

const FOLDERS: [&str; 3] = ["Work/", "Home/", "Archive/"];
const SELECTIONS: [&[&str]; 5] = [&[], &["Work/"], &["Home/"], &["Work/", "Archive/"], &["Archive/"]];

/// Random selection changes interleaved with edits, deletes and moves on
/// two or three devices: no note is ever duplicated, no word typed is
/// lost unless a device that had it deleted the note, and no delete is
/// sent that a device didn't make.
fn simulate_selections(seed: u64, n: usize, steps: usize) {
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    let server = MemoryServer::new();
    let mut devs: Vec<(Engine, Scoped)> = (0..n)
        .map(|i| {
            (Engine::new(space.clone(), DeviceId::random().unwrap(), format!("d{i}"), key.keys(), SyncState::default()), Scoped::default())
        })
        .collect();
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut token = 0u64;
    // Words typed, and words a device deleted along with a note it had.
    let mut typed: Vec<String> = Vec::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    // Paths a device deleted a note from (or moved one away from).
    let mut deleted_paths: BTreeSet<String> = BTreeSet::new();
    for _ in 0..steps {
        let i = rng.below(n);
        let (e, d) = &mut devs[i];
        token += 1;
        let word = format!("w{token}");
        let held: Vec<String> = d.list();
        let op = rng.below(10);
        if std::env::var_os("SYNC_TRACE").is_some() {
            eprintln!("step {token}: dev {i} op {op} only {:?}\n  disk {:?}", d.only, d.disk);
        }
        match op {
            // Sync.
            0..=2 => {
                let seen = server.len();
                let r = d.sync(e, &server);
                if std::env::var_os("SYNC_TRACE").is_some() {
                    eprintln!("  sync {r:?}\n  -> {:?}", d.disk);
                    for rv in &server.dump()[seen..] {
                        let c = Content::open(&key.keys(), &space, rv).unwrap();
                        eprintln!("  pushed {} {} del={} {:?}", &rv.file.0[..6], c.path, c.deleted, c.text);
                    }
                }
            }
            // A new note, under a name only this device uses.
            3 => {
                let folder = FOLDERS[rng.below(FOLDERS.len())];
                let p = format!("{folder}d{i}-{token}.md");
                if d.inside(&p) {
                    d.disk.insert(p, format!("{word}\n"));
                    typed.push(word);
                }
            }
            // Add a line to a note this device holds.
            4 | 5 if !held.is_empty() => {
                let p = &held[rng.below(held.len())];
                let mut lines: Vec<String> = d.disk[p].lines().map(str::to_string).collect();
                let at = rng.below(lines.len() + 1);
                lines.insert(at, word.clone());
                d.disk.insert(p.clone(), lines.join("\n") + "\n");
                typed.push(word);
            }
            // Delete one.
            6 if !held.is_empty() => {
                let p = &held[rng.below(held.len())];
                let text = d.disk.remove(p).unwrap();
                dropped.extend(text.split_whitespace().map(str::to_string));
                deleted_paths.insert(p.clone());
            }
            // Move one to another folder, maybe one this device doesn't sync.
            7 if !held.is_empty() => {
                let p = held[rng.below(held.len())].clone();
                let folder = FOLDERS[rng.below(FOLDERS.len())];
                let to = format!("{folder}d{i}-{token}.md");
                let text = d.disk.remove(&p).unwrap();
                d.disk.insert(to, text);
                // A move of a note changed since its last pass is sent as a
                // delete and a new note, as any rename with edits is.
                deleted_paths.insert(p);
            }
            // Change which folders this device syncs, once what it has is sent
            // (`sync scope` runs while no sync does; the pass before it sent
            // everything typed here).
            8 | 9 => {
                let seen = server.len();
                d.sync(e, &server);
                if std::env::var_os("SYNC_TRACE").is_some() {
                    for rv in &server.dump()[seen..] {
                        let c = Content::open(&key.keys(), &space, rv).unwrap();
                        eprintln!("  pushed {} {} del={} {:?}", &rv.file.0[..6], c.path, c.deleted, c.text);
                    }
                }
                d.only = SELECTIONS[rng.below(SELECTIONS.len())].iter().map(|s| (*s).to_string()).collect();
            }
            _ => {}
        }
    }
    // Everyone ends up holding everything.
    let trace = std::env::var_os("SYNC_TRACE").is_some();
    for round in 0..7 {
        for (i, (e, d)) in devs.iter_mut().enumerate() {
            d.only.clear();
            let seen = server.len();
            let r = d.sync(e, &server);
            if trace {
                eprintln!("final {round} dev {i}: {r:?}\n  -> {:?}", d.disk);
                for rv in &server.dump()[seen..] {
                    let c = Content::open(&key.keys(), &space, rv).unwrap();
                    eprintln!("  pushed {} {} del={} {:?}", &rv.file.0[..6], c.path, c.deleted, c.text);
                }
            }
        }
    }
    let first = devs[0].1.disk.clone();
    for (_, d) in &devs[1..] {
        assert_eq!(d.disk, first, "seed {seed}: devices disagree");
    }
    let dups: Vec<&String> = first.keys().filter(|p| numbered(p)).collect();
    assert!(dups.is_empty(), "seed {seed}: duplicated {dups:?}\n{first:?}");
    let all: BTreeSet<&str> = first.values().flat_map(|t| t.split_whitespace()).collect();
    for w in &typed {
        assert!(all.contains(w.as_str()) || dropped.contains(w), "seed {seed}: lost {w}\n{first:?}");
    }
    let opened: Vec<Content> = server.dump().iter().map(|r| Content::open(&key.keys(), &space, r).unwrap()).collect();
    for c in opened.iter().filter(|c| c.deleted) {
        assert!(deleted_paths.contains(&c.path), "seed {seed}: a delete nobody made: {}", c.path);
    }
}

#[test]
fn random_selection_changes_never_duplicate_lose_or_delete() {
    if let Some(seed) = std::env::var("SYNC_SEED").ok().and_then(|s| s.parse().ok()) {
        simulate_selections(seed, 2 + usize::try_from(seed % 2).unwrap(), 120);
        return;
    }
    for seed in 1..=300 {
        simulate_selections(seed, 2 + usize::try_from(seed % 2).unwrap(), 120);
    }
}
