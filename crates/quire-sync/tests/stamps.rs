// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stamps and base prints (`docs/design/0.14-sync-speed.md` §3.1, §3.2): a
//! pass with nothing to do reads no note, one changed note is read once, a
//! change that kept its stamp is found by a verify pass, and a merge whose
//! base text is gone keeps both versions.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds fixtures with std directly"
)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use quire_sync::{DeviceId, Engine, LocalStore, MemoryBases, MemoryServer, SpaceId, Stamp, SyncState, VaultKey};
use sha2::{Digest as _, Sha256};

const TODAY: &str = "2026-11-01";

/// A folder whose stamps work like the vault manifest's rows: a file the
/// engine wrote or read has the stamp of those bytes; a new file is hashed.
/// An edit made by the person updates the row too, unless `fixed`: then it
/// keeps the old row, as an edit that kept its size and time does until a
/// verify pass.
#[derive(Default)]
struct Store {
    files: BTreeMap<String, String>,
    rows: RefCell<BTreeMap<String, Stamp>>,
    fixed: bool,
    reads: Cell<usize>,
}

fn stamp_of(text: &str) -> Stamp {
    Stamp(Sha256::digest(text.as_bytes()).into())
}

impl Store {
    /// The person edits (or makes) a note.
    fn edit(&mut self, path: &str, text: &str) {
        self.files.insert(path.to_string(), text.to_string());
        if !self.fixed || !self.rows.borrow().contains_key(path) {
            self.rows.borrow_mut().insert(path.to_string(), stamp_of(text));
        }
    }

    /// Rehash every file (the manifest's verify pass).
    fn verify(&self) {
        *self.rows.borrow_mut() = self.files.iter().map(|(p, t)| (p.clone(), stamp_of(t))).collect();
    }

    fn take_reads(&self) -> usize {
        self.reads.replace(0)
    }
}

impl LocalStore for Store {
    fn list(&self) -> Result<Vec<String>, String> {
        Ok(self.files.keys().cloned().collect())
    }
    fn read(&self, path: &str) -> Option<String> {
        let t = self.files.get(path).cloned()?;
        self.reads.set(self.reads.get() + 1);
        Some(t)
    }
    fn read_stamped(&self, path: &str) -> Option<(String, Option<Stamp>)> {
        let t = self.read(path)?;
        // A read that finds other bytes than the row corrects the row.
        let s = stamp_of(&t);
        self.rows.borrow_mut().insert(path.to_string(), s);
        Some((t, Some(s)))
    }
    fn stamp(&self, path: &str) -> Option<Stamp> {
        self.files.contains_key(path).then(|| self.rows.borrow().get(path).copied()).flatten()
    }
    fn missing(&self, path: &str) -> bool {
        !self.files.contains_key(path)
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        self.files.insert(path.to_string(), text.to_string());
        self.rows.borrow_mut().insert(path.to_string(), stamp_of(text));
        Ok(())
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.files.remove(path);
        self.rows.borrow_mut().remove(path);
        Ok(())
    }
}

struct Device {
    engine: Engine,
    store: Store,
}

fn devices(n: usize, fixed: bool) -> (MemoryServer, Vec<Device>) {
    let server = MemoryServer::new();
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    let devs = (0..n)
        .map(|i| Device {
            engine: Engine::new(space.clone(), DeviceId::random().unwrap(), format!("device {i}"), key.keys(), SyncState::default()),
            store: Store { fixed, ..Store::default() },
        })
        .collect();
    (server, devs)
}

fn sync(d: &mut Device, server: &MemoryServer) -> quire_sync::Report {
    d.engine.sync(&mut d.store, server, TODAY).unwrap()
}

fn settle(devs: &mut [Device], server: &MemoryServer) {
    for _ in 0..10 {
        let mut changed = false;
        for d in devs.iter_mut() {
            changed |= sync(d, server).changed_anything();
        }
        if !changed {
            return;
        }
    }
    panic!("devices never settled");
}

#[test]
fn an_idle_pass_reads_no_note_and_one_change_reads_one() {
    let (server, mut devs) = devices(2, false);
    for i in 0..50 {
        devs[0].store.edit(&format!("Note {i}.md"), &format!("note {i}\n"));
    }
    settle(&mut devs, &server);
    assert_eq!(devs[1].store.files, devs[0].store.files);
    for d in &devs {
        d.store.take_reads();
    }
    for d in &mut devs {
        let r = sync(d, &server);
        assert!(!r.changed_anything());
        assert_eq!(d.store.take_reads(), 0, "an idle pass reads no note");
    }
    devs[0].store.edit("Note 7.md", "note 7, changed\n");
    let r = sync(&mut devs[0], &server);
    assert_eq!(r.pushed, 1);
    assert_eq!(devs[0].store.take_reads(), 1, "the changed note is read once");
    // The other device writes it, and knows it by its stamp from then on.
    sync(&mut devs[1], &server);
    assert_eq!(devs[1].store.files["Note 7.md"], "note 7, changed\n");
    devs[1].store.take_reads();
    sync(&mut devs[1], &server);
    sync(&mut devs[0], &server);
    assert_eq!(devs[0].store.take_reads() + devs[1].store.take_reads(), 0);
}

#[test]
fn a_fresh_state_reads_each_note_once_then_nothing() {
    // As after a migration from 0.13: no stamp is known yet.
    let (server, mut devs) = devices(1, false);
    for i in 0..20 {
        devs[0].store.edit(&format!("{i}.md"), "x\n");
    }
    sync(&mut devs[0], &server);
    let fresh = devs[0].engine.state.files.values().map(|t| t.disk).collect::<Vec<_>>();
    assert!(fresh.iter().all(Option::is_some), "a pushed note is known by the stamp of what was read");
    for t in devs[0].engine.state.files.values_mut() {
        t.disk = None;
    }
    devs[0].store.take_reads();
    sync(&mut devs[0], &server);
    assert_eq!(devs[0].store.take_reads(), 20, "each note read once, found unchanged");
    sync(&mut devs[0], &server);
    assert_eq!(devs[0].store.take_reads(), 0);
}

#[test]
fn an_edit_that_kept_its_stamp_is_found_by_a_verify_pass() {
    let (server, mut devs) = devices(2, true);
    devs[0].store.edit("Plan.md", "one\n");
    settle(&mut devs, &server);
    devs[0].store.edit("Plan.md", "one\ntwo\n");
    settle(&mut devs, &server);
    assert_eq!(devs[1].store.files["Plan.md"], "one\n", "a stamp that didn't change hides the edit");
    devs[0].store.verify();
    settle(&mut devs, &server);
    assert_eq!(devs[1].store.files["Plan.md"], "one\ntwo\n");
}

#[test]
fn an_edit_that_kept_its_stamp_merges_with_an_incoming_change() {
    // The incoming revision is merged with what is on disk, read in full:
    // a stale stamp never makes the engine overwrite an edit.
    let (server, mut devs) = devices(2, true);
    devs[0].store.fixed = false;
    devs[0].store.edit("Plan.md", "a\nb\nc\n");
    settle(&mut devs, &server);
    devs[1].store.edit("Plan.md", "a\nb\nC here\n");
    devs[0].store.edit("Plan.md", "A there\nb\nc\n");
    sync(&mut devs[0], &server);
    sync(&mut devs[1], &server);
    assert_eq!(devs[1].store.files["Plan.md"], "A there\nb\nC here\n");
    devs[1].store.verify();
    settle(&mut devs, &server);
    assert_eq!(devs[0].store.files, devs[1].store.files);
}

#[test]
fn a_missing_base_keeps_both_versions() {
    let (server, mut devs) = devices(2, false);
    devs[0].store.edit("Plan.md", "a\nb\nc\n");
    settle(&mut devs, &server);
    // The base's text is gone on device 1 (a damaged pack).
    let _ = devs[1].engine.take_bases();
    devs[0].store.edit("Plan.md", "A\nb\nc\n");
    devs[1].store.edit("Plan.md", "a\nb\nC\n");
    sync(&mut devs[0], &server);
    let r = sync(&mut devs[1], &server);
    assert!(r.merged.is_empty(), "nothing to merge from");
    assert_eq!(r.conflict_copies.len(), 1, "{r:?}");
    assert_eq!(devs[1].store.files["Plan.md"], "a\nb\nC\n", "this device's version stays in place");
    assert_eq!(devs[1].store.files[&r.conflict_copies[0]], "A\nb\nc\n", "the other beside it");
    settle(&mut devs, &server);
    assert_eq!(devs[0].store.files, devs[1].store.files);
}

#[test]
fn an_unchanged_note_takes_an_incoming_version_without_its_base() {
    // Only a merge needs the base's text; "changed here?" compares prints.
    let (server, mut devs) = devices(2, false);
    devs[0].store.edit("Plan.md", "a\n");
    settle(&mut devs, &server);
    devs[1].engine = Engine::new(
        devs[1].engine.space.clone(),
        devs[1].engine.device.clone(),
        "device 1",
        devs[1].engine.keys().clone(),
        devs[1].engine.state.clone(),
    )
    .with_bases(Box::new(MemoryBases::default()));
    devs[0].store.edit("Plan.md", "a\nb\n");
    settle(&mut devs, &server);
    assert_eq!(devs[1].store.files["Plan.md"], "a\nb\n");
    assert_eq!(devs[1].store.files.len(), 1);
}

// ------------------------------------------------------------------ random

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn seeds() -> std::ops::RangeInclusive<u64> {
    1..=std::env::var("SYNC_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(150)
}

/// Random appends on three devices with random passes in between; with
/// `fixed` stamps, every edit keeps its stamp until a verify pass at the
/// end. Every device ends with the same notes and no word is lost.
fn simulate(seed: u64, fixed: bool) {
    let (server, mut devs) = devices(3, fixed);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let names = ["A.md", "B.md", "Notes/C.md", "pic.png"];
    let mut words = Vec::new();
    for step in 0..60 {
        let i = rng.below(3);
        if rng.below(3) == 0 {
            sync(&mut devs[i], &server);
        } else {
            let p = names[rng.below(names.len())];
            let w = format!("w{step}");
            let mut lines: Vec<String> = devs[i].store.files.get(p).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
            let at = rng.below(lines.len() + 1);
            lines.insert(at, w.clone());
            devs[i].store.edit(p, &(lines.join("\n") + "\n"));
            words.push(w);
        }
    }
    settle(&mut devs, &server);
    for d in &devs {
        d.store.verify();
    }
    settle(&mut devs, &server);
    for d in &devs[1..] {
        assert_eq!(d.store.files, devs[0].store.files, "seed {seed}: devices disagree");
    }
    let all: String = devs[0].store.files.values().cloned().collect::<Vec<_>>().join("\n");
    for w in &words {
        assert!(all.split_whitespace().any(|x| x == w), "seed {seed}: lost {w}\n{all}");
    }
}

#[test]
fn random_edits_with_honest_stamps_never_lose_text() {
    for seed in seeds() {
        simulate(seed, false);
    }
}

#[test]
fn random_edits_whose_stamps_stay_put_converge_after_a_verify() {
    for seed in seeds() {
        simulate(seed, true);
    }
}
