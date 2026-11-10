// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! S3 and WebDAV storage that loses, cuts short, damages or rewrites
//! revision objects (0.12 review H2, M5, M6; data review L2). Sync carries
//! on with what it can verify, says what it skipped, and the device that
//! wrote a lost object puts it back.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::Path;
use std::sync::Arc;

use quire_sync::remote::mock::{Kind, MockServer};
use quire_sync::remote::net::Retry;
use quire_sync::remote::{self, MemoryObjects, ObjectStore, RemoteTransport, WebDavConfig, WebDavCredentials, WebDavStore};
use quire_sync::{DeviceId, Engine, KdfParams, MemoryStore, SpaceId, SyncError, SyncState, VaultKey, WrappedKey};
use zeroize::Zeroizing;

const TODAY: &str = "2026-11-11";

struct Dev {
    engine: Engine,
    local: MemoryStore,
    t: RemoteTransport,
    id: DeviceId,
}

impl Dev {
    fn pass(&mut self) -> Result<quire_sync::Report, SyncError> {
        let r = self.engine.sync(&mut self.local, &self.t, TODAY);
        let f = self.t.flush();
        let r = r?;
        f?;
        Ok(r)
    }
}

struct Vault {
    store: Arc<dyn ObjectStore>,
    space: SpaceId,
    key: VaultKey,
}

impl Vault {
    fn new(store: Arc<dyn ObjectStore>) -> Self {
        let key = VaultKey::generate().unwrap();
        let space = SpaceId::random().unwrap();
        remote::create_space(&*store, &space, &WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap()).unwrap();
        Self { store, space, key }
    }

    /// A device; `state` keeps its mirror across sessions.
    fn device(&self, id: &DeviceId, state: Option<&Path>, engine: Option<Engine>) -> Dev {
        let t = RemoteTransport::new(Arc::clone(&self.store), &self.space, id.clone(), state).unwrap().with_keys(self.key.keys());
        let engine = engine.unwrap_or_else(|| Engine::new(self.space.clone(), id.clone(), "dev", self.key.keys(), SyncState::default()));
        Dev { engine, local: MemoryStore::default(), t, id: id.clone() }
    }

    fn object(&self, d: &DeviceId, n: u64) -> String {
        format!("{}/{}/devices/{d}/{n:012}.rev", remote::ROOT, self.space)
    }
}

#[derive(Debug, Clone, Copy)]
enum Damage {
    Deleted,
    Truncated,
    Corrupted,
}

fn damage(store: &MemoryObjects, key: &str, how: Damage) {
    let body = store.get(key, u64::MAX).unwrap().unwrap();
    match how {
        Damage::Deleted => store.set_raw(key, None),
        Damage::Truncated => store.set_raw(key, Some(body[..body.len() - 40].to_vec())),
        Damage::Corrupted => {
            let mut b = body;
            let last = b.len() - 1;
            b[last] ^= 0x55;
            store.set_raw(key, Some(b));
        }
    }
}

/// Device B writes eight notes, one per pass (objects 1–8); object 2 is
/// then lost or damaged in the storage.
fn eight_notes_then(how: Damage) -> (Arc<MemoryObjects>, Vault, Dev, tempfile::TempDir) {
    let mem = Arc::new(MemoryObjects::new());
    let vault = Vault::new(Arc::<MemoryObjects>::clone(&mem));
    let dir = tempfile::tempdir().unwrap();
    let mut b = vault.device(&DeviceId::random().unwrap(), Some(dir.path()), None);
    for i in 1..=8 {
        b.local.0.insert(format!("n{i}.md"), format!("note {i}\n"));
        b.pass().unwrap();
    }
    damage(&mem, &vault.object(&b.id, 2), how);
    (mem, vault, b, dir)
}

#[test]
fn one_lost_or_damaged_object_is_skipped_named_and_put_back() {
    for how in [Damage::Deleted, Damage::Truncated, Damage::Corrupted] {
        let (_mem, vault, b, dir) = eight_notes_then(how);
        // Another device carries on with everything it can verify, and says
        // which revision of which device it skipped.
        let mut a = vault.device(&DeviceId::random().unwrap(), None, None);
        a.pass().unwrap();
        assert_eq!(a.local.0.len(), 7, "{how:?}: every other note arrives");
        assert!(!a.local.0.contains_key("n2.md"));
        let w = a.t.waiting().unwrap();
        assert!(w.contains(&format!("device {} revision 2", &b.id.0[..8])), "{how:?}: {w}");
        // A device joining later does the same.
        let mut c = vault.device(&DeviceId::random().unwrap(), None, None);
        c.pass().unwrap();
        assert_eq!(c.local.0.len(), 7, "{how:?}");
        // B doesn't know until it starts a new session, which checks its
        // own objects against the listing and uploads the missing or cut
        // ones again from its mirror.
        let (engine, local) = (b.engine, b.local);
        let mut b = vault.device(&b.id, Some(dir.path()), Some(engine));
        b.local = local;
        b.pass().unwrap();
        a.pass().unwrap();
        match how {
            Damage::Deleted | Damage::Truncated => {
                assert_eq!(a.local.0.get("n2.md").map(String::as_str), Some("note 2\n"), "{how:?}: put back and fetched again");
                assert!(a.t.waiting().is_none(), "{how:?}: {:?}", a.t.waiting());
            }
            // Same size, damaged bytes: the listing can't tell B. Readers
            // keep skipping it and keep saying so.
            Damage::Corrupted => {
                assert!(a.t.waiting().is_some_and(|w| w.contains("revision 2")));
                assert_eq!(a.local.0.len(), 7);
            }
        }
    }
}

#[test]
fn skipped_revisions_are_remembered_across_restarts() {
    let (_mem, vault, b, _dir) = eight_notes_then(Damage::Deleted);
    let state = tempfile::tempdir().unwrap();
    let id = DeviceId::random().unwrap();
    let mut a = vault.device(&id, Some(state.path()), None);
    a.pass().unwrap();
    let engine = a.engine;
    // A new process reads progress.json: nothing is fetched twice, and the
    // warning is still there.
    let mut a = vault.device(&id, Some(state.path()), Some(engine));
    a.pass().unwrap();
    assert!(a.t.waiting().is_some_and(|w| w.contains(&format!("device {} revision 2", &b.id.0[..8]))));
    let lines: usize = std::fs::read_dir(state.path().join("mirror").join(&vault.space.0).join("devices").join(&b.id.0))
        .unwrap()
        .flatten()
        .map(|f| std::fs::read_to_string(f.path()).unwrap().lines().count())
        .sum();
    assert_eq!(lines, 7);
}

#[test]
fn a_gap_longer_than_a_batch_is_skipped_too() {
    let mem = Arc::new(MemoryObjects::new());
    let vault = Vault::new(Arc::<MemoryObjects>::clone(&mem));
    let mut b = vault.device(&DeviceId::random().unwrap(), None, None);
    for i in 1..=50 {
        b.local.0.insert(format!("n{i}.md"), format!("note {i}\n"));
        b.pass().unwrap();
    }
    for n in 3..=45 {
        mem.set_raw(&vault.object(&b.id, n), None);
    }
    let mut a = vault.device(&DeviceId::random().unwrap(), None, None);
    a.pass().unwrap();
    assert_eq!(a.local.0.len(), 2 + 5);
    assert!(a.t.waiting().is_some_and(|w| w.contains("Skipped 43 revision(s)")));
}

#[test]
fn missing_newest_objects_wait_rather_than_skip() {
    let (mem, vault, b, _dir) = eight_notes_then(Damage::Deleted);
    mem.set_raw(&vault.object(&b.id, 8), None);
    let mut a = vault.device(&DeviceId::random().unwrap(), None, None);
    a.pass().unwrap();
    // 2 is skipped (3–7 are there); 8 has nothing after it yet: waiting.
    assert_eq!(a.local.0.len(), 6);
    let w = a.t.waiting().unwrap();
    assert!(w.contains("Waiting for 1 device(s)") && w.contains("revision 2"), "{w}");
}

/// The storage rewrites a revision's header to make an old text current
/// (review M6): it's never used.
#[test]
fn a_rewritten_header_is_refused() {
    let mem = Arc::new(MemoryObjects::new());
    let vault = Vault::new(Arc::<MemoryObjects>::clone(&mem));
    let mut b = vault.device(&DeviceId::random().unwrap(), None, None);
    for text in ["old\n", "middle\n", "new\n"] {
        b.local.0.insert("A.md".into(), text.into());
        b.pass().unwrap();
    }
    // Serve the first revision again as a fourth object, now claiming the
    // newest as its parent: without a bound header it would win.
    let first = remote::decode_object(&mem.get(&vault.object(&b.id, 1), u64::MAX).unwrap().unwrap()).unwrap();
    let newest = remote::decode_object(&mem.get(&vault.object(&b.id, 3), u64::MAX).unwrap().unwrap()).unwrap();
    let forged = quire_sync::Revision { parents: vec![newest.rev], rev: quire_sync::RevId::random().unwrap(), ..first };
    mem.set_raw(&vault.object(&b.id, 4), Some(remote::encode_object(&forged)));
    mem.set_raw(&format!("{}/{}/tips/{}", remote::ROOT, vault.space, b.id), Some(br#"{"n":4,"w":"x"}"#.to_vec()));
    let mut a = vault.device(&DeviceId::random().unwrap(), None, None);
    a.pass().unwrap();
    assert_eq!(a.local.0.get("A.md").map(String::as_str), Some("new\n"));
    assert!(a.t.waiting().is_some_and(|w| w.contains("revision 4")));
    // Even without the transport checking, the engine can't open it: an
    // error, never the old text.
    let bare = RemoteTransport::new(Arc::<MemoryObjects>::clone(&mem), &vault.space, DeviceId::random().unwrap(), None).unwrap();
    let mut e = Engine::new(vault.space.clone(), DeviceId::random().unwrap(), "x", vault.key.keys(), SyncState::default());
    let mut local = MemoryStore::default();
    let r = e.sync(&mut local, &bare, TODAY);
    assert!(r.is_err() || local.0.get("A.md").map(String::as_str) == Some("new\n"), "{r:?} {local:?}");
}

/// The storage says this device uploaded 10¹² revisions after its mirror
/// was lost (review M5): refused at once instead of fetched for ever.
#[test]
fn a_tip_far_beyond_the_objects_is_refused() {
    let mem = Arc::new(MemoryObjects::new());
    let vault = Vault::new(Arc::<MemoryObjects>::clone(&mem));
    let dir = tempfile::tempdir().unwrap();
    let mut b = vault.device(&DeviceId::random().unwrap(), Some(dir.path()), None);
    b.local.0.insert("A.md".into(), "a\n".into());
    b.pass().unwrap();
    // Replay the one object under other numbers too.
    let one = mem.get(&vault.object(&b.id, 1), u64::MAX).unwrap().unwrap();
    for n in 2..=5 {
        mem.set_raw(&vault.object(&b.id, n), Some(one.clone()));
    }
    mem.set_raw(&format!("{}/{}/tips/{}", remote::ROOT, vault.space, b.id), Some(br#"{"n":1000000000000,"w":"x"}"#.to_vec()));
    std::fs::remove_dir_all(dir.path().join("mirror")).unwrap();
    let mut b = vault.device(&b.id, Some(dir.path()), None);
    let e = b.pass().unwrap_err().to_string();
    assert!(e.contains("tip in the storage says"), "{e}");
}

/// A lost mirror is restored even when the storage lost one of this
/// device's own objects: the numbering holds, and new revisions don't
/// collide with old ones.
#[test]
fn a_lost_mirror_is_restored_around_a_lost_own_object() {
    let (mem, vault, b, dir) = eight_notes_then(Damage::Deleted);
    let local = b.local.clone();
    std::fs::remove_dir_all(dir.path().join("mirror")).unwrap();
    let mut b = vault.device(&b.id, Some(dir.path()), None);
    b.local = local;
    b.local.0.insert("n9.md".into(), "note 9\n".into());
    b.pass().unwrap();
    assert!(b.t.problem().is_none());
    assert!(mem.get(&vault.object(&b.id, 9), u64::MAX).unwrap().is_some());
}

fn dav_store(srv: &MockServer) -> Arc<dyn ObjectStore> {
    let cfg = WebDavConfig { url: srv.dav_url(), allow_http: false };
    Arc::new(
        WebDavStore::new(&cfg, &WebDavCredentials { user: "asha".into(), password: Zeroizing::new("app pass".into()) })
            .unwrap()
            .with_retry(Retry::QUICK),
    )
}

/// A WebDAV server kept part of an upload (data review L2): the retry
/// replaces it, instead of stopping with "Another computer is syncing as
/// this one".
#[test]
fn a_cut_short_upload_on_webdav_is_replaced_not_blamed_on_another_computer() {
    let srv = MockServer::start(Kind::WebDav { user: "asha".into(), password: "app pass".into() });
    let vault = Vault::new(dav_store(&srv));
    let mut b = vault.device(&DeviceId::random().unwrap(), None, None);
    b.local.0.insert("first.md".into(), "first\n".into());
    b.pass().unwrap();
    // The server holds the start of object 2 from an earlier, cut attempt.
    let mut cut = srv.objects()[&vault.object(&b.id, 1)].clone();
    cut.truncate(30);
    srv.state.objects.lock().unwrap().insert(vault.object(&b.id, 2), cut);
    b.local.0.insert("second.md".into(), "second\n".into());
    b.pass().unwrap();
    assert!(b.t.problem().is_none());
    let mut a = vault.device(&DeviceId::random().unwrap(), None, None);
    a.pass().unwrap();
    assert_eq!(a.local.0.get("second.md").map(String::as_str), Some("second\n"));
}
