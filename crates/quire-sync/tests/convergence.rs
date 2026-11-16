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

#[test]
fn attachments_travel_rename_and_are_remembered_by_hash() {
    let (server, mut devs) = devices(2);
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    put(&mut devs[0], "attachments/pixel.png", png);
    put(&mut devs[0], "Note.md", "![[pixel.png]]\n");
    settle(&mut devs, &server);
    assert_eq!(devs[1].store.read("attachments/pixel.png").unwrap(), png);
    // The engine keeps a hash, not the file.
    let state = serde_json::to_string(&devs[1].engine.state).unwrap();
    assert!(!state.contains(png) && state.contains("sha256:"));
    // A rename is a rename, not a delete and a new file.
    let before = server.len();
    let t = devs[1].store.0.remove("attachments/pixel.png").unwrap();
    put(&mut devs[1], "attachments/dot.png", &t);
    settle(&mut devs, &server);
    assert_eq!(server.len(), before + 1);
    assert_eq!(devs[0].store.read("attachments/dot.png").unwrap(), png);
    assert!(devs[0].store.read("attachments/pixel.png").is_none());
}

#[test]
fn an_attachment_changed_on_two_devices_keeps_both_never_merged() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "diagram.svg", "<svg>\nold\n</svg>\n");
    settle(&mut devs, &server);
    // Different lines, which a note would merge cleanly.
    put(&mut devs[0], "diagram.svg", "<svg>\nold\n</svg>\n<!-- a -->\n");
    put(&mut devs[1], "diagram.svg", "<!-- b -->\n<svg>\nold\n</svg>\n");
    sync(&mut devs[0], &server);
    let r = sync(&mut devs[1], &server);
    assert!(r.merged.is_empty() && r.conflict_copies.len() == 1, "{r:?}");
    settle(&mut devs, &server);
    assert_eq!(devs[0].store, devs[1].store);
    let all: Vec<&String> = devs[0].store.0.values().collect();
    assert!(all.iter().any(|t| t.contains("<!-- a -->") && !t.contains("<!-- b -->")));
    assert!(all.iter().any(|t| t.contains("<!-- b -->") && !t.contains("<!-- a -->")));
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

const NAMES: [&str; 6] = ["A.md", "B.md", "Notes/C.md", "Notes/D.md", "E.md", "files/pic.png"];

/// One random local change. `additive` changes only add text (no deletes or
/// line rewrites), so every word written must survive.
fn random_edit(d: &mut Device, rng: &mut Rng, token: &mut u64, additive: bool) -> Option<String> {
    *token += 1;
    let word = format!("w{token}");
    let paths = d.store.list().unwrap();
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
    fn list(&self) -> Result<Vec<String>, String> {
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

/// Review finding 9: an agent's "create" suggestion accepted on two devices
/// before they synced. Both pushed the note as new; it must end up as one
/// note, never `Plan (2).md`, whichever device syncs first.
#[test]
fn the_same_new_note_with_the_same_text_on_two_devices_is_one_note() {
    for attempt in 0..12 {
        // Device 3 joins only at the end.
        let (server, mut devs) = devices(4);
        let text = "# Plan\nWritten by the agent.\n";
        put(&mut devs[0], "Plan.md", text);
        put(&mut devs[1], "Plan.md", text);
        let (first, rest) = devs.split_at_mut(1);
        let other = &mut rest[0];
        let racing = Racing {
            inner: &server,
            before_first_push: std::cell::RefCell::new(Some(|| {
                other.engine.sync(&mut other.store, &server, TODAY).unwrap();
            })),
        };
        // Device 0 pulls nothing, device 1 pushes its note, device 0 pushes its own.
        first[0].engine.sync(&mut first[0].store, &racing, TODAY).unwrap();
        settle(&mut devs[..3], &server);
        for d in &devs[..3] {
            assert_eq!(d.store.list().unwrap(), ["Plan.md"], "attempt {attempt}: {:?}", d.store);
            assert_eq!(d.store.read("Plan.md").as_deref(), Some(text));
        }
        // Edits still travel, through the note that kept the name.
        put(&mut devs[2], "Plan.md", "# Plan\nWritten by the agent.\nThen by me.\n");
        settle(&mut devs[..3], &server);
        for d in &devs[..3] {
            assert_eq!(d.store.read("Plan.md").as_deref(), Some("# Plan\nWritten by the agent.\nThen by me.\n"));
            assert_eq!(d.store.list().unwrap().len(), 1, "attempt {attempt}: {:?}", d.store);
        }
        // A device that joins now sees one note too.
        sync(&mut devs[3], &server);
        assert_eq!(devs[3].store, devs[0].store, "attempt {attempt}");
    }
}

/// Texts that differ stay two notes (nothing typed is ever dropped).
#[test]
fn the_same_new_name_with_different_texts_on_racing_devices_keeps_both() {
    let (server, mut devs) = devices(2);
    put(&mut devs[0], "Plan.md", "# Plan\nfrom laptop\n");
    put(&mut devs[1], "Plan.md", "# Plan\nfrom phone\n");
    let (first, rest) = devs.split_at_mut(1);
    let other = &mut rest[0];
    let racing = Racing {
        inner: &server,
        before_first_push: std::cell::RefCell::new(Some(|| {
            other.engine.sync(&mut other.store, &server, TODAY).unwrap();
        })),
    };
    first[0].engine.sync(&mut first[0].store, &racing, TODAY).unwrap();
    settle(&mut devs, &server);
    assert_eq!(devs[0].store, devs[1].store);
    let all: String = devs[0].store.0.values().cloned().collect();
    assert!(all.contains("from laptop") && all.contains("from phone"), "{all}");
}

// ---- 0.13.2: deletes only for notes that are really gone ----

/// A folder whose listing can fail (a drive pulled out, a network share
/// that dropped) and whose files can be there yet unreadable for a moment.
#[derive(Default)]
struct Flaky {
    inner: MemoryStore,
    unlisted: bool,
    /// There, but neither listed nor readable right now.
    stuck: std::collections::BTreeSet<String>,
}

impl LocalStore for Flaky {
    fn list(&self) -> Result<Vec<String>, String> {
        if self.unlisted {
            return Err("the folder is gone".into());
        }
        Ok(self.inner.list()?.into_iter().filter(|p| !self.stuck.contains(p)).collect())
    }
    fn read(&self, path: &str) -> Option<String> {
        if self.unlisted || self.stuck.contains(path) { None } else { self.inner.read(path) }
    }
    fn missing(&self, path: &str) -> bool {
        self.inner.read(path).is_none()
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        if self.unlisted {
            return Err("the folder is gone".into());
        }
        self.inner.write(path, text)
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.inner.remove(path)
    }
}

fn pair() -> (MemoryServer, Engine, Engine) {
    let server = MemoryServer::new();
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    let a = Engine::new(space.clone(), DeviceId::random().unwrap(), "a", key.keys(), SyncState::default());
    let b = Engine::new(space, DeviceId::random().unwrap(), "b", key.keys(), SyncState::default());
    (server, a, b)
}

/// Reliability review #1: a listing that fails is never "no notes here".
#[test]
fn a_folder_that_cant_be_listed_stops_the_pass_and_deletes_nothing() {
    let (server, mut a, mut b) = pair();
    let (mut sa, mut sb) = (Flaky::default(), MemoryStore::default());
    for i in 0..3 {
        sa.inner.write(&format!("Note {i}.md"), "text\n").unwrap();
    }
    a.sync(&mut sa, &server, TODAY).unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    // A change from the other device waits too: nothing is written into a
    // folder that can't be read.
    sb.write("Note 0.md", "text, edited on b\n").unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    sa.unlisted = true;
    let err = a.sync(&mut sa, &server, TODAY).unwrap_err();
    assert!(matches!(err, quire_sync::SyncError::List(_)), "{err}");
    assert!(err.to_string().contains("sync stopped before sending anything"), "{err}");
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.list().unwrap().len(), 3, "nothing deleted on the other device: {sb:?}");
    sa.unlisted = false;
    a.sync(&mut sa, &server, TODAY).unwrap();
    assert_eq!(sa.inner.read("Note 0.md").unwrap(), "text, edited on b\n");
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sa.inner, sb);
}

/// A note left out of the listing but still there (it couldn't be read just
/// then) is checked again before its delete is sent: not sent.
#[test]
fn a_note_that_is_still_there_is_never_sent_as_deleted() {
    let (server, mut a, mut b) = pair();
    let (mut sa, mut sb) = (Flaky::default(), MemoryStore::default());
    sa.inner.write("Plan.md", "plan\n").unwrap();
    sa.inner.write("Ideas.md", "ideas\n").unwrap();
    a.sync(&mut sa, &server, TODAY).unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    sa.stuck.insert("Plan.md".into());
    let r = a.sync(&mut sa, &server, TODAY).unwrap();
    assert_eq!(r.pushed, 0);
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.read("Plan.md").unwrap(), "plan\n");
    // Gone for real: the delete goes.
    sa.stuck.clear();
    sa.inner.remove("Plan.md").unwrap();
    a.sync(&mut sa, &server, TODAY).unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.list().unwrap(), ["Ideas.md"]);
}

/// A folder that is suddenly (nearly) empty, like a drive's mount point
/// with the drive gone, sends nothing until the person says the deletes
/// are meant; a few deletes never ask.
#[test]
fn deleting_most_of_a_vault_at_once_waits_for_the_person() {
    let (server, mut a, mut b) = pair();
    let (mut sa, mut sb) = (MemoryStore::default(), MemoryStore::default());
    for i in 0..30 {
        sa.write(&format!("Notes/{i:02}.md"), &format!("note {i}\n")).unwrap();
    }
    a.sync(&mut sa, &server, TODAY).unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    // Ten at once: fine.
    for i in 0..10 {
        sa.remove(&format!("Notes/{i:02}.md")).unwrap();
    }
    a.sync(&mut sa, &server, TODAY).unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.list().unwrap().len(), 20);
    // The folder looks empty: twenty of twenty would go.
    let before = sa.clone();
    sa.0.clear();
    sa.write("New.md", "typed meanwhile\n").unwrap();
    let err = a.sync(&mut sa, &server, TODAY).unwrap_err();
    assert!(matches!(err, quire_sync::SyncError::ManyDeletes { count: 20, of: 20 }), "{err}");
    assert!(err.to_string().starts_with("20 of the 20 notes"), "{err}");
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.list().unwrap().len(), 20, "nothing deleted on the other device");
    assert_eq!(sa.list().unwrap(), ["New.md"], "nothing written here either");
    // The folder comes back: everything goes on as before.
    let mut back = before;
    back.write("New.md", "typed meanwhile\n").unwrap();
    let mut sa2 = back;
    a.sync(&mut sa2, &server, TODAY).unwrap();
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.list().unwrap().len(), 21);
    // Deleted on purpose, and said so: sent.
    sa2.0.retain(|p, _| p == "New.md");
    assert!(a.sync(&mut sa2, &server, TODAY).is_err());
    a.allow_many_deletes = true;
    let r = a.sync(&mut sa2, &server, TODAY).unwrap();
    assert_eq!(r.pushed, 20);
    b.sync(&mut sb, &server, TODAY).unwrap();
    assert_eq!(sb.list().unwrap(), ["New.md"]);
}

/// The receiving side: deletes are only ever applied from a delete another
/// device sent. A server that lost everything, or can't be reached,
/// deletes nothing here.
#[test]
fn an_empty_or_unreachable_server_deletes_nothing_here() {
    let (server, mut a, _) = pair();
    let mut sa = MemoryStore::default();
    sa.write("Plan.md", "plan\n").unwrap();
    a.sync(&mut sa, &server, TODAY).unwrap();
    server.set_offline(true);
    assert!(a.sync(&mut sa, &server, TODAY).is_err());
    assert_eq!(sa.list().unwrap(), ["Plan.md"]);
    // A new, empty server (restored from nothing) under the same state.
    let empty = MemoryServer::new();
    let _ = a.sync(&mut sa, &empty, TODAY);
    assert_eq!(sa.read("Plan.md").unwrap(), "plan\n");
}

// ---- 0.13.2: names this computer can't store ----

/// A disk like Windows': names it refuses fail to write, and names that
/// differ only in capitals are one file.
#[derive(Debug, Default)]
struct WindowsLike(MemoryStore);

impl WindowsLike {
    fn key(&self, path: &str) -> Option<String> {
        self.0.0.keys().find(|p| quire_sync::names::same_ignoring_case(p, path)).cloned()
    }
}

impl LocalStore for WindowsLike {
    fn list(&self) -> Result<Vec<String>, String> {
        self.0.list()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.key(path).and_then(|k| self.0.read(&k))
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        if quire_sync::names::windows_problem(path).is_some() {
            return Err("The filename, directory name, or volume label syntax is incorrect. (os error 123)".into());
        }
        let k = self.key(path).unwrap_or_else(|| path.to_string());
        self.0.write(&k, text)
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        match self.key(path) {
            Some(k) => self.0.remove(&k),
            None => Ok(()),
        }
    }
    fn unstorable(&self, path: &str, own: Option<&str>) -> Option<String> {
        if let Some(why) = quire_sync::names::windows_problem(path) {
            return Some(why);
        }
        if own.is_some_and(|o| quire_sync::names::same_ignoring_case(o, path)) {
            return None;
        }
        self.key(path).filter(|k| k != path).map(|k| format!("“{k}” is already here"))
    }
}

/// Reliability review #3: one note whose name this computer can't store
/// never stops the rest, in either direction.
#[test]
fn a_note_this_computer_cant_store_is_skipped_and_the_rest_syncs() {
    let (server, mut l, mut w) = pair();
    let (mut sl, mut sw) = (MemoryStore::default(), WindowsLike::default());
    sl.write("ok.md", "ok\n").unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    w.sync(&mut sw, &server, TODAY).unwrap();
    // Made on Linux: names Windows refuses, and two that differ in capitals.
    sl.write("10:30 meeting.md", "meeting\n").unwrap();
    sl.write("aux.md", "aux\n").unwrap();
    sl.write("Plan.md", "big plan\n").unwrap();
    sl.write("plan.md", "small plan\n").unwrap();
    sl.write("fine.md", "fine\n").unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    sw.0.write("ok.md", "ok, edited on windows\n").unwrap();
    let r = w.sync(&mut sw, &server, TODAY).unwrap();
    let mut skipped: Vec<&str> = r.skipped.iter().map(|(p, _)| p.as_str()).collect();
    skipped.sort_unstable();
    assert_eq!(skipped, ["10:30 meeting.md", "aux.md", "plan.md"], "{r:?}");
    assert!(r.skipped.iter().any(|(_, why)| why.contains("“:”")), "{r:?}");
    assert_eq!(sw.0.list().unwrap(), ["Plan.md", "fine.md", "ok.md"]);
    assert_eq!(r.pushed, 1, "this device's own edit still goes out");
    // Told once.
    let r = w.sync(&mut sw, &server, TODAY).unwrap();
    assert!(r.skipped.is_empty() && !r.changed_anything(), "{r:?}");
    // Nothing deleted or undone on the other device.
    l.sync(&mut sl, &server, TODAY).unwrap();
    assert_eq!(sl.list().unwrap(), ["10:30 meeting.md", "Plan.md", "aux.md", "fine.md", "ok.md", "plan.md"]);
    assert_eq!(sl.read("ok.md").unwrap(), "ok, edited on windows\n");
    // Renamed there to a name Windows takes: it arrives.
    let t = sl.0.remove("10:30 meeting.md").unwrap();
    sl.write("10.30 meeting.md", &t).unwrap();
    // The note in the way goes: the other one can be stored now.
    sl.remove("Plan.md").unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    let r = w.sync(&mut sw, &server, TODAY).unwrap();
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(sw.0.list().unwrap(), ["10.30 meeting.md", "fine.md", "ok.md", "plan.md"]);
    assert_eq!(sw.0.read("plan.md").unwrap(), "small plan\n");
    for _ in 0..3 {
        l.sync(&mut sl, &server, TODAY).unwrap();
        w.sync(&mut sw, &server, TODAY).unwrap();
    }
    assert_eq!(sl.list().unwrap(), ["10.30 meeting.md", "aux.md", "fine.md", "ok.md", "plan.md"]);
    assert_eq!(w.state.skipped.len(), 1, "aux.md stays skipped here, and is remembered");
}

/// A note renamed elsewhere to a name this computer can't store stays here
/// under its old name, untouched, and isn't renamed back for everyone.
#[test]
fn a_rename_to_a_name_this_computer_cant_store_is_not_undone() {
    let (server, mut l, mut w) = pair();
    let (mut sl, mut sw) = (MemoryStore::default(), WindowsLike::default());
    sl.write("Plan.md", "plan\n").unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    w.sync(&mut sw, &server, TODAY).unwrap();
    let t = sl.0.remove("Plan.md").unwrap();
    sl.write("Plan?.md", &t).unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    let r = w.sync(&mut sw, &server, TODAY).unwrap();
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(sw.0.list().unwrap(), ["Plan.md"], "kept where it was");
    for _ in 0..2 {
        w.sync(&mut sw, &server, TODAY).unwrap();
        l.sync(&mut sl, &server, TODAY).unwrap();
    }
    assert_eq!(sl.list().unwrap(), ["Plan?.md"], "not renamed back, not deleted");
    // Edited there: still skipped here, still nothing lost there.
    sl.write("Plan?.md", "plan, more\n").unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    let r = w.sync(&mut sw, &server, TODAY).unwrap();
    assert!(r.skipped.is_empty(), "same name, same reason: told once");
    // Renamed again to a name Windows takes: it moves here, with the edit.
    let t = sl.0.remove("Plan?.md").unwrap();
    sl.write("Plan, maybe.md", &t).unwrap();
    l.sync(&mut sl, &server, TODAY).unwrap();
    w.sync(&mut sw, &server, TODAY).unwrap();
    assert_eq!(sw.0.list().unwrap(), ["Plan, maybe.md"]);
    assert_eq!(sw.0.read("Plan, maybe.md").unwrap(), "plan, more\n");
    assert!(w.state.skipped.is_empty());
}

/// A conflict copy is named after the other device: a name with characters
/// Windows refuses still gives a name every device can store.
#[test]
fn conflict_copies_get_names_every_system_can_store() {
    let (server, mut devs) = devices(2);
    devs[0].engine.device_name = "Asha's PC: \"work\"?".into();
    put(&mut devs[0], "Plan.md", "# Plan\nShip it.\n");
    settle(&mut devs, &server);
    put(&mut devs[0], "Plan.md", "# Plan\nShip it on Friday.\n");
    put(&mut devs[1], "Plan.md", "# Plan\nShip it to everyone.\n");
    sync(&mut devs[0], &server);
    let r = sync(&mut devs[1], &server);
    assert_eq!(r.conflict_copies.len(), 1);
    assert_eq!(quire_sync::names::windows_problem(&r.conflict_copies[0]), None, "{:?}", r.conflict_copies);
}
