// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13 sync hardening through S3-compatible storage and WebDAV (in-memory
//! storage; the transport is the same for both): compaction, removing a
//! device, and vaults recorded by Quire 0.12.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use quire_sync::admin::{self, Removal};
use quire_sync::compact::Policy;
use quire_sync::folder::Compacted;
use quire_sync::keyring::{DeviceSecret, KeyError};
use quire_sync::remote::{self, MemoryObjects, ObjectStore, RemoteTransport};
use quire_sync::{DeviceId, Engine, KdfParams, KeySet, LocalStore, MemoryStore, Pins, SpaceId, SyncState, VaultKey, WrappedKey};

const TODAY: &str = "2026-11-12";
const ALL: Policy = Policy { keep_days: 0, min_versions: 1 };

struct Dev {
    id: DeviceId,
    secret: DeviceSecret,
    keys: KeySet,
    pins: Pins,
    engine: Engine,
    local: MemoryStore,
    t: RemoteTransport,
    state: tempfile::TempDir,
}

impl Dev {
    fn new(store: &Arc<MemoryObjects>, space: &SpaceId, keys: &KeySet, name: &str) -> Self {
        let state = tempfile::tempdir().unwrap();
        let id = DeviceId::random().unwrap();
        let t = RemoteTransport::new(Arc::clone(store) as Arc<dyn ObjectStore>, space, id.clone(), Some(state.path()))
            .unwrap()
            .with_keys(keys.clone())
            .with_settle(0);
        Self {
            engine: Engine::new(space.clone(), id.clone(), name, keys.clone(), SyncState::default()),
            id,
            secret: DeviceSecret::generate().unwrap(),
            keys: keys.clone(),
            pins: Pins::default(),
            local: MemoryStore::default(),
            t,
            state,
        }
    }

    fn sync(&mut self) {
        self.engine.sync(&mut self.local, &self.t, TODAY).unwrap();
        self.t.flush().unwrap();
    }

    fn write(&mut self, p: &str, t: &str) {
        self.local.write(p, t).unwrap();
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

fn hardened() -> (Arc<MemoryObjects>, SpaceId, KeySet) {
    let store = Arc::new(MemoryObjects::new());
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    let wrapped = WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap();
    remote::create_space_at(&*store, &space, &wrapped, remote::LEVEL_HARDENED).unwrap();
    (store, space, KeySet::new(&key))
}

fn objects_of(store: &MemoryObjects, space: &SpaceId, dev: &DeviceId) -> usize {
    store
        .keys()
        .iter()
        .filter(|k| k.contains(&format!("{space}/devices/{dev}/")) && Path::new(k.as_str()).extension().is_some_and(|e| e == "rev"))
        .count()
}

#[test]
fn compaction_in_storage_frees_old_objects_and_an_offline_device_catches_up() {
    let (store, space, keys) = hardened();
    let (mut a, mut b) = (Dev::new(&store, &space, &keys, "a"), Dev::new(&store, &space, &keys, "b"));
    a.write("Shared.md", "top\n\nmiddle\n\nbottom\n");
    a.sync();
    let mut c = Dev::new(&store, &space, &keys, "c");
    c.sync();
    c.write("Shared.md", "top\n\nmiddle\n\nbottom\nfrom c, offline\n");
    for i in 0..20 {
        a.write("Shared.md", &format!("top {i}\n\nmiddle\n\nbottom\n"));
        a.sync();
        b.write("B.md", &format!("b {i}\n"));
        b.sync();
    }
    a.sync();
    let before = objects_of(&store, &space, &a.id);
    assert!(before >= 21);
    let Compacted::Done(m) = a.t.compact(ALL, true).unwrap() else { panic!("no snapshot") };
    assert!(m.stats.dropped >= 30, "{:?}", m.stats);
    assert!(store.keys().iter().any(|k| k.ends_with(&format!("snapshots/{}/manifest", m.name()))));
    // B notices it through A's tip, and once settled each device deletes
    // its own old objects (a new session: once per session).
    b.sync();
    assert_eq!(b.t.generation().unwrap().0.name(), m.name());
    // A new session of A (the old one ends first).
    a.t = RemoteTransport::new(Arc::clone(&store) as Arc<dyn ObjectStore>, &space, a.id.clone(), Some(a.state.path()))
        .unwrap()
        .with_keys(keys.clone())
        .with_settle(0);
    a.sync();
    assert!(objects_of(&store, &space, &a.id) < before, "old objects deleted");
    // C, offline all along, catches up and keeps its line.
    c.sync();
    assert_eq!(c.local.read("Shared.md").unwrap(), "top 19\n\nmiddle\n\nbottom\nfrom c, offline\n");
    assert_eq!(c.local.read("B.md").unwrap(), "b 19\n");
    // A newcomer gets everything from the snapshot and what came after.
    let mut d = Dev::new(&store, &space, &keys, "d");
    d.sync();
    a.sync();
    assert_eq!(d.local.read("Shared.md"), c.local.read("Shared.md"));
    assert_eq!(d.local.read("B.md").unwrap(), "b 19\n");
}

#[test]
fn concurrent_compaction_in_storage_leaves_one_winner() {
    let (store, space, keys) = hardened();
    let (mut a, mut b) = (Dev::new(&store, &space, &keys, "a"), Dev::new(&store, &space, &keys, "b"));
    a.t = RemoteTransport::new(Arc::clone(&store) as Arc<dyn ObjectStore>, &space, a.id.clone(), Some(a.state.path()))
        .unwrap()
        .with_keys(keys.clone())
        .with_settle(3600);
    for i in 0..5 {
        a.write("A.md", &format!("{i}\n"));
        a.sync();
        b.write("B.md", &format!("{i}\n"));
        b.sync();
    }
    a.sync();
    b.sync();
    // Both compact before either sees the other's snapshot (A's is held
    // back, and its tip shows the old state, as on a slow storage).
    let tip_key = format!("quire-sync/{space}/tips/{}", a.id);
    let old_tip = store.get(&tip_key, 1 << 20).unwrap();
    let Compacted::Done(ma) = a.t.compact(ALL, true).unwrap() else { panic!() };
    let held: Vec<(String, Vec<u8>)> = store.dump().into_iter().filter(|(k, _)| k.contains("/snapshots/")).collect();
    let new_tip = store.get(&tip_key, 1 << 20).unwrap();
    for (k, _) in &held {
        store.set_raw(k, None);
    }
    store.set_raw(&tip_key, old_tip);
    let Compacted::Done(mb) = b.t.compact(ALL, true).unwrap() else { panic!("B's own") };
    for (k, v) in held {
        store.set_raw(&k, Some(v));
    }
    store.set_raw(&tip_key, new_tip);
    a.sync();
    b.sync();
    a.sync();
    let winner = if a.id < b.id { ma.name() } else { mb.name() };
    assert_eq!(a.t.generation().unwrap().0.name(), winner);
    assert_eq!(b.t.generation().unwrap().0.name(), winner);
    let mut c = Dev::new(&store, &space, &keys, "c");
    c.sync();
    for d in [&a, &b, &c] {
        assert_eq!((d.local.read("A.md").unwrap(), d.local.read("B.md").unwrap()), ("4\n".to_string(), "4\n".to_string()));
    }
    // Changes after the race still flow both ways.
    a.write("A.md", "after\n");
    a.sync();
    b.write("B.md", "after\n");
    b.sync();
    a.sync();
    c.sync();
    assert_eq!((c.local.read("A.md").unwrap(), c.local.read("B.md").unwrap()), ("after\n".to_string(), "after\n".to_string()));
    // A third device trying now waits for the race to settle.
    assert!(matches!(c.t.compact(ALL, true).unwrap(), Compacted::Settling { .. } | Compacted::Done(_)));
}

#[test]
fn a_removed_device_cant_read_new_objects_or_get_its_own_taken() {
    let (store, space, keys) = hardened();
    let (mut a, mut b, mut r) =
        (Dev::new(&store, &space, &keys, "a"), Dev::new(&store, &space, &keys, "b"), Dev::new(&store, &space, &keys, "r"));
    for d in [&a, &b, &r] {
        admin::publish_self(&d.t, &space, &d.id, &d.secret, "x", &d.keys, &BTreeMap::new()).unwrap();
    }
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.refresh(&space).is_none());
    }
    a.write("Old.md", "before\n");
    a.sync();
    b.sync();
    r.sync();
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
    assert!(a.refresh(&space).is_none());
    a.write("New.md", "secret after removal\n");
    a.sync();
    // B hears of the key change through A's tip.
    b.sync();
    assert!(b.t.take_key_news());
    assert!(b.refresh(&space).is_none());
    b.sync();
    assert_eq!(b.local.read("New.md").unwrap(), "secret after removal\n");
    assert_eq!(r.refresh(&space), Some(KeyError::Removed));
    let _ = r.engine.sync(&mut r.local, &r.t, TODAY);
    assert!(r.local.read("New.md").is_none(), "the removed device can't open what came after");
    assert_eq!(r.local.read("Old.md").unwrap(), "before\n");
    // What it uploads now is ignored.
    r.write("Planted.md", "planted\n");
    let _ = r.engine.sync(&mut r.local, &r.t, TODAY);
    let _ = r.t.flush();
    a.sync();
    b.sync();
    assert!(a.local.read("Planted.md").is_none() && b.local.read("Planted.md").is_none());
}

/// Objects as Quire 0.12 uploaded them (bound `v2` headers), recorded.
#[test]
fn a_recorded_012_storage_is_read_by_013_and_upgraded() {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote-0.12.json");
    let dump: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(fx).unwrap()).unwrap();
    let store = Arc::new(MemoryObjects::new());
    let unhex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
    for (k, v) in &dump {
        store.set_raw(k, Some(unhex(v)));
    }
    let spaces = remote::list_spaces(&*store).unwrap();
    let space = spaces[0].space.clone();
    let wrapped = remote::wrapped_key(&*store, &space).unwrap();
    let keys = quire_sync::keyring::join_with_passphrase(&wrapped, &[], "fixture pass").unwrap();
    let mut d = Dev::new(&store, &space, &keys, "0.13");
    d.sync();
    assert_eq!(d.local.read("Remote.md").unwrap(), "remote one\nremote two\n");
    assert!(d.t.level() < remote::LEVEL_HARDENED);
    admin::upgrade(&d.t).unwrap();
    // What 0.12 checks: min_reader at most 3.
    let sj: serde_json::Value =
        serde_json::from_slice(&store.get(&format!("quire-sync/{space}/space.json"), 1 << 20).unwrap().unwrap()).unwrap();
    assert!(sj["min_reader"].as_u64().unwrap() > 3);
    let mut d2 = Dev::new(&store, &space, &keys, "another 0.13");
    d2.sync();
    d2.write("Remote.md", "remote one\nremote two\nthree\n");
    d2.sync();
    d.sync();
    assert_eq!(d.local.read("Remote.md").unwrap(), "remote one\nremote two\nthree\n");
}
