// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13 on the server (`docs/design/0.13-sync.md`): device tokens, removing
//! a device, key changes by compare-and-swap, bound headers once the space
//! is upgraded, Quire 0.12 clients kept out with a clear error, and
//! compaction with a device that was offline.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use quire_sync::admin::{self, Removal, SpaceAdmin};
use quire_sync::keyring::{DeviceSecret, KeyError};
use quire_sync::{
    DeviceId, Engine, HttpTransport, KdfParams, KeySet, LocalStore, MemoryStore, Pins, SpaceId, SyncState, VaultKey, WrappedKey,
};
use quire_sync_server::{AppState, Store, app};

const TOKEN: &str = "test-token-0123456789abcdef";
const TODAY: &str = "2026-11-12";

fn serve() -> (String, Arc<AppState>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let st = Arc::new(AppState::new(Store::in_memory().unwrap(), TOKEN));
    let st2 = Arc::clone(&st);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app(st2)).await.unwrap();
        });
    });
    (format!("http://{}", rx.recv().unwrap()), st)
}

struct Dev {
    id: DeviceId,
    secret: DeviceSecret,
    keys: KeySet,
    pins: Pins,
    engine: Engine,
    local: MemoryStore,
    t: HttpTransport,
}

impl Dev {
    /// A 0.13 device: its own token.
    fn new(base: &str, space: &SpaceId, key: &VaultKey, name: &str) -> Self {
        let id = DeviceId::random().unwrap();
        let token = HttpTransport::enroll(base, space, TOKEN, &id).unwrap();
        let keys = KeySet::new(key);
        let d = Self {
            engine: Engine::new(space.clone(), id.clone(), name, keys.clone(), SyncState::default()),
            t: HttpTransport::new(base, space.clone(), &token),
            id,
            secret: DeviceSecret::generate().unwrap(),
            keys,
            pins: Pins::default(),
            local: MemoryStore::default(),
        };
        admin::publish_self(&d.t, space, &d.id, &d.secret, name, &d.keys, &BTreeMap::new()).unwrap();
        d
    }

    fn sync(&mut self) -> Result<quire_sync::Report, quire_sync::SyncError> {
        self.engine.sync(&mut self.local, &self.t, TODAY)
    }

    fn refresh(&mut self, space: &SpaceId) -> Option<KeyError> {
        let r = admin::refresh_keys(&self.t, space, &self.id, Some(&self.secret), &self.keys, &mut self.pins).unwrap();
        self.keys = r.keys.clone();
        self.engine.set_keys(r.keys.clone());
        self.t.set_rules(r.rules(""));
        self.t.set_removed(r.removed.iter().map(|(d, x)| (d.clone(), x.lanes.get("").copied().unwrap_or_default())).collect());
        r.stuck.map(|(_, w)| w)
    }
}

fn space(base: &str) -> (SpaceId, VaultKey) {
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    HttpTransport::new(base, space.clone(), TOKEN).create_space(&WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap()).unwrap();
    (space, key)
}

#[test]
fn upgrading_binds_headers_takes_device_tokens_only_and_tells_old_clients_to_update() {
    let (base, _st) = serve();
    let (space, key) = space(&base);
    let mut a = Dev::new(&base, &space, &key, "a");
    // A 0.12 device (protocol 2, the server's token) syncs alongside until the upgrade.
    let old_t = HttpTransport::new(&base, space.clone(), TOKEN).with_protocol(2);
    let mut old = Engine::new(space.clone(), DeviceId::random().unwrap(), "0.12", key.keys(), SyncState::default());
    let mut old_local = MemoryStore::default();
    old_local.write("Old.md", "from 0.12\n").unwrap();
    old.sync(&mut old_local, &old_t, TODAY).unwrap();
    a.sync().unwrap();
    assert_eq!(a.local.read("Old.md").unwrap(), "from 0.12\n");
    let listed = admin::old_devices(&a.t, &space, &a.id, &a.keys).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    admin::upgrade(&a.t).unwrap();
    assert_eq!(a.t.level().unwrap(), admin::HARDENED);
    // The 0.12 client: a clear "update Quire", never a silent mix.
    let e = old.sync(&mut old_local, &old_t, TODAY).unwrap_err().to_string();
    assert!(e.contains("Quire 0.13"), "{e}");
    // The server's own token no longer reads the space's revisions.
    let e = HttpTransport::new(&base, space.clone(), TOKEN).wrapped_key().unwrap_err().to_string();
    assert!(e.contains("device tokens"), "{e}");
    // New revisions bind their header: a server rewriting `parents` would
    // make them fail to open.
    a.local.write("New.md", "bound\n").unwrap();
    a.sync().unwrap();
    let mut b = Dev::new(&base, &space, &key, "b");
    b.sync().unwrap();
    assert_eq!(b.local.read("New.md").unwrap(), "bound\n");
    let hist = b.engine.history(&b.t, "New.md").unwrap();
    assert_eq!(hist.len(), 1);
}

#[test]
fn removing_a_device_refuses_its_token_and_its_old_key() {
    let (base, _st) = serve();
    let (space, key) = space(&base);
    let (mut a, mut b, mut r) =
        (Dev::new(&base, &space, &key, "a"), Dev::new(&base, &space, &key, "b"), Dev::new(&base, &space, &key, "r"));
    admin::upgrade(&a.t).unwrap();
    a.local.write("Old.md", "before\n").unwrap();
    a.sync().unwrap();
    b.sync().unwrap();
    r.sync().unwrap();
    // Each device's passes pin the others' keys before any key change.
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.refresh(&space).is_none());
    }
    let ends = a.t.lane_ends().unwrap();
    let removal = Removal {
        space: &space,
        me: &a.id,
        secret: &a.secret,
        keys: &a.keys,
        target: &r.id,
        passphrase: "new pass",
        kdf: KdfParams::TEST,
        lanes: BTreeMap::from([(String::new(), ends.get(&r.id).copied().unwrap_or_default())]),
        removed: &BTreeSet::new(),
        seen: BTreeMap::new(),
        snapshots: Vec::new(),
    };
    admin::remove_device(&a.t, &removal, &mut a.pins).unwrap();
    // B writes before it heard of the new key: refused, then fine after.
    b.local.write("FromB.md", "b\n").unwrap();
    let e = b.sync().unwrap_err().to_string();
    assert!(e.contains("old key"), "{e}");
    assert!(b.refresh(&space).is_none());
    b.sync().unwrap();
    assert!(a.refresh(&space).is_none());
    a.local.write("New.md", "after\n").unwrap();
    a.sync().unwrap();
    b.sync().unwrap();
    assert_eq!(b.local.read("New.md").unwrap(), "after\n");
    assert_eq!(a.local.read("FromB.md").unwrap(), "b\n");
    // The removed device: its token is refused outright.
    r.local.write("Planted.md", "x\n").unwrap();
    let e = r.sync().unwrap_err().to_string();
    assert!(e.contains("removed"), "{e}");
    assert!(r.local.read("New.md").is_none());
    assert_eq!(r.local.read("Old.md").unwrap(), "before\n");
    // Even re-made with the server's token, it can't get a new device token.
    assert!(HttpTransport::enroll(&base, &space, TOKEN, &r.id).is_err());
    // A device joining later: the new passphrase.
    let wrapped =
        HttpTransport::new(&base, space.clone(), &HttpTransport::enroll(&base, &space, TOKEN, &DeviceId::random().unwrap()).unwrap())
            .wrapped_key()
            .unwrap();
    let recs = a.t.key_records().unwrap();
    assert!(quire_sync::keyring::join_with_passphrase(&wrapped, &recs, "new pass").is_ok_and(|k| k.current() == 1));
}

#[test]
fn two_key_changes_at_once_one_wins() {
    let (base, _st) = serve();
    let (space, key) = space(&base);
    let (mut a, mut b, r) = (Dev::new(&base, &space, &key, "a"), Dev::new(&base, &space, &key, "b"), Dev::new(&base, &space, &key, "r"));
    admin::upgrade(&a.t).unwrap();
    for d in [&mut a, &mut b] {
        assert!(d.refresh(&space).is_none());
    }
    let empty = BTreeSet::new();
    let ra = Removal {
        space: &space,
        me: &a.id,
        secret: &a.secret,
        keys: &a.keys,
        target: &r.id,
        passphrase: "p",
        kdf: KdfParams::TEST,
        lanes: BTreeMap::new(),
        removed: &empty,
        seen: BTreeMap::new(),
        snapshots: Vec::new(),
    };
    admin::remove_device(&a.t, &ra, &mut a.pins).unwrap();
    // B made its record from epoch 0 too: the server takes only one epoch 1.
    let (rec, _) = quire_sync::EpochRecord::make(
        &space,
        &b.keys,
        &b.id,
        &b.secret,
        &[(b.id.clone(), b.secret.public())],
        vec![],
        "p",
        KdfParams::TEST,
    )
    .unwrap();
    let e = b.t.put_key_record(&rec).unwrap_err().to_string();
    assert!(e.contains("same moment"), "{e}");
    assert!(b.refresh(&space).is_none());
    assert_eq!(b.keys.current(), 1);
}

#[test]
fn compaction_on_the_server_and_a_device_that_was_offline() {
    let (base, st) = serve();
    let (space, key) = space(&base);
    let (mut a, mut c) = (Dev::new(&base, &space, &key, "a"), Dev::new(&base, &space, &key, "c"));
    a.local.write("N.md", "head\n\nbody\n\nfoot\n").unwrap();
    a.sync().unwrap();
    c.sync().unwrap();
    c.local.write("N.md", "head\n\nbody\n\nfoot\nfrom c\n").unwrap();
    for i in 0..15 {
        a.local.write("N.md", &format!("head {i}\n\nbody\n\nfoot\n")).unwrap();
        a.sync().unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let gone = st.store().compact(0, 2).unwrap();
    assert!(gone >= 13, "{gone}");
    assert_eq!(a.engine.history(&a.t, "N.md").unwrap().len(), 2);
    c.sync().unwrap();
    assert_eq!(c.local.read("N.md").unwrap(), "head 14\n\nbody\n\nfoot\nfrom c\n");
    a.sync().unwrap();
    assert_eq!(a.local.read("N.md"), c.local.read("N.md"));
    // A newcomer gets the current notes.
    let mut d = Dev::new(&base, &space, &key, "d");
    d.sync().unwrap();
    assert_eq!(d.local.read("N.md"), c.local.read("N.md"));
    // Data review #14: even keeping "1" version, a deleted note keeps its
    // last text (the version before its tombstone).
    a.local.write("Gone.md", "last words\n").unwrap();
    a.sync().unwrap();
    a.local.remove("Gone.md").unwrap();
    a.sync().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    st.store().compact(0, 1).unwrap();
    let h = a.engine.history(&a.t, "Gone.md").unwrap();
    assert!(h.iter().any(|v| v.text == "last words\n"), "{h:?}");
}

/// Security review M1: whoever holds the server's token can't enroll a
/// device that already has a token of its own (and so take it over).
#[test]
fn a_device_token_is_given_once() {
    let (base, _st) = serve();
    let (space, key) = space(&base);
    let mut b = Dev::new(&base, &space, &key, "b");
    b.local.write("B.md", "b\n").unwrap();
    b.sync().unwrap();
    let e = HttpTransport::enroll(&base, &space, TOKEN, &b.id).unwrap_err().to_string();
    assert!(e.contains("already"), "{e}");
    b.local.write("B.md", "b again\n").unwrap();
    b.sync().unwrap();
}
