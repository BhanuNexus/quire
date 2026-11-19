//! `VaultStore` on the vault manifest (`docs/design/0.14-sync-speed.md` §3.1,
//! §3.6): what a pass opens, the compare-and-swap by hash, a listing error
//! with a warm manifest, an unreadable note, and the verify pass.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use quire_sync::folder::{self, FolderTransport};
use quire_sync::{DeviceId, Engine, KdfParams, Report, SpaceId, SyncState, VaultKey, WrappedKey};
use quire_vault::changes::{Clock, Manifest};
use quire_vault::{FileStat, NotePath, Storage, Vault, VaultError};

use super::*;

/// A vault's storage with its reads counted (the "note files opened"
/// budget) and faults on demand.
struct Probe {
    inner: Arc<dyn Storage>,
    reads: AtomicUsize,
    fail_list: AtomicBool,
    fail_read: Mutex<Option<NotePath>>,
}

impl Probe {
    fn reads(&self) -> usize {
        self.reads.swap(0, Ordering::SeqCst)
    }
}

impl Storage for Probe {
    fn list(&self) -> quire_vault::Result<Vec<NotePath>> {
        self.inner.list()
    }
    fn list_stat(&self) -> quire_vault::Result<Vec<(NotePath, Option<FileStat>)>> {
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(VaultError::Io { path: "Work".into(), source: std::io::Error::other("the drive went away") });
        }
        self.inner.list_stat()
    }
    fn read_bytes(&self, path: &NotePath) -> quire_vault::Result<Vec<u8>> {
        if !path.as_str().starts_with(".quire/") {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        if self.fail_read.lock().unwrap().as_ref() == Some(path) {
            return Err(VaultError::Io { path: path.to_string(), source: std::io::Error::other("permission denied") });
        }
        self.inner.read_bytes(path)
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> quire_vault::Result<()> {
        self.inner.write_bytes_atomic(path, bytes)
    }
    fn rename(&self, from: &NotePath, to: &NotePath) -> quire_vault::Result<()> {
        self.inner.rename(from, to)
    }
    fn trash(&self, path: &NotePath) -> quire_vault::Result<()> {
        self.inner.trash(path)
    }
    fn exists(&self, path: &NotePath) -> bool {
        self.inner.exists(path)
    }
    fn modified(&self, path: &NotePath) -> quire_vault::Result<SystemTime> {
        self.inner.modified(path)
    }
    fn absolute(&self, path: &NotePath) -> Option<PathBuf> {
        self.inner.absolute(path)
    }
    fn list_under(&self, folder: &str) -> quire_vault::Result<Vec<NotePath>> {
        self.inner.list_under(folder)
    }
    fn remove(&self, path: &NotePath) -> quire_vault::Result<()> {
        self.inner.remove(path)
    }
}

/// Far ahead of every file time, so nothing written by a test counts as
/// racy (the manifest hashes a file again within 2 s of its time).
fn later() -> Clock {
    Arc::new(|| SystemTime::now() + Duration::from_hours(1))
}

struct Dev {
    root: PathBuf,
    probe: Arc<Probe>,
    store: VaultStore,
    engine: Engine,
    t: FolderTransport,
}

impl Dev {
    fn pass(&mut self) -> Result<Report, String> {
        begin_pass(&mut self.store)?;
        self.engine.sync(&mut self.store, &self.t, "2026-11-19").map_err(|e| e.to_string())
    }

    fn put(&self, path: &str, text: &str) {
        let p = self.root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn get(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path)).ok()
    }
}

fn pair(dir: &Path) -> (Dev, Dev) {
    let shared = folder::sync_root(&dir.join("Dropbox"));
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&shared, &space, &WrappedKey::wrap(&key, "pw", KdfParams::TEST).unwrap()).unwrap();
    let dev = |name: &str| {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let inner = Arc::clone(Vault::open_local(&root).unwrap().storage());
        let probe = Arc::new(Probe { inner, reads: AtomicUsize::new(0), fail_list: AtomicBool::new(false), fail_read: Mutex::new(None) });
        let vault = Vault::new(Arc::clone(&probe) as Arc<dyn Storage>);
        let store = VaultStore::with_manifest(vault.clone(), Manifest::open_with_clock(&vault, later()));
        let id = DeviceId::random().unwrap();
        let t = FolderTransport::new(&shared, &space, id.clone(), None);
        Dev { root, probe, store, engine: Engine::new(space.clone(), id, name, key.keys(), SyncState::default()), t }
    };
    (dev("A"), dev("B"))
}

#[test]
fn an_idle_pass_opens_no_note_and_a_changed_one_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = pair(dir.path());
    for i in 0..30 {
        a.put(&format!("Notes/{i:02}.md"), &format!("note {i}\n"));
    }
    a.put("pic.png", "not really a png");
    a.pass().unwrap();
    b.pass().unwrap();
    // B wrote what it received: the manifest hashes its own writes once more.
    b.pass().unwrap();
    a.pass().unwrap();
    a.probe.reads();
    b.probe.reads();
    for d in [&mut a, &mut b] {
        let r = d.pass().unwrap();
        assert!(!r.changed_anything());
        assert_eq!(d.probe.reads(), 0, "an idle pass opens no note");
    }
    a.put("Notes/07.md", "note 7, longer now\n");
    let r = a.pass().unwrap();
    assert_eq!(r.pushed, 1);
    // One read by the manifest's scan (its size changed) and one by sync
    // for the text it sends.
    assert_eq!(a.probe.reads(), 2);
    let r = b.pass().unwrap();
    assert_eq!(r.pulled, 1);
    assert_eq!(b.get("Notes/07.md").as_deref(), Some("note 7, longer now\n"));
    // Receiving opens the note three times: its old text (changed here?),
    // the check that no unsynced file is in the way, and the write's
    // compare-and-swap. Only that note.
    assert_eq!(b.probe.reads(), 3);
    a.pass().unwrap();
    b.pass().unwrap();
    a.probe.reads();
    b.probe.reads();
    a.pass().unwrap();
    b.pass().unwrap();
    assert_eq!(a.probe.reads() + b.probe.reads(), 0);
}

#[test]
fn an_edit_that_kept_its_time_and_size_merges_on_the_second_pass() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = pair(dir.path());
    a.put("Plan.md", "one\ntwo\nthree\n");
    a.pass().unwrap();
    b.pass().unwrap();
    b.pass().unwrap();
    // On B, an edit that keeps the file's size and time (a tool that
    // restores times, a coarse clock).
    let p = b.root.join("Plan.md");
    let t = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::fs::write(&p, "one\ntwo\nTHREE\n").unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(t).unwrap();
    // A changes another line.
    a.put("Plan.md", "ONE\ntwo\nthree\n");
    a.pass().unwrap();
    let first = b.pass();
    let second = b.pass();
    assert!(first.is_ok() || second.is_ok(), "{first:?} {second:?}");
    assert_eq!(b.get("Plan.md").as_deref(), Some("ONE\ntwo\nTHREE\n"), "merged, the edit kept");
    a.pass().unwrap();
    assert_eq!(a.get("Plan.md").as_deref(), Some("ONE\ntwo\nTHREE\n"));
}

#[test]
fn a_write_refused_by_a_hidden_edit_corrects_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, _) = pair(dir.path());
    a.put("Plan.md", "one\n");
    a.pass().unwrap();
    begin_pass(&mut a.store).unwrap();
    let p = a.root.join("Plan.md");
    let t = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::fs::write(&p, "ONE\n").unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(t).unwrap();
    // The engine writes without reading first: refused, and the row learns
    // the bytes on disk, so the next pass reads the note.
    assert!(LocalStore::write(&mut a.store, "Plan.md", "from sync\n").is_err());
    assert!(LocalStore::write(&mut a.store, "Plan.md", "from sync\n").is_err(), "still refused in this pass");
    assert_eq!(a.get("Plan.md").as_deref(), Some("ONE\n"));
    begin_pass(&mut a.store).unwrap();
    assert_eq!(LocalStore::stamp(&a.store, "Plan.md"), Some(quire_sync::Stamp(quire_vault::changes::FileHash::of(b"ONE\n").0)));
}

#[test]
fn a_listing_error_with_a_warm_manifest_still_stops() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = pair(dir.path());
    for i in 0..12 {
        a.put(&format!("{i:02}.md"), &format!("note {i}\n"));
    }
    a.pass().unwrap();
    b.pass().unwrap();
    a.store.manifest().flush().unwrap();
    // The vault can't be listed now; the manifest still has every row.
    a.probe.fail_list.store(true, Ordering::SeqCst);
    let e = a.pass().unwrap_err();
    assert!(e.contains("the drive went away"), "{e}");
    assert!(a.store.list().is_err(), "no listing for this pass");
    b.pass().unwrap();
    assert_eq!(std::fs::read_dir(&b.root).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "md")).count(), 12);
    a.probe.fail_list.store(false, Ordering::SeqCst);
    a.pass().unwrap();
    b.pass().unwrap();
    assert_eq!(b.get("03.md").as_deref(), Some("note 3\n"));
}

#[test]
fn an_unreadable_note_is_skipped_and_named_while_the_rest_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = pair(dir.path());
    a.put("Secret.md", "one\ntwo\n");
    a.put("Other.md", "other\n");
    a.pass().unwrap();
    b.pass().unwrap();
    b.pass().unwrap();
    // On B, Secret.md changes and then can't be read.
    b.put("Secret.md", "one\ntwo\nthree on B\n");
    *b.probe.fail_read.lock().unwrap() = Some(NotePath::new("Secret.md"));
    a.put("Other.md", "other, edited\n");
    a.put("Secret.md", "one on A\ntwo\n");
    a.pass().unwrap();
    let r = b.pass().unwrap();
    assert_eq!(b.get("Other.md").as_deref(), Some("other, edited\n"), "the rest syncs");
    assert_eq!(b.get("Secret.md").as_deref(), Some("one\ntwo\nthree on B\n"), "never written over");
    assert_eq!(b.store.unreadable().len(), 1);
    let status = unreadable_status(&b.store.unreadable());
    assert!(status.contains("“Secret.md”") && status.contains("permission denied"), "{status}");
    assert!(r.skipped.iter().any(|(p, why)| p == "Secret.md" && why.starts_with(UNREADABLE)), "{r:?}");
    let told = skipped_notice("Secret.md", &r.skipped[0].1);
    assert!(told.contains("can't be read on this computer"), "{told}");
    // Every pass names it, and sends nothing for it.
    let r = b.pass().unwrap();
    assert!(r.pushed == 0 && b.store.unreadable().len() == 1);
    a.pass().unwrap();
    assert_eq!(a.get("Secret.md").as_deref(), Some("one on A\ntwo\n"), "not sent as deleted");
    // Readable again: both edits meet.
    *b.probe.fail_read.lock().unwrap() = None;
    b.pass().unwrap();
    b.pass().unwrap();
    a.pass().unwrap();
    assert_eq!(b.get("Secret.md").as_deref(), Some("one on A\ntwo\nthree on B\n"));
    assert_eq!(a.get("Secret.md"), b.get("Secret.md"));
    assert!(b.store.unreadable().is_empty());
}

#[test]
fn a_verify_pass_finds_a_change_that_kept_its_time_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let (va, shared) = (dir.path().join("A"), dir.path().join("Shared"));
    std::fs::create_dir_all(&va).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(va.join("Plan.md"), "one\n").unwrap();
    sync_create_at(&va, &SyncTarget::Folder(shared), "correct horse battery", "laptop", &Selection::default()).unwrap();
    sync_once_at(&va).unwrap();
    // Wait out the manifest's racy window, so the file's row is trusted.
    std::thread::sleep(Duration::from_millis(2100));
    sync_once_at(&va).unwrap();
    let p = va.join("Plan.md");
    let t = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::fs::write(&p, "ONE\n").unwrap();
    std::fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(t).unwrap();
    assert_eq!(sync_once_at(&va).unwrap().pushed, 0, "size and time say nothing changed");
    assert_eq!(sync_verify_at(&va).unwrap().pushed, 1, "a verify pass reads every note");
    assert!(journal::Journal::open(&va, "state.json").unwrap().journal.verified_at() > 0);
}
