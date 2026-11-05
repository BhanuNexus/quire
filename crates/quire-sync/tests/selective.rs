// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Selective sync: a device that holds only some folders of a vault never
//! sends the notes it doesn't hold as deletes, whatever happens to them.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::BTreeMap;

use quire_sync::engine::fingerprint;
use quire_sync::{Content, DeviceId, Engine, Keys, LocalStore, MemoryServer, MemoryStore, SpaceId, SyncState, VaultKey};

const TODAY: &str = "2026-11-05";

/// A folder on disk of which only some top-level folders sync.
#[derive(Default)]
struct Scoped {
    disk: BTreeMap<String, String>,
    /// Folder prefixes (`Work/`); empty: everything.
    only: Vec<String>,
}

impl Scoped {
    fn inside(&self, p: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|o| p.starts_with(o.as_str()))
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
    fn moved_out(&self, base: &str) -> Option<(String, String)> {
        self.disk.iter().find(|(p, t)| !self.inside(p) && fingerprint(p, t) == base).map(|(p, t)| (p.clone(), t.clone()))
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
            self.part.0.sync(&mut self.part.1, &self.server, TODAY).unwrap();
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
    assert_eq!(w.full_has("Home/y.md"), Some("one\ntwo\n"));
    assert_eq!(w.full_has("Work/y.md"), Some("one\ntwo\nthree (typed on the server)\n"), "kept as a note of its own");
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
