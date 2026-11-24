// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Batches and chunked passes (`docs/design/0.14-sync-speed.md` §3.4, §3.6):
//! a first sync goes in batches and in passes of a chunk each, deletes are
//! never split across passes, the cursor skips this device's own
//! positions only where the server numbers every revision, and a batch
//! whose reply is lost is sent again without duplicates.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds fixtures with std directly"
)]

use std::sync::atomic::{AtomicBool, Ordering};

use quire_sync::folder::{self, FolderTransport};
use quire_sync::{
    BatchReply, DeviceId, Engine, FileId, KdfParams, LocalStore, MemoryServer, MemoryStore, PushOutcome, Revision, SpaceId, SyncError,
    SyncState, Transport, TransportError, VaultKey, WrappedKey,
};

const TODAY: &str = "2026-11-20";

struct Device {
    engine: Engine,
    store: MemoryStore,
}

fn devices(n: usize, chunk: usize) -> (MemoryServer, Vec<Device>) {
    let server = MemoryServer::new();
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    let devs = (0..n)
        .map(|i| Device {
            engine: Engine::new(space.clone(), DeviceId::random().unwrap(), format!("device {i}"), key.keys(), SyncState::default())
                .with_chunk(chunk),
            store: MemoryStore::default(),
        })
        .collect();
    (server, devs)
}

fn sync(d: &mut Device, server: &dyn Transport) -> quire_sync::Report {
    d.engine.sync(&mut d.store, server, TODAY).unwrap()
}

/// Passes until one leaves nothing for the next.
fn sync_all(d: &mut Device, server: &dyn Transport) -> usize {
    let mut passes = 0;
    loop {
        passes += 1;
        let r = sync(d, server);
        if !r.more_to_send() {
            return passes;
        }
        assert!(passes < 100, "never done");
    }
}

#[test]
fn a_first_sync_goes_in_batches_and_chunked_passes() {
    let (server, mut devs) = devices(2, 50);
    for i in 0..120 {
        devs[0].store.write(&format!("Note {i:03}.md"), &format!("note {i}\n")).unwrap();
    }
    let r = sync(&mut devs[0], &server);
    assert_eq!((r.pushed, r.left), (50, 70), "a chunk, and what is left for the next pass");
    assert_eq!(server.requests(), 1, "one batch");
    let r = sync(&mut devs[0], &server);
    assert_eq!((r.pushed, r.left), (50, 20));
    let r = sync(&mut devs[0], &server);
    assert_eq!((r.pushed, r.left), (20, 0));
    assert_eq!(server.requests(), 3);
    assert_eq!(server.len(), 120);
    sync_all(&mut devs[1], &server);
    assert_eq!(devs[1].store, devs[0].store);
}

#[test]
fn a_batch_holds_at_most_a_thousand_revisions() {
    let (server, mut devs) = devices(1, 5000);
    for i in 0..2500 {
        devs[0].store.write(&format!("{i}.md"), "x\n").unwrap();
    }
    let r = sync(&mut devs[0], &server);
    assert_eq!(r.pushed, 2500);
    assert_eq!(server.requests(), 3, "⌈2500 / 1000⌉ requests");
}

#[test]
fn a_server_without_batches_takes_one_revision_a_request() {
    // A server before 0.14, through a transport that can't batch.
    let (server, mut devs) = devices(2, 5000);
    server.set_batches(false);
    for i in 0..30 {
        devs[0].store.write(&format!("{i}.md"), "x\n").unwrap();
    }
    sync(&mut devs[0], &server);
    assert_eq!(server.requests(), 30);
    sync(&mut devs[1], &server);
    assert_eq!(devs[1].store, devs[0].store);
}

/// Forty synced notes on two devices.
fn forty(chunk: usize) -> (MemoryServer, Vec<Device>) {
    let (server, mut devs) = devices(2, chunk);
    for i in 0..40 {
        devs[0].store.write(&format!("{i:02}.md"), &format!("note {i}\n")).unwrap();
    }
    sync_all(&mut devs[0], &server);
    sync_all(&mut devs[1], &server);
    (server, devs)
}

#[test]
fn deletes_are_never_split_across_chunked_passes() {
    let (server, mut devs) = forty(5);
    for i in 0..8 {
        devs[0].store.0.remove(&format!("{i:02}.md"));
    }
    for i in 20..32 {
        devs[0].store.write(&format!("{i:02}.md"), &format!("note {i}, edited\n")).unwrap();
    }
    let r = sync(&mut devs[0], &server);
    assert_eq!((r.pushed, r.left), (8 + 5, 7), "every delete, then a chunk of edits");
    let gone = devs[0].engine.state.files.values().filter(|t| t.deleted).count();
    assert_eq!(gone, 8);
    sync_all(&mut devs[0], &server);
    sync_all(&mut devs[1], &server);
    assert_eq!(devs[1].store, devs[0].store);
}

#[test]
fn an_approved_set_larger_than_a_chunk_is_sent_in_one_pass() {
    let (server, mut devs) = forty(5);
    for i in 0..30 {
        devs[0].store.0.remove(&format!("{i:02}.md"));
    }
    let d = &mut devs[0];
    let err = d.engine.sync(&mut d.store, &server, TODAY).unwrap_err();
    let SyncError::ManyDeletes { notes, .. } = err else { panic!("{err}") };
    assert_eq!(notes.len(), 30, "the whole set is checked before anything is sent");
    d.engine.allowed_deletes = notes.into_iter().map(|(id, _)| id).collect();
    let r = d.engine.sync(&mut d.store, &server, TODAY).unwrap();
    assert_eq!((r.pushed, r.left), (30, 0), "all of them in one pass, whatever the chunk");
    sync_all(&mut devs[1], &server);
    assert_eq!(devs[1].store.0.len(), 10);
}

#[test]
fn the_cursor_passes_this_devices_own_positions_on_a_server() {
    let (server, mut devs) = devices(2, 5000);
    for i in 0..10 {
        devs[0].store.write(&format!("{i}.md"), "x\n").unwrap();
    }
    sync(&mut devs[0], &server);
    assert_eq!(devs[0].engine.state.cursor, 10, "its own revisions aren't fetched again");
    // Another device's revision in between: the cursor stops before it.
    devs[1].store.write("B.md", "b\n").unwrap();
    sync(&mut devs[1], &server);
    devs[0].store.write("A.md", "a\n").unwrap();
    let before = devs[0].engine.state.cursor;
    sync(&mut devs[0], &server);
    assert!(devs[0].engine.state.cursor > before);
    assert_eq!(devs[0].store.read("B.md").as_deref(), Some("b\n"), "the other device's revision arrived");
    assert_eq!(devs[0].engine.state.cursor, server.len() as u64);
}

#[test]
fn a_sync_folder_never_moves_the_cursor_past_a_pull() {
    let dir = tempfile::tempdir().unwrap();
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(dir.path(), &space, &WrappedKey::wrap(&key, "pw", KdfParams::TEST).unwrap()).unwrap();
    let id = DeviceId::random().unwrap();
    let t = FolderTransport::new(dir.path(), &space, id.clone(), None);
    let mut d = Device { engine: Engine::new(space, id, "d", key.keys(), SyncState::default()), store: MemoryStore::default() };
    for i in 0..10 {
        d.store.write(&format!("{i}.md"), "x\n").unwrap();
    }
    sync(&mut d, &t);
    let cursor = d.engine.state.cursor;
    assert!(!t.pull(cursor).unwrap().is_empty(), "its own lines come back once, past the cursor");
}

/// A transport whose next batch reply is lost: the batch is stored, the
/// device hears nothing back (a cut connection).
struct LosesReply<'a> {
    inner: &'a MemoryServer,
    lose: AtomicBool,
}

impl Transport for LosesReply<'_> {
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        self.inner.pull(after)
    }
    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
        self.inner.push(rev)
    }
    fn push_batch(&self, revs: &[Revision]) -> BatchReply {
        let reply = self.inner.push_batch(revs);
        if self.lose.swap(false, Ordering::Relaxed) {
            return BatchReply { outcomes: Vec::new(), error: Some(TransportError::Offline("connection reset".into())) };
        }
        reply
    }
    fn own_seqs_skippable(&self) -> bool {
        true
    }
    fn history(&self, file: &FileId) -> Result<Vec<Revision>, TransportError> {
        self.inner.history(file)
    }
}

#[test]
fn a_batch_whose_reply_was_lost_leaves_no_duplicate() {
    let (server, mut devs) = forty(5000);
    let t = LosesReply { inner: &server, lose: AtomicBool::new(true) };
    for i in 0..10 {
        devs[0].store.write(&format!("{i:02}.md"), &format!("note {i}, edited\n")).unwrap();
        devs[0].store.write(&format!("New {i}.md"), &format!("new {i}\n")).unwrap();
    }
    let d = &mut devs[0];
    assert!(d.engine.sync(&mut d.store, &t, TODAY).is_err());
    let r = sync(&mut devs[0], &t);
    assert!(r.conflict_copies.is_empty(), "{r:?}");
    sync_all(&mut devs[1], &server);
    assert_eq!(devs[1].store, devs[0].store);
    assert_eq!(devs[0].store.0.len(), 50, "no note twice");
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

/// Three devices editing, creating and deleting, with passes in between
/// that send in small chunks and batches: every device ends with the same
/// notes and every revision is applied everywhere.
fn simulate(seed: u64, server: &dyn Transport, devs: &mut [Device]) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for step in 0..80 {
        let i = rng.below(devs.len());
        match rng.below(4) {
            0 => {
                let d = &mut devs[i];
                // A pass may stop between batches or chunks: what it sent stands.
                let _ = d.engine.sync(&mut d.store, server, TODAY);
            }
            1 if !devs[i].store.0.is_empty() && rng.below(4) == 0 => {
                let names: Vec<String> = devs[i].store.0.keys().cloned().collect();
                let p = &names[rng.below(names.len())];
                devs[i].store.0.remove(p);
            }
            _ => {
                let p = format!("N{}.md", rng.below(12));
                let w = format!("w{step}");
                let mut lines: Vec<String> = devs[i].store.read(&p).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
                let at = rng.below(lines.len() + 1);
                lines.insert(at, w.clone());
                devs[i].store.write(&p, &(lines.join("\n") + "\n")).unwrap();
            }
        }
    }
    for _ in 0..10 {
        let mut changed = false;
        for d in devs.iter_mut() {
            let r = d.engine.sync(&mut d.store, server, TODAY).unwrap();
            changed |= r.changed_anything() || r.more_to_send();
        }
        if !changed {
            break;
        }
    }
    for d in &devs[1..] {
        assert_eq!(d.store, devs[0].store, "seed {seed}: devices disagree");
    }
}

#[test]
fn three_devices_pushing_in_chunks_and_batches_converge() {
    for seed in seeds() {
        let (server, mut devs) = devices(3, 2);
        simulate(seed, &server, &mut devs);
        assert_eq!(devs[0].engine.state.cursor, server.len() as u64, "seed {seed}: every revision applied");
    }
}

/// A vault whose files under `Broken/` are listed but can't be read.
struct Unreadable(MemoryStore);

impl LocalStore for Unreadable {
    fn list(&self) -> Result<Vec<String>, String> {
        self.0.list()
    }
    fn read(&self, path: &str) -> Option<String> {
        if path.starts_with("Broken/") { None } else { self.0.read(path) }
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        self.0.write(path, text)
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.0.remove(path)
    }
}

#[test]
fn new_files_that_cant_be_read_take_no_room_and_start_no_pass() {
    let (server, mut devs) = devices(1, quire_sync::engine::CHUNK);
    let mut store = Unreadable(MemoryStore::default());
    for i in 0..5001 {
        store.0.write(&format!("Broken/{i:04}.md"), "x\n").unwrap();
    }
    for i in 0..3 {
        store.0.write(&format!("Readable/{i}.md"), "y\n").unwrap();
    }
    let d = &mut devs[0];
    let r = d.engine.sync(&mut store, &server, TODAY).unwrap();
    assert_eq!(r.pushed, 3, "the readable ones go, behind 5,001 that can't be read");
    assert!(!r.more_to_send(), "{r:?}");
    let r = d.engine.sync(&mut store, &server, TODAY).unwrap();
    assert_eq!(r.pushed, 0);
    assert!(!r.more_to_send(), "a pass that sent nothing starts no other at once");
    assert_eq!(server.len(), 3);
    // Left work but sent nothing: the app and `sync once` wait for the poll.
    let stuck = quire_sync::Report { left: 5000, ..quire_sync::Report::default() };
    assert!(!stuck.more_to_send());
}

#[test]
fn a_resend_is_the_same_revision_only() {
    let (server, mut devs) = devices(1, 50);
    devs[0].store.write("A.md", "a\n").unwrap();
    sync(&mut devs[0], &server);
    let stored = server.pull(0).unwrap().remove(0);
    let reply = server.push_batch(std::slice::from_ref(&stored));
    assert!(matches!(reply.outcomes[..], [quire_sync::BatchOutcome::Accepted { seq: 1 }]), "a resend: {:?}", reply.outcomes);
    let mut forged = stored.clone();
    forged.sealed[1] ^= 1;
    let mut other = stored;
    other.device = DeviceId::random().unwrap();
    let reply = server.push_batch(&[forged, other]);
    assert!(matches!(reply.outcomes[..], [quire_sync::BatchOutcome::Conflict, quire_sync::BatchOutcome::Conflict]), "{:?}", reply.outcomes);
    assert_eq!(server.len(), 1);
}

/// A transport whose sealing changes once the first batch went (the vault
/// upgraded meanwhile), and that can answer short.
struct Switching<'a> {
    inner: &'a MemoryServer,
    sent: AtomicBool,
    short: bool,
}

impl Transport for Switching<'_> {
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        self.inner.pull(after)
    }
    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
        self.inner.push(rev)
    }
    fn push_batch(&self, revs: &[Revision]) -> BatchReply {
        self.sent.store(true, Ordering::Relaxed);
        let mut reply = self.inner.push_batch(revs);
        if self.short {
            reply.outcomes.pop();
        }
        reply
    }
    fn sealing(&self) -> quire_sync::Sealing {
        if self.sent.load(Ordering::Relaxed) { quire_sync::Sealing::Epoch } else { quire_sync::Sealing::Plain }
    }
}

#[test]
fn a_revision_left_for_the_next_batch_is_sealed_as_that_batch() {
    let (server, mut devs) = devices(1, 50);
    let t = Switching { inner: &server, sent: AtomicBool::new(false), short: false };
    // About 3 MiB of request each: one a batch, the next one built and
    // left waiting.
    for i in 0..3 {
        devs[0].store.write(&format!("Big {i}.md"), &format!("{i}{}", "x".repeat(1536 * 1024))).unwrap();
    }
    let d = &mut devs[0];
    d.engine.sync(&mut d.store, &t, TODAY).unwrap();
    let revs = server.pull(0).unwrap();
    assert_eq!(revs.len(), 3);
    assert_eq!(revs[0].sealed[0], quire_sync::crypto::SEALED_V1);
    for r in &revs[1..] {
        assert_eq!(r.sealed[0], quire_sync::crypto::SEALED_V2, "sealed as the batch it went in");
    }
}

#[test]
fn a_reply_shorter_than_its_batch_is_an_error() {
    let (server, mut devs) = devices(1, 50);
    let t = Switching { inner: &server, sent: AtomicBool::new(false), short: true };
    for i in 0..5 {
        devs[0].store.write(&format!("N{i}.md"), "n\n").unwrap();
    }
    let d = &mut devs[0];
    assert!(d.engine.sync(&mut d.store, &t, TODAY).is_err());
}

#[test]
fn edits_to_more_notes_than_a_chunk_go_a_chunk_a_pass() {
    let (server, mut devs) = devices(2, 3);
    for i in 0..10 {
        devs[0].store.write(&format!("N{i}.md"), "one\n").unwrap();
    }
    sync_all(&mut devs[0], &server);
    for i in 0..10 {
        devs[0].store.write(&format!("N{i}.md"), &format!("two {i}\n")).unwrap();
    }
    let r = sync(&mut devs[0], &server);
    assert_eq!((r.pushed, r.left), (3, 7));
    // Edited again before its pass: the text read then is the one sent.
    devs[0].store.write("N9.md", "three\n").unwrap();
    assert_eq!(sync_all(&mut devs[0], &server), 3);
    sync_all(&mut devs[1], &server);
    assert_eq!(devs[1].store, devs[0].store);
    assert_eq!(devs[1].store.read("N9.md").as_deref(), Some("three\n"));
}
