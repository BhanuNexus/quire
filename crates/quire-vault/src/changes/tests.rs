// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::{LocalFs, MemFs};

fn np(s: &str) -> NotePath {
    NotePath::new(s)
}

const SEC: u64 = 1_000_000_000;

/// A storage whose file times come from a clock the test moves (P10): each
/// write takes the clock's time, as a disk does.
#[derive(Default)]
struct Fake {
    files: Mutex<BTreeMap<NotePath, (Vec<u8>, u64)>>,
    now: Arc<AtomicU64>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let f = Self::default();
        f.now.store(1_000 * SEC, Ordering::SeqCst);
        Arc::new(f)
    }

    fn clock(&self) -> Clock {
        let now = Arc::clone(&self.now);
        Arc::new(move || UNIX_EPOCH + Duration::from_nanos(now.load(Ordering::SeqCst)))
    }

    fn tick(&self, ns: u64) {
        self.now.fetch_add(ns, Ordering::SeqCst);
    }

    /// A write, at the clock's time plus a nanosecond: two writes never
    /// share a time, as on a disk with a fine clock. (On a coarse one, a
    /// rename over a deleted file of the same size and time is found only
    /// by verify; `docs/formats/manifest.md` says so.)
    fn put(&self, p: &str, bytes: &[u8]) {
        let t = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.files.lock().unwrap().insert(np(p), (bytes.to_vec(), t));
    }

    /// New bytes, the old time kept (a tool that restores times).
    fn put_keeping_time(&self, p: &str, bytes: &[u8]) {
        let mut files = self.files.lock().unwrap();
        let t = files.get(&np(p)).map_or(0, |(_, t)| *t);
        files.insert(np(p), (bytes.to_vec(), t));
    }

    fn del(&self, p: &NotePath) {
        self.files.lock().unwrap().remove(p);
    }

    fn paths(&self) -> Vec<NotePath> {
        self.files.lock().unwrap().keys().cloned().collect()
    }
}

impl Storage for Fake {
    fn list(&self) -> Result<Vec<NotePath>> {
        Ok(self.paths().into_iter().filter(|p| !crate::storage::is_hidden(p.as_str())).collect())
    }
    fn list_stat(&self) -> Result<Vec<(NotePath, Option<FileStat>)>> {
        let files = self.files.lock().unwrap();
        Ok(files
            .iter()
            .filter(|(p, _)| !crate::storage::is_hidden(p.as_str()))
            .map(|(p, (b, t))| (p.clone(), Some(FileStat { size: b.len() as u64, mtime_ns: *t })))
            .collect())
    }
    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        self.files.lock().unwrap().get(path).map(|(b, _)| b.clone()).ok_or_else(|| VaultError::NotFound(path.to_string()))
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> Result<()> {
        self.put(path.as_str(), bytes);
        Ok(())
    }
    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        let v = files.remove(from).ok_or_else(|| VaultError::NotFound(from.to_string()))?;
        files.insert(to.clone(), v);
        Ok(())
    }
    fn trash(&self, path: &NotePath) -> Result<()> {
        self.remove(path)
    }
    fn exists(&self, path: &NotePath) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
    fn modified(&self, path: &NotePath) -> Result<SystemTime> {
        let t = self.files.lock().unwrap().get(path).map(|(_, t)| *t).ok_or_else(|| VaultError::NotFound(path.to_string()))?;
        Ok(UNIX_EPOCH + Duration::from_nanos(t))
    }
    fn absolute(&self, _: &NotePath) -> Option<PathBuf> {
        None
    }
    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        Ok(self.paths().into_iter().filter(|p| p.folder() == folder).collect())
    }
    fn remove(&self, path: &NotePath) -> Result<()> {
        self.files.lock().unwrap().remove(path).map(|_| ()).ok_or_else(|| VaultError::NotFound(path.to_string()))
    }
}

/// What a [`Probe`] waits on: reads of `path` stop until `open` is set.
type Gate = (NotePath, Arc<AtomicBool>, Arc<AtomicBool>);

/// Another storage, with its reads counted (the "files opened" budget) and
/// faults on demand: a failing listing, a failing read, a read that waits.
struct Probe {
    inner: Arc<dyn Storage>,
    reads: AtomicUsize,
    fail_list: AtomicBool,
    fail_read: Mutex<Option<NotePath>>,
    /// (path, reached, open): a read of `path` sets `reached` and waits for `open`.
    gate: Mutex<Option<Gate>>,
    /// (reached, open): a listing, once made, sets `reached` and waits for `open`.
    list_gate: Mutex<Option<(Arc<AtomicBool>, Arc<AtomicBool>)>>,
}

impl Probe {
    fn new(inner: Arc<dyn Storage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reads: AtomicUsize::new(0),
            fail_list: AtomicBool::new(false),
            fail_read: Mutex::new(None),
            gate: Mutex::new(None),
            list_gate: Mutex::new(None),
        })
    }

    /// Reads since the last call.
    fn reads(&self) -> usize {
        self.reads.swap(0, Ordering::SeqCst)
    }
}

impl Storage for Probe {
    fn list(&self) -> Result<Vec<NotePath>> {
        self.inner.list()
    }
    fn list_stat(&self) -> Result<Vec<(NotePath, Option<FileStat>)>> {
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(VaultError::Io { path: "Projects".into(), source: std::io::Error::other("the folder kept moving") });
        }
        let listing = self.inner.list_stat();
        let gate = self.list_gate.lock().unwrap().clone();
        if let Some((reached, open)) = gate {
            reached.store(true, Ordering::SeqCst);
            while !open.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
        listing
    }
    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_read.lock().unwrap().as_ref() == Some(path) {
            return Err(VaultError::Io { path: path.to_string(), source: std::io::Error::other("permission denied") });
        }
        let gate = self.gate.lock().unwrap().clone();
        if let Some((p, reached, open)) = gate
            && p == *path
        {
            reached.store(true, Ordering::SeqCst);
            while !open.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        }
        self.inner.read_bytes(path)
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> Result<()> {
        self.inner.write_bytes_atomic(path, bytes)
    }
    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn trash(&self, path: &NotePath) -> Result<()> {
        self.inner.trash(path)
    }
    fn exists(&self, path: &NotePath) -> bool {
        self.inner.exists(path)
    }
    fn modified(&self, path: &NotePath) -> Result<SystemTime> {
        self.inner.modified(path)
    }
    fn absolute(&self, path: &NotePath) -> Option<PathBuf> {
        self.inner.absolute(path)
    }
    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        self.inner.list_under(folder)
    }
    fn remove(&self, path: &NotePath) -> Result<()> {
        self.inner.remove(path)
    }
}

fn vault_of(s: &Arc<impl Storage + 'static>) -> Vault {
    Vault::new(Arc::clone(s) as Arc<dyn Storage>)
}

/// Path → (size, hash) of the files there now.
fn summary(m: &Manifest) -> BTreeMap<NotePath, (u64, FileHash)> {
    m.rows().into_iter().map(|r| (r.path, (r.size, r.hash))).collect()
}

/// What a manifest built from nothing over the same storage says.
fn cold(storage: Arc<dyn Storage>) -> BTreeMap<NotePath, (u64, FileHash)> {
    let m = Manifest::open(&Vault::new(storage));
    m.verify().unwrap();
    summary(&m)
}

/// Far ahead of every file time, so nothing on a real disk counts as racy.
fn later() -> Clock {
    Arc::new(|| SystemTime::now() + Duration::from_hours(1))
}

#[test]
fn a_scan_reads_only_what_changed() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    fs.put("a.md", b"alpha");
    fs.put("Projects/b.md", b"beta");
    fs.put(".quire/hidden.json", b"{}");
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    assert!(!m.scanned());
    fs.tick(10 * SEC);
    let d = m.scan().unwrap();
    assert_eq!(d.changed.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["Projects/b.md", "a.md"]);
    assert!(d.removed.is_empty() && !d.verified && m.scanned());
    assert_eq!(probe.reads(), 2);
    assert_eq!(m.get(&np("a.md")).unwrap().hash, FileHash::of(b"alpha"));
    assert!(!m.has(&np(".quire/hidden.json")), "hidden files are not in the manifest");
    // Nothing changed: no file is opened.
    let g = m.generation();
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0);
    assert_eq!(m.generation(), g);
    // An edit, a new file and a delete.
    fs.put("a.md", b"alpha two");
    fs.put("c.md", b"gamma");
    fs.del(&np("Projects/b.md"));
    fs.tick(10 * SEC);
    let d = m.scan().unwrap();
    assert_eq!(probe.reads(), 2);
    assert_eq!(d.changed.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["a.md", "c.md"]);
    assert_eq!(d.removed, [np("Projects/b.md")]);
    assert_eq!(d.generation, g + 1);
    assert!(d.changed.iter().all(|r| r.generation == g + 1));
    assert!(!m.has(&np("Projects/b.md")));
    let since = m.since(g).unwrap();
    assert_eq!(since.len(), 3);
    assert!(since.iter().find(|r| r.path.as_str() == "Projects/b.md").unwrap().gone);
    assert_eq!(m.since(m.generation()).unwrap(), []);
    // A file touched but not changed is read once, and is no change.
    fs.put("c.md", b"gamma");
    fs.tick(10 * SEC);
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 1);
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0);
}

#[test]
fn an_edit_in_the_same_tick_is_found_by_the_next_scan_and_verify_finds_one_that_kept_size_and_time() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    fs.put("a.md", b"aaaa");
    // Scanned in the tick it was written: racy.
    assert_eq!(m.scan().unwrap().changed.len(), 1);
    assert!(m.get(&np("a.md")).unwrap().is_racy());
    fs.put_keeping_time("a.md", b"bbbb");
    let d = m.scan().unwrap();
    assert_eq!(d.changed.len(), 1, "same size, same time, found because the row was racy");
    assert_eq!(d.changed[0].hash, FileHash::of(b"bbbb"));
    // Within the window it stays racy; past it the row is trusted.
    fs.tick(SEC);
    assert!(m.scan().unwrap().is_empty());
    assert!(m.get(&np("a.md")).unwrap().is_racy());
    fs.tick(5 * SEC);
    assert!(m.scan().unwrap().is_empty());
    assert!(!m.get(&np("a.md")).unwrap().is_racy());
    probe.reads();
    // Now an edit that keeps size and time is not seen by a scan...
    fs.put_keeping_time("a.md", b"cccc");
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0);
    // ...but by verify, which reads every file.
    fs.put("other.md", b"x");
    fs.tick(10 * SEC);
    let d = m.verify().unwrap();
    assert!(d.verified);
    assert_eq!(probe.reads(), 2);
    assert_eq!(
        d.changed.iter().map(|r| (r.path.as_str(), r.hash)).collect::<Vec<_>>(),
        [("a.md", FileHash::of(b"cccc")), ("other.md", FileHash::of(b"x"))]
    );
}

#[test]
fn a_listing_error_is_an_error_and_changes_no_row() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    fs.put("a.md", b"a");
    fs.put("b.md", b"b");
    fs.tick(10 * SEC);
    m.scan().unwrap();
    let (before, generation) = (m.since(0).unwrap(), m.generation());
    let rx = m.subscribe();
    fs.del(&np("a.md"));
    fs.put("b.md", b"b2");
    fs.put("c.md", b"c");
    fs.tick(10 * SEC);
    probe.fail_list.store(true, Ordering::SeqCst);
    assert!(m.scan().is_err());
    assert!(m.verify().is_err());
    assert_eq!((m.since(0).unwrap(), m.generation()), (before, generation), "never \"all gone\"");
    assert!(rx.try_recv().is_err(), "subscribers hear nothing");
    probe.fail_list.store(false, Ordering::SeqCst);
    let d = m.scan().unwrap();
    assert_eq!((d.changed.len(), d.removed.len()), (2, 1));
    assert_eq!(*rx.try_recv().unwrap(), d);
}

#[test]
fn one_unreadable_file_among_many_keeps_its_row_and_the_others_update() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    for i in 0..10 {
        fs.put(&format!("n{i}.md"), b"one");
    }
    fs.tick(10 * SEC);
    m.scan().unwrap();
    let rx = m.subscribe();
    let old = m.get(&np("n3.md")).unwrap();
    let changed = |d: &Delta| d.changed.iter().map(|r| r.path.to_string()).collect::<Vec<_>>();
    let fail = |p: Option<&str>| *probe.fail_read.lock().unwrap() = p.map(np);
    // Every note changes, and n3.md can't be read (a share lock, `chmod 000`).
    for i in 0..10 {
        fs.put(&format!("n{i}.md"), b"two!");
    }
    fs.put("new.md", b"new");
    fs.tick(10 * SEC);
    fail(Some("n3.md"));
    let d = m.scan().unwrap();
    let others: Vec<String> = (0..10).filter(|i| *i != 3).map(|i| format!("n{i}.md")).chain(["new.md".into()]).collect();
    assert_eq!(changed(&d), others, "the others update");
    assert!(d.removed.is_empty(), "nothing is tombstoned");
    assert_eq!(d.unread.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["n3.md"]);
    assert!(d.unread[0].1.contains("permission denied"), "{:?}", d.unread);
    assert_eq!(*rx.try_recv().unwrap(), d, "the others' changes are sent, with what was unread");
    let kept = m.get(&np("n3.md")).unwrap();
    assert_eq!((kept.hash, kept.generation), (old.hash, old.generation), "the old row stays");
    assert!(kept.is_racy(), "tried again by the next scan");
    // Readable again: found by the next scan. A new file that can't be
    // read has no row yet.
    fs.put("later.md", b"later");
    fs.tick(10 * SEC);
    fail(Some("later.md"));
    let d = m.scan().unwrap();
    assert_eq!(changed(&d), ["n3.md"]);
    assert_eq!(d.unread.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["later.md"]);
    assert!(!m.has(&np("later.md")) && d.removed.is_empty());
    assert_eq!(*rx.try_recv().unwrap(), d);
    // A scan whose only news is an unreadable file sends nothing.
    fail(Some("later.md"));
    let d = m.scan().unwrap();
    assert!(d.is_empty() && d.unread.len() == 1);
    assert!(rx.try_recv().is_err());
    fail(None);
    assert_eq!(changed(&m.scan().unwrap()), ["later.md"]);
    assert_eq!(summary(&m), cold(Arc::clone(&fs) as Arc<dyn Storage>));
}

#[test]
fn subscribers_get_each_scans_delta_and_a_dropped_one_is_forgotten() {
    let fs = Fake::new();
    let m = Manifest::open_with_clock(&vault_of(&fs), fs.clock());
    let (one, two) = (m.subscribe(), m.subscribe());
    fs.put("a.md", b"a");
    fs.tick(10 * SEC);
    let d = m.scan().unwrap();
    assert_eq!(*one.recv().unwrap(), d);
    assert_eq!(*two.recv().unwrap(), d);
    drop(two);
    assert!(m.scan().unwrap().is_empty());
    assert!(one.try_recv().is_err(), "an empty scan sends nothing");
    fs.put("b.md", b"b");
    fs.tick(10 * SEC);
    let d = m.scan().unwrap();
    assert_eq!(*one.recv().unwrap(), d);
    assert_eq!(lock(&m.shared.subscribers).len(), 1);
    // A clone shares the table and the subscribers.
    let c = m.clone();
    fs.put("c.md", b"c");
    fs.tick(10 * SEC);
    assert_eq!(c.scan().unwrap().changed.len(), 1);
    assert_eq!(one.recv().unwrap().changed[0].path, np("c.md"));
    assert!(m.has(&np("c.md")));
}

#[test]
fn own_writes_update_the_row_at_once_and_count_as_racy() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    fs.put("a.md", b"a");
    fs.tick(10 * SEC);
    m.scan().unwrap();
    let rx = m.subscribe();
    let g = m.generation();
    fs.put("new.md", b"new");
    assert_eq!(m.note_written(&np("new.md"), b"new"), FileHash::of(b"new"));
    assert!(m.has(&np("new.md")), "listed before anyone else hears of it");
    let row = m.get(&np("new.md")).unwrap();
    assert!(row.is_racy() && row.generation == g + 1 && row.size == 3);
    // The same bytes again: no new generation.
    m.note_written(&np("new.md"), b"new");
    assert_eq!(m.generation(), g + 1);
    fs.del(&np("a.md"));
    m.note_removed(&np("a.md"));
    assert!(!m.has(&np("a.md")));
    assert_eq!(m.generation(), g + 2);
    m.note_removed(&np("never-there.md"));
    assert_eq!(m.generation(), g + 2);
    // The next scan reads the written file again and finds nothing new.
    fs.tick(10 * SEC);
    probe.reads();
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 1);
    assert!(rx.try_recv().is_err(), "own writes are not sent to subscribers");
    assert!(!m.get(&np("new.md")).unwrap().is_racy());
}

#[test]
fn own_writes_never_wait_for_the_table() {
    let fs = Fake::new();
    let m = Manifest::open_with_clock(&vault_of(&fs), fs.clock());
    let g = m.generation();
    {
        // Something holds the table (as a flush encoding it would).
        let held = m.shared.table.read().unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                let t = Instant::now();
                m.note_written(&np("a.md"), b"a");
                m.note_written(&np("b.md"), b"b");
                m.note_removed(&np("b.md"));
                assert!(t.elapsed() < Duration::from_secs(1));
                // Answered from the queue.
                assert_eq!(m.get(&np("a.md")).unwrap().hash, FileHash::of(b"a"));
                assert!(!m.has(&np("b.md")));
            })
            .join()
            .unwrap();
        });
        assert_eq!(held.generation, g, "nothing applied while held");
        assert_eq!(lock(&m.shared.pending).len(), 3);
    }
    // Applied in order as soon as the table is free.
    assert_eq!(m.generation(), g + 3);
    assert!(lock(&m.shared.pending).is_empty());
    assert!(m.has(&np("a.md")) && !m.has(&np("b.md")));
    let since: Vec<(String, bool)> = m.since(g).unwrap().into_iter().map(|r| (r.path.to_string(), r.gone)).collect();
    assert_eq!(since, [("a.md".to_string(), false), ("b.md".to_string(), true)]);
}

#[test]
fn an_own_write_during_a_scan_wins_until_the_next_scan() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    fs.put("a.md", b"old");
    fs.put("b.md", b"b");
    fs.tick(10 * SEC);
    m.scan().unwrap();
    fs.put("a.md", b"edited elsewhere");
    fs.tick(10 * SEC);
    let (reached, open) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    *probe.gate.lock().unwrap() = Some((np("a.md"), Arc::clone(&reached), Arc::clone(&open)));
    std::thread::scope(|s| {
        let scan = s.spawn(|| m.scan().unwrap());
        while !reached.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // The scan is reading a.md; our save of it and of a new note land now.
        let t = Instant::now();
        fs.put("a.md", b"saved");
        m.note_written(&np("a.md"), b"saved");
        fs.put("c.md", b"c");
        m.note_written(&np("c.md"), b"c");
        assert!(t.elapsed() < Duration::from_secs(1), "never waits for the scan");
        open.store(true, Ordering::SeqCst);
        let d = scan.join().unwrap();
        assert!(d.changed.iter().all(|r| r.path.as_str() != "a.md"), "the scan's older read of a.md is not used");
        assert!(d.removed.is_empty(), "c.md was not in its listing, but is ours");
    });
    *probe.gate.lock().unwrap() = None;
    assert_eq!(m.get(&np("a.md")).unwrap().hash, FileHash::of(b"saved"));
    assert!(m.has(&np("c.md")));
    fs.tick(10 * SEC);
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(summary(&m), cold(Arc::clone(&fs) as Arc<dyn Storage>));
}

#[test]
fn a_note_saved_while_the_scan_lists_is_not_taken_for_removed() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    fs.put("a.md", b"a");
    fs.tick(10 * SEC);
    m.scan().unwrap();
    let (reached, open) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    *probe.list_gate.lock().unwrap() = Some((Arc::clone(&reached), Arc::clone(&open)));
    std::thread::scope(|s| {
        let scan = s.spawn(|| m.scan().unwrap());
        while !reached.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // The listing is made without it; then it is saved, and a.md removed.
        fs.put("new.md", b"new");
        m.note_written(&np("new.md"), b"new");
        fs.del(&np("a.md"));
        m.note_removed(&np("a.md"));
        let removed = m.generation();
        open.store(true, Ordering::SeqCst);
        let d = scan.join().unwrap();
        assert!(d.removed.is_empty() && d.changed.is_empty(), "{d:?}");
        assert_eq!(m.generation(), removed);
    });
    *probe.list_gate.lock().unwrap() = None;
    assert!(m.has(&np("new.md")) && !m.has(&np("a.md")));
    fs.tick(10 * SEC);
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(summary(&m), cold(Arc::clone(&fs) as Arc<dyn Storage>));
}

/// xorshift64*: a seeded generator, so a failure names its seed (P10).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Incremental equals rebuilt (`docs/design/0.14-architecture.md` §7 rule 4):
/// after random creates, edits (some in the same tick as a scan, some
/// keeping the size), renames, deletes, own writes and scans, a scan gives
/// the rows a manifest built from nothing gives, hash by hash; and the
/// deltas subscribers got, with our own writes, add up to the same.
#[test]
fn after_random_changes_a_scan_equals_a_manifest_built_from_nothing() {
    let cases: u64 = std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(200);
    for seed in 1..=cases {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let fs = Fake::new();
        let m = Manifest::open_with_clock(&vault_of(&fs), fs.clock());
        let rx = m.subscribe();
        let mut seen: BTreeMap<NotePath, FileHash> = BTreeMap::new();
        let names = ["a.md", "b.md", "c.md", "d/e.md", "d/f.md", "g/h/i.md", "pic.png", "Journal/x.md.qenc"];
        for _ in 0..60 {
            let p = np(names[rng.below(names.len() as u64) as usize]);
            // Same size or not: one or two letters.
            let content = vec![b'a' + rng.below(3) as u8; 1 + rng.below(2) as usize];
            match rng.below(10) {
                0..=2 => fs.put(p.as_str(), &content),
                3 => {
                    fs.put(p.as_str(), &content);
                    seen.insert(p.clone(), m.note_written(&p, &content));
                }
                4 => {
                    let to = np(names[rng.below(names.len() as u64) as usize]);
                    if fs.exists(&p) && !fs.exists(&to) {
                        fs.rename(&p, &to).unwrap();
                    }
                }
                5 => fs.del(&p),
                6 => {
                    if fs.exists(&p) {
                        fs.del(&p);
                        m.note_removed(&p);
                        seen.remove(&p);
                    }
                }
                7 => fs.tick(rng.below(3 * SEC)),
                _ => {
                    m.scan().unwrap();
                }
            }
            while let Ok(d) = rx.try_recv() {
                for r in &d.changed {
                    seen.insert(r.path.clone(), r.hash);
                }
                for p in &d.removed {
                    seen.remove(p);
                }
            }
        }
        m.scan().unwrap();
        while let Ok(d) = rx.try_recv() {
            for r in &d.changed {
                seen.insert(r.path.clone(), r.hash);
            }
            for p in &d.removed {
                seen.remove(p);
            }
        }
        let want = cold(Arc::clone(&fs) as Arc<dyn Storage>);
        assert_eq!(summary(&m), want, "seed {seed}");
        assert_eq!(seen, want.iter().map(|(p, (_, h))| (p.clone(), *h)).collect(), "seed {seed}: deltas and own writes");
        // Every row's time is the listing's after that scan, except racy ones.
        for r in m.rows() {
            assert!(r.is_racy() || r.mtime_ns != 0, "seed {seed}: {r:?}");
        }
    }
}

#[test]
fn a_locked_folders_rows_hash_the_stored_bytes() {
    let fs = Arc::new(MemFs::with_files([("Journal/Day.md", "# Day\nsecret\n"), ("Plain.md", "# Plain\n")]));
    let v = vault_of(&fs);
    crate::lock::lock_folder(&v, "Journal", Some("correct horse"), crate::lock::Kdf::TEST, 1).unwrap();
    let m = Manifest::open(&v);
    m.scan().unwrap();
    assert!(!m.has(&np("Journal/Day.md")));
    let stored = np("Journal/Day.md.qenc");
    let row = m.get(&stored).unwrap();
    let bytes = fs.read_bytes(&stored).unwrap();
    assert_eq!((row.hash, row.size), (FileHash::of(&bytes), bytes.len() as u64));
    assert_ne!(row.hash, FileHash::of(b"# Day\nsecret\n"));
    assert!(m.has(&np("Journal/quire-lock.json")));
    assert!(m.has(&np("Plain.md")));
}

#[test]
fn works_on_mem_fs_with_nothing_persisted() {
    let fs = Arc::new(MemFs::with_files([("a.md", "a"), ("b/c.md", "c")]));
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), later());
    assert_eq!(m.scan().unwrap().changed.len(), 2);
    assert_eq!(probe.reads(), 2);
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0, "MemFs gives sizes and times");
    assert!(!m.is_dirty(), "no place on disk");
    m.flush().unwrap();
    assert!(fs.list_all().iter().all(|p| !p.as_str().starts_with(".quire")));
}

#[test]
fn on_disk_a_scan_with_nothing_changed_opens_no_file_and_the_table_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalFs::new(dir.path()).unwrap());
    for i in 0..20 {
        local.write_atomic(&np(&format!("f{}/n{i}.md", i % 3)), &format!("note {i}")).unwrap();
    }
    local.write_atomic(&np(".quire/other.json"), "{}").unwrap();
    let probe = Probe::new(Arc::clone(&local) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), later());
    assert_eq!(m.scan().unwrap().changed.len(), 20);
    assert_eq!(probe.reads(), 20);
    assert!(m.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0);
    local.write_atomic(&np("f0/n0.md"), "note 0, longer now").unwrap();
    assert_eq!(m.scan().unwrap().changed.len(), 1);
    assert_eq!(probe.reads(), 1);
    assert!(m.is_dirty());
    m.flush().unwrap();
    assert!(!m.is_dirty());
    assert!(dir.path().join(CACHE_PATH).is_file());
    let (epoch, generation) = (m.epoch(), m.generation());
    // The next start: loaded, nothing read, the same rows, epoch and generation.
    let again = Manifest::open_with_clock(&vault_of(&probe), later());
    assert_eq!((again.epoch(), again.generation()), (epoch, generation));
    assert!(!again.scanned() && again.has(&np("f1/n1.md")));
    assert!(again.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0);
    assert_eq!(again.since(0).unwrap(), m.since(0).unwrap());
    assert!(!m.has(&np(".quire/other.json")) && !m.has(&np(CACHE_PATH)));
}

#[cfg(unix)]
#[test]
fn a_symlinked_note_or_folder_is_not_listed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.md"), "outside").unwrap();
    let local = Arc::new(LocalFs::new(dir.path()).unwrap());
    local.write_atomic(&np("real.md"), "real").unwrap();
    std::os::unix::fs::symlink(outside.path().join("x.md"), dir.path().join("link.md")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
    let m = Manifest::open(&vault_of(&local));
    m.scan().unwrap();
    assert!(m.has(&np("real.md")));
    assert!(!m.has(&np("link.md")) && !m.has(&np("linked/x.md")));
}

#[test]
fn a_second_process_replacing_the_file_costs_only_a_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalFs::new(dir.path()).unwrap());
    for i in 0..10 {
        local.write_atomic(&np(&format!("n{i}.md")), &format!("note {i}")).unwrap();
    }
    let probe = Probe::new(Arc::clone(&local) as Arc<dyn Storage>);
    let open = || Manifest::open_with_clock(&vault_of(&probe), later());
    // Two processes on one vault, each with its own table.
    let a = open();
    a.scan().unwrap();
    a.flush().unwrap();
    let b = open();
    probe.reads();
    local.write_atomic(&np("n1.md"), "note 1 changed in b").unwrap();
    b.scan().unwrap();
    assert_eq!(probe.reads(), 1);
    b.flush().unwrap();
    local.write_atomic(&np("n2.md"), "note 2 changed in a").unwrap();
    a.scan().unwrap();
    assert_eq!(probe.reads(), 2, "a reads what b changed and its own change, nothing else");
    a.flush().unwrap(); // replaces b's file
    let c = open();
    assert!(c.scan().unwrap().is_empty());
    assert_eq!(probe.reads(), 0);
    let truth = cold(Arc::clone(&local) as Arc<dyn Storage>);
    assert_eq!(summary(&c), truth);
    b.scan().unwrap();
    assert_eq!(summary(&b), truth);
    probe.reads();
    let file = dir.path().join(CACHE_PATH);
    let good = std::fs::read(&file).unwrap();
    // One flipped byte, a truncated file, no file: a cold build, the same rows.
    for damage in [0, 1, 2] {
        let mut bytes = good.clone();
        match damage {
            0 => bytes[cache::HEADER_LEN + 40] ^= 1,
            1 => bytes.truncate(bytes.len() - 10),
            _ => bytes.clear(),
        }
        if damage == 2 {
            std::fs::remove_file(&file).unwrap();
        } else {
            std::fs::write(&file, &bytes).unwrap();
        }
        let d = open();
        assert_ne!(d.epoch(), c.epoch(), "built from nothing: a new epoch");
        assert_eq!(d.since(0).unwrap(), []);
        d.scan().unwrap();
        assert_eq!(probe.reads(), 10);
        assert_eq!(summary(&d), truth);
    }
    // Another vault's copy is not used.
    std::fs::write(&file, &good).unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(other.path().join(".quire/cache")).unwrap();
    std::fs::copy(&file, other.path().join(CACHE_PATH)).unwrap();
    std::fs::write(other.path().join("n1.md"), "different").unwrap();
    let ofs = Arc::new(LocalFs::new(other.path()).unwrap());
    let o = Manifest::open_with_clock(&vault_of(&ofs), later());
    assert!(o.rows().is_empty());
    o.scan().unwrap();
    assert_eq!(summary(&o), cold(ofs as Arc<dyn Storage>));
}

#[test]
fn a_newer_format_on_disk_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let local = Arc::new(LocalFs::new(dir.path()).unwrap());
    local.write_atomic(&np("a.md"), "a").unwrap();
    let file = dir.path().join(CACHE_PATH);
    assert_eq!(cache::write(&file, Kind::MANIFEST, FORMAT + 1, BUILDER, b"from a newer Quire").unwrap(), cache::Written::Written);
    let m = Manifest::open_with_clock(&vault_of(&local), later());
    m.scan().unwrap();
    assert!(m.is_dirty());
    m.flush().unwrap();
    assert!(!m.is_dirty(), "in memory only from now on");
    assert_eq!(cache::read(&file, Kind::MANIFEST, FORMAT + 1, BUILDER).as_deref(), Some(&b"from a newer Quire"[..]));
}

#[test]
fn old_tombstones_are_dropped_and_since_then_says_so() {
    let fs = Fake::new();
    let m = Manifest::open_with_clock(&vault_of(&fs), fs.clock());
    for i in 0..1100 {
        fs.put(&format!("n{i}.md"), b"x");
    }
    fs.tick(10 * SEC);
    m.scan().unwrap();
    let g = m.generation();
    for p in fs.paths() {
        fs.del(&p);
    }
    fs.put("kept.md", b"k");
    fs.tick(10 * SEC);
    let d = m.scan().unwrap();
    assert_eq!(d.removed.len(), 1100);
    let tombs = m.shared.table.read().unwrap().rows.values().filter(|r| r.gone).count();
    assert!(tombs <= MIN_TOMBSTONES, "{tombs}");
    assert_eq!(m.since(g - 1), None, "too old: compare everything");
    assert_eq!(m.since(m.generation()).unwrap(), []);
    assert_eq!(m.rows().len(), 1);
}

#[test]
fn the_body_round_trips_and_any_doubt_is_no_table() {
    let fs = Fake::new();
    let m = Manifest::open_with_clock(&vault_of(&fs), fs.clock());
    fs.put("a.md", b"a");
    fs.put("b/c.md", b"c");
    m.scan().unwrap();
    fs.del(&np("a.md"));
    fs.tick(10 * SEC);
    m.scan().unwrap();
    let origin = [7u8; 32];
    let t = m.shared.table.read().unwrap();
    let body = encode(&t, &origin);
    let back = decode(&body, &origin).unwrap();
    assert_eq!((back.epoch, back.generation, back.floor), (t.epoch, t.generation, t.floor));
    assert_eq!(back.rows, t.rows);
    assert!(decode(&body, &[8u8; 32]).is_none(), "another origin");
    assert!(decode(&body[..body.len() - 1], &origin).is_none(), "short");
    let mut long = body.clone();
    long.push(0);
    assert!(decode(&long, &origin).is_none(), "bytes left over");
    let mut flags = body.clone();
    *flags.last_mut().unwrap() |= 4;
    assert!(decode(&flags, &origin).is_none(), "an unknown flag");
    let mut generation = body;
    generation[48..56].copy_from_slice(&0u64.to_le_bytes());
    assert!(decode(&generation, &origin).is_none(), "a row from a later generation");
    let bad = |path: &str| {
        let mut b = Table::fresh([0; 16]);
        b.rows.insert(
            np(path),
            Row { path: np(path), size: 0, mtime_ns: 0, hash: FileHash([0; 32]), generation: 0, gone: false, racy: false },
        );
        let mut enc = encode(&b, &origin);
        // Put the raw path back as written, bypassing NotePath's clean-up.
        let at = 32 + 16 + 24 + 1;
        enc.splice(at..at + np(path).as_str().len(), path.bytes());
        enc[at - 1] = path.len() as u8;
        decode(&enc, &origin)
    };
    assert!(bad("ok.md").is_some());
    assert!(bad("../escape.md").is_none());
    assert!(bad("/abs.md").is_none());
}

/// Budgets (`docs/design/0.14-build-plan.md`, A2; report only): a warm scan
/// with nothing changed opens no file; 10k files in 60 ms or less, 50k in
/// 300 ms (`a_warm_scan_of_50k_files`, ignored: run by hand); the file is at
/// most 120 bytes a row.
#[test]
fn a_warm_scan_of_10k_files() {
    budget(10_000);
}

#[test]
#[ignore = "creates 50,000 files: run by hand for the budget"]
fn a_warm_scan_of_50k_files() {
    budget(50_000);
}

fn budget(n: usize) {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..n {
        let folder = dir.path().join(format!("Folder {:02}/Sub {}", i % 50, i % 7));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(format!("A note with a title {i}.md")), format!("# Note {i}\n\nSome text of the note.\n")).unwrap();
    }
    let probe = Probe::new(Arc::new(LocalFs::new(dir.path()).unwrap()) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), later());
    let t = Instant::now();
    m.scan().unwrap();
    let cold = t.elapsed();
    probe.reads();
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        assert!(m.scan().unwrap().is_empty());
        best = best.min(t.elapsed());
    }
    assert_eq!(probe.reads(), 0, "a warm scan opens no file");
    m.flush().unwrap();
    let size = std::fs::metadata(dir.path().join(CACHE_PATH)).unwrap().len();
    let t = Instant::now();
    let again = Manifest::open_with_clock(&vault_of(&probe), later());
    let load = t.elapsed();
    assert_eq!(again.rows().len(), n);
    println!(
        "manifest, {n} files: cold scan {cold:?}, warm scan best of 5 {best:?}, load {load:?}, file {size} bytes ({:.1} a row)",
        size as f64 / n as f64
    );
}

/// A reader of `len` seeded bytes that are never all in memory: it makes
/// each piece when asked, hashes what it gave, and remembers the biggest
/// piece it was asked for.
struct Endless {
    left: u64,
    rng: Rng,
    given: Sha256,
    biggest: usize,
}

impl std::io::Read for Endless {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.biggest = self.biggest.max(buf.len());
        let n = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        // Uneven pieces, as a disk gives them.
        let n = n.min(1 + self.rng.below(HASH_BUFFER as u64 + 7) as usize);
        for b in &mut buf[..n] {
            *b = self.rng.next() as u8;
        }
        self.given.update(&buf[..n]);
        self.left -= n as u64;
        Ok(n)
    }
}

#[test]
fn hashing_100_mb_never_holds_it_all() {
    let len = 100 * 1024 * 1024;
    let mut r = Endless { left: len, rng: Rng(0x5eed), given: Sha256::new(), biggest: 0 };
    let (hash, n) = FileHash::of_reader(&mut r).unwrap();
    assert_eq!(n, len);
    assert_eq!(hash.0, <[u8; 32]>::from(r.given.finalize()));
    assert!(r.biggest <= HASH_BUFFER, "asked for {} bytes at once", r.biggest);
}

#[test]
fn a_streamed_hash_equals_the_one_shot_hash_across_buffer_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let local = LocalFs::new(dir.path()).unwrap();
    let mem = MemFs::new();
    let b = HASH_BUFFER;
    let mut sizes = vec![0, 1, b - 1, b, b + 1, 2 * b - 1, 2 * b, 2 * b + 1, 3 * b + 17];
    let mut rng = Rng(0xb0b);
    sizes.extend((0..12).map(|_| rng.below(5 * b as u64) as usize));
    for (i, size) in sizes.into_iter().enumerate() {
        let bytes: Vec<u8> = (0..size).map(|_| rng.next() as u8).collect();
        let p = np(&format!("f{i}.bin"));
        local.write_bytes_atomic(&p, &bytes).unwrap();
        mem.write_bytes_atomic(&p, &bytes).unwrap();
        let want = (FileHash::of(&bytes), size as u64);
        assert_eq!(local.hash_file(&p).unwrap(), want, "LocalFs, {size} bytes");
        assert_eq!(mem.hash_file(&p).unwrap(), want, "the default, {size} bytes");
        assert_eq!(FileHash::of_reader(&bytes[..]).unwrap(), want, "{size} bytes");
    }
    assert!(matches!(local.hash_file(&np("missing.bin")), Err(VaultError::NotFound(_))));
}

#[test]
fn verify_where_reads_only_the_files_it_is_given() {
    let fs = Fake::new();
    let probe = Probe::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let m = Manifest::open_with_clock(&vault_of(&probe), fs.clock());
    fs.put("a.md", b"aaaa");
    fs.put("video.mp4", b"vvvv");
    fs.tick(10 * SEC);
    m.scan().unwrap();
    probe.reads();
    // Both change, keeping size and time: only verify can see it.
    fs.put_keeping_time("a.md", b"bbbb");
    fs.put_keeping_time("video.mp4", b"wwww");
    let notes = |p: &NotePath| p.as_str() == "a.md";
    let d = m.verify_where(&notes).unwrap();
    assert!(d.verified);
    assert_eq!(probe.reads(), 1, "the video is not read");
    assert_eq!(d.changed.iter().map(|r| (r.path.as_str(), r.hash)).collect::<Vec<_>>(), [("a.md", FileHash::of(b"bbbb"))]);
    // A file the predicate turns down still follows the scan rules: a
    // changed size is read.
    fs.put("video.mp4", b"a longer video");
    fs.tick(10 * SEC);
    let d = m.verify_where(&notes).unwrap();
    assert_eq!(probe.reads(), 2);
    assert_eq!(d.changed.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["video.mp4"]);
}
