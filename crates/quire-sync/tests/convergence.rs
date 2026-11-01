// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Devices editing the same vault through an in-memory server must end up
//! with identical folders, and must never lose text anyone wrote.
#![allow(clippy::unwrap_used, reason = "test code")]

use quire_sync::{DeviceId, Engine, LocalStore, MemoryServer, MemoryStore, SpaceId, SyncState, VaultKey};

const TODAY: &str = "2026-11-01";

struct Device {
    engine: Engine,
    store: MemoryStore,
}

fn devices(n: usize) -> (MemoryServer, Vec<Device>) {
    let server = MemoryServer::new();
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    let devs = (0..n)
        .map(|i| Device {
            engine: Engine::new(space.clone(), DeviceId::random().unwrap(), format!("device {i}"), key.keys(), SyncState::default()),
            store: MemoryStore::default(),
        })
        .collect();
    (server, devs)
}

fn sync(d: &mut Device, server: &MemoryServer) -> quire_sync::Report {
    d.engine.sync(&mut d.store, server, TODAY).unwrap()
}

/// Sync everyone until a full round changes nothing.
fn settle(devs: &mut [Device], server: &MemoryServer) {
    for round in 0..10 {
        let mut changed = false;
        for (i, d) in devs.iter_mut().enumerate() {
            let r = sync(d, server);
            if round >= 7 && std::env::var_os("SYNC_TRACE").is_some() {
                eprintln!("round {round} dev {i}: {r:?}\n  {:?}", d.store.0);
            }
            changed |= r.changed_anything();
        }
        if !changed {
            return;
        }
    }
    panic!("devices never settled");
}

fn put(d: &mut Device, path: &str, text: &str) {
    d.store.write(path, text).unwrap();
}

#[test]
fn notes_travel_and_renames_and_deletes_follow() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Plan.md", "# Plan\nShip it.\n");
    put(&mut devs[0], "Ideas.md", "# Ideas\n");
    settle(&mut devs, &server);
    assert_eq!(devs[1].store, devs[0].store);

    // Rename on one device (same text, new path) is a rename everywhere.
    let text = devs[0].store.0.remove("Plan.md").unwrap();
    put(&mut devs[0], "Projects/Plan.md", &text);
    settle(&mut devs, &server);
    assert!(devs[1].store.0.contains_key("Projects/Plan.md") && !devs[1].store.0.contains_key("Plan.md"));

    devs[1].store.0.remove("Ideas.md");
    settle(&mut devs, &server);
    assert!(!devs[0].store.0.contains_key("Ideas.md"));
    assert_eq!(devs[0].store, devs[1].store);
}

#[test]
fn concurrent_edits_to_different_lines_merge() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Plan.md", "# Plan\none\ntwo\nthree\n");
    settle(&mut devs, &server);
    put(&mut devs[0], "Plan.md", "# Plan\nONE\ntwo\nthree\n");
    put(&mut devs[1], "Plan.md", "# Plan\none\ntwo\nTHREE\n");
    sync(&mut devs[0], &server);
    let r = sync(&mut devs[1], &server);
    assert_eq!(r.merged, vec!["Plan.md".to_string()]);
    settle(&mut devs, &server);
    assert_eq!(devs[0].store.read("Plan.md").unwrap(), "# Plan\nONE\ntwo\nTHREE\n");
    assert_eq!(devs[0].store, devs[1].store);
}

#[test]
fn overlapping_edits_keep_both_versions() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Plan.md", "# Plan\nShip it.\n");
    settle(&mut devs, &server);
    put(&mut devs[0], "Plan.md", "# Plan\nShip it on Friday.\n");
    put(&mut devs[1], "Plan.md", "# Plan\nShip it to everyone.\n");
    sync(&mut devs[0], &server);
    let r = sync(&mut devs[1], &server);
    assert_eq!(r.conflict_copies, vec!["Plan (conflict from device 0 2026-11-01).md".to_string()]);
    settle(&mut devs, &server);
    assert_eq!(devs[0].store, devs[1].store);
    let all: String = devs[0].store.0.values().cloned().collect();
    assert!(all.contains("Ship it on Friday.") && all.contains("Ship it to everyone."));
}

#[test]
fn an_edit_beats_a_delete() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Plan.md", "# Plan\n");
    settle(&mut devs, &server);
    devs[0].store.0.remove("Plan.md");
    put(&mut devs[1], "Plan.md", "# Plan\nStill needed.\n");
    sync(&mut devs[0], &server);
    let r = sync(&mut devs[1], &server);
    assert_eq!(r.kept_over_delete, vec!["Plan.md".to_string()]);
    settle(&mut devs, &server);
    assert_eq!(devs[0].store.read("Plan.md").unwrap(), "# Plan\nStill needed.\n");

    // The other way round: deleted here after the other device edited.
    put(&mut devs[0], "Plan.md", "# Plan\nStill needed.\nMore.\n");
    devs[1].store.0.remove("Plan.md");
    sync(&mut devs[0], &server);
    sync(&mut devs[1], &server);
    settle(&mut devs, &server);
    assert_eq!(devs[1].store.read("Plan.md").unwrap(), "# Plan\nStill needed.\nMore.\n");
}

#[test]
fn the_same_new_name_on_two_devices_keeps_both_texts() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Today.md", "# Today\nfrom laptop\n");
    put(&mut devs[1], "Today.md", "# Today\nfrom phone\n");
    settle(&mut devs, &server);
    assert_eq!(devs[0].store, devs[1].store);
    let all: String = devs[0].store.0.values().cloned().collect();
    assert!(all.contains("from laptop") && all.contains("from phone"), "{all}");
}

#[test]
fn offline_device_catches_up() {
    let (server, mut devs) = devices(3);
    put(&mut devs[0], "A.md", "a\n");
    settle(&mut devs, &server);
    server.set_offline(true);
    let d = &mut devs[2];
    assert!(d.engine.sync(&mut d.store, &server, TODAY).is_err());
    put(&mut devs[2], "A.md", "a\noffline edit\n");
    server.set_offline(false);
    put(&mut devs[1], "B.md", "b\n");
    settle(&mut devs, &server);
    assert_eq!(devs[0].store.read("A.md").unwrap(), "a\noffline edit\n");
    assert_eq!(devs[2].store.read("B.md").unwrap(), "b\n");
}

#[test]
fn the_server_sees_no_names_or_text() {
    let (server, mut devs) = devices(1);
    put(&mut devs[0], "Secret Project/Launch Plan.md", "# Launch\nCodename BLUEBIRD\n");
    settle(&mut devs, &server);
    let dump = serde_json::to_string(&server.dump()).unwrap();
    for leak in ["Secret", "Launch", "BLUEBIRD", ".md"] {
        assert!(!dump.contains(leak), "server can read {leak:?}");
    }
}

#[test]
fn state_survives_a_restart() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "A.md", "a\n");
    settle(&mut devs, &server);
    let saved = serde_json::to_string(&devs[1].engine.state).unwrap();
    let restored: SyncState = serde_json::from_str(&saved).unwrap();
    assert_eq!(restored, devs[1].engine.state);
    let before = server.len();
    devs[1].engine.state = restored;
    sync(&mut devs[1], &server);
    assert_eq!(server.len(), before, "a restarted device doesn't re-upload what's already synced");
}

// ------------------------------------------------------------------ random simulations

/// Small deterministic RNG so failures can be replayed by seed.
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

const NAMES: [&str; 5] = ["A.md", "B.md", "Notes/C.md", "Notes/D.md", "E.md"];

/// One random local change. `additive` changes only add text (no deletes or
/// line rewrites), so every word written must survive.
fn random_edit(d: &mut Device, rng: &mut Rng, token: &mut u64, additive: bool) -> Option<String> {
    *token += 1;
    let word = format!("w{token}");
    let paths = d.store.list();
    let choice = rng.below(if additive { 3 } else { 6 });
    match choice {
        // Insert a line in an existing note.
        0 | 1 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            let mut lines: Vec<String> = d.store.read(&p).unwrap().lines().map(str::to_string).collect();
            let at = rng.below(lines.len() + 1);
            lines.insert(at, word.clone());
            put(d, &p, &(lines.join("\n") + "\n"));
            Some(word)
        }
        // Create a note (or append to one with that name).
        0..=2 => {
            let p = NAMES[rng.below(NAMES.len())];
            let mut text = d.store.read(p).unwrap_or_default();
            text.push_str(&word);
            text.push('\n');
            put(d, p, &text);
            Some(word)
        }
        // Rewrite a line.
        3 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            let mut lines: Vec<String> = d.store.read(&p).unwrap().lines().map(str::to_string).collect();
            if !lines.is_empty() {
                let i = rng.below(lines.len());
                lines[i] = word;
            }
            put(d, &p, &(lines.join("\n") + "\n"));
            None
        }
        // Delete a note.
        4 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            d.store.0.remove(&p);
            None
        }
        // Rename a note.
        5 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            let to = NAMES[rng.below(NAMES.len())];
            if !d.store.0.contains_key(to) {
                let t = d.store.0.remove(&p).unwrap();
                put(d, to, &t);
            }
            None
        }
        _ => None,
    }
}

fn simulate(seed: u64, n_devices: usize, steps: usize, additive: bool) {
    let (server, mut devs) = devices(n_devices);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut token = 0;
    let mut written = Vec::new();
    for _ in 0..steps {
        let i = rng.below(devs.len());
        if rng.below(3) == 0 {
            sync(&mut devs[i], &server);
        } else if let Some(w) = random_edit(&mut devs[i], &mut rng, &mut token, additive) {
            written.push(w);
        }
    }
    settle(&mut devs, &server);
    for d in &devs[1..] {
        assert_eq!(d.store, devs[0].store, "seed {seed}: devices disagree");
    }
    if additive {
        let all: String = devs[0].store.0.values().cloned().collect::<Vec<_>>().join("\n");
        for w in &written {
            assert!(all.split_whitespace().any(|x| x == w), "seed {seed}: lost {w}\n{all}");
        }
    }
}

#[test]
fn random_edits_never_lose_text() {
    for seed in 1..=300 {
        simulate(seed, 3, 60, true);
    }
}

#[test]
fn random_edits_renames_and_deletes_converge() {
    if let Some(seed) = std::env::var("SYNC_SEED").ok().and_then(|s| s.parse().ok()) {
        simulate(seed, 4, 80, false);
        return;
    }
    for seed in 1..=300 {
        simulate(seed, 4, 80, false);
    }
}

/// Lets something happen on the server right before this device's first push,
/// to exercise the "someone else got there first" path inside one sync.
struct Racing<'a, F: FnMut()> {
    inner: &'a MemoryServer,
    before_first_push: std::cell::RefCell<Option<F>>,
}

impl<F: FnMut()> quire_sync::Transport for Racing<'_, F> {
    fn pull(&self, after: u64) -> Result<Vec<quire_sync::Revision>, quire_sync::TransportError> {
        self.inner.pull(after)
    }
    fn push(&self, rev: &quire_sync::Revision) -> Result<quire_sync::PushOutcome, quire_sync::TransportError> {
        if let Some(mut f) = self.before_first_push.borrow_mut().take() {
            f();
        }
        self.inner.push(rev)
    }
}

#[test]
fn a_push_that_loses_the_race_pulls_merges_and_retries() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Plan.md", "# Plan\none\ntwo\nthree\n");
    settle(&mut devs, &server);
    put(&mut devs[0], "Plan.md", "# Plan\nONE\ntwo\nthree\n");
    put(&mut devs[1], "Plan.md", "# Plan\none\ntwo\nTHREE\n");
    let (first, rest) = devs.split_at_mut(1);
    let other = &mut rest[0];
    let racing = Racing {
        inner: &server,
        before_first_push: std::cell::RefCell::new(Some(|| {
            other.engine.sync(&mut other.store, &server, TODAY).unwrap();
        })),
    };
    let r = first[0].engine.sync(&mut first[0].store, &racing, TODAY).unwrap();
    assert_eq!(r.merged, vec!["Plan.md".to_string()], "lost the race, then merged");
    settle(&mut devs, &server);
    assert_eq!(devs[0].store.read("Plan.md").unwrap(), "# Plan\nONE\ntwo\nTHREE\n");
    assert_eq!(devs[0].store, devs[1].store);
}

#[test]
fn a_remote_rename_never_overwrites_an_unsynced_file() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Draft.md", "draft\n");
    settle(&mut devs, &server);
    // Device 1 writes Final.md but hasn't synced; device 0 renames Draft to Final.
    put(&mut devs[1], "Final.md", "my unsynced final\n");
    let t = devs[0].store.0.remove("Draft.md").unwrap();
    put(&mut devs[0], "Final.md", &t);
    sync(&mut devs[0], &server);
    sync(&mut devs[1], &server);
    settle(&mut devs, &server);
    let all: String = devs[1].store.0.values().cloned().collect();
    assert!(all.contains("my unsynced final") && all.contains("draft"), "{:?}", devs[1].store);
    assert_eq!(devs[0].store, devs[1].store);
}

/// A folder whose files can change between a read and a write, like a real
/// vault with the app saving in the background.
#[derive(Default)]
struct Guarded {
    inner: MemoryStore,
    seen: std::collections::HashMap<String, Option<String>>,
}

impl LocalStore for Guarded {
    fn list(&self) -> Vec<String> {
        self.inner.list()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.inner.read(path)
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        if let Some(expected) = self.seen.get(path)
            && self.inner.read(path) != *expected
        {
            return Err("changed while syncing".into());
        }
        self.inner.write(path, text)
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.inner.remove(path)
    }
}

#[test]
fn a_failed_write_is_retried_not_skipped() {
    let server = MemoryServer::new();
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    let mut a = Engine::new(space.clone(), DeviceId::random().unwrap(), "a", key.keys(), SyncState::default());
    let mut b = Engine::new(space, DeviceId::random().unwrap(), "b", key.keys(), SyncState::default());
    let (mut sa, mut sb) = (MemoryStore::default(), Guarded::default());
    sa.write("A.md", "one\n").unwrap();
    a.sync(&mut sa, &server, TODAY).unwrap();
    // B's write of A.md fails once (the file "changed" underneath).
    sb.seen.insert("A.md".into(), Some("something else".into()));
    assert!(b.sync(&mut sb, &server, TODAY).is_err());
    assert_eq!(b.state.cursor, 0, "the unapplied revision isn't skipped");
    sb.seen.clear();
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.inner.read("A.md").unwrap(), "one\n");
}
