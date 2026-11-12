// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What a removed device that still has the old key and write access to the
//! storage can try (`docs/design/0.13-sync.md` §2, "What a removed device can
//! still do"), and that each attempt now fails: lowering the vault's level,
//! making up a device to win a key change, writing into another device's
//! lane, and planting a snapshot.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quire_sync::admin::{self, Removal};
use quire_sync::compact::{self, Manifest, Part, Stats};
use quire_sync::folder::{self, FolderTransport};
use quire_sync::keyring::{DeviceSecret, KeyError, Removed};
use quire_sync::remote::{MemoryObjects, ObjectStore, RemoteTransport};
use quire_sync::{
    Content, DeviceId, DeviceRecord, Engine, EpochRecord, FileId, KdfParams, KeySet, LanePos, LocalStore, MemoryStore, Pins, RevId,
    Revision, SpaceId, SyncState, VaultKey, WrappedKey,
};

const TODAY: &str = "2026-11-13";

struct Vault {
    dir: tempfile::TempDir,
    root: PathBuf,
    space: SpaceId,
    key: VaultKey,
}

struct Dev {
    id: DeviceId,
    name: String,
    secret: DeviceSecret,
    keys: KeySet,
    pins: Pins,
    engine: Engine,
    store: MemoryStore,
    t: FolderTransport,
}

impl Vault {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = folder::sync_root(&dir.path().join("cloud"));
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        let wrapped = WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap();
        folder::create_space_at(&root, &space, &wrapped, folder::LEVEL_HARDENED).unwrap();
        Self { dir, root, space, key }
    }

    fn device(&self, name: &str) -> Dev {
        let id = DeviceId::random().unwrap();
        let state = self.dir.path().join(format!("state-{name}"));
        fs::create_dir_all(&state).unwrap();
        let keys = KeySet::new(&self.key);
        Dev {
            engine: Engine::new(self.space.clone(), id.clone(), name, keys.clone(), SyncState::default()),
            t: FolderTransport::new(&self.root, &self.space, id.clone(), Some(&state)).with_keys(keys.clone()).with_settle(0),
            id,
            name: name.into(),
            secret: DeviceSecret::generate().unwrap(),
            keys,
            pins: Pins::default(),
            store: MemoryStore::default(),
        }
    }

    fn lane(&self, d: &DeviceId) -> PathBuf {
        self.root.join(&self.space.0).join("devices").join(&d.0)
    }

    /// Every revision in the folder that the first key opens, as text.
    fn readable_with_first_key(&self) -> Vec<String> {
        let k0 = KeySet::new(&self.key);
        let mut out = Vec::new();
        for f in walk(&self.root.join(&self.space.0).join("devices")) {
            if f.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            for line in fs::read_to_string(&f).unwrap().lines() {
                if let Ok(r) = serde_json::from_str::<Revision>(line)
                    && let Ok(c) = Content::open_set(&k0, &self.space, &r, true)
                {
                    out.push(c.text);
                }
            }
        }
        out
    }
}

impl Dev {
    /// The start of an app pass: publish this device's record, take key
    /// changes, and hand removals and rules to the transport.
    fn pass(&mut self, space: &SpaceId) -> Option<KeyError> {
        self.pass_trusting(space, &BTreeMap::new())
    }

    fn pass_trusting(&mut self, space: &SpaceId, trust: &BTreeMap<u32, DeviceId>) -> Option<KeyError> {
        // As the app: a record that can't be published (another computer
        // writes as this one) is left for the next pass.
        let _ = admin::publish_self(&self.t, space, &self.id, &self.secret, &self.name, &self.keys, &BTreeMap::new());
        let before = self.keys.current();
        let r = admin::refresh_keys_trusting(&self.t, space, &self.id, Some(&self.secret), &self.keys, &mut self.pins, trust).unwrap();
        self.keys = r.keys.clone();
        self.engine.set_keys(r.keys.clone());
        self.t.set_removed(r.removed.iter().map(|(d, x)| (d.clone(), x.lanes.get("").copied().unwrap_or_default())).collect());
        self.t.set_rules(r.rules(""));
        if self.keys.current() > before {
            let _ = admin::publish_self(&self.t, space, &self.id, &self.secret, &self.name, &self.keys, &BTreeMap::new());
        }
        r.stuck.map(|(_, why)| why)
    }

    fn sync(&mut self) {
        self.engine.sync(&mut self.store, &self.t, TODAY).unwrap();
    }

    fn write(&mut self, path: &str, text: &str) {
        self.store.write(path, text).unwrap();
    }

    fn text(&self, path: &str) -> Option<String> {
        self.store.read(path)
    }

    /// Remove `target`, as Settings → Sync → Devices does.
    fn remove(&mut self, space: &SpaceId, target: &DeviceId) -> VaultKey {
        let ends = self.t.lane_ends().unwrap();
        let removal = Removal {
            space,
            me: &self.id,
            secret: &self.secret,
            keys: &self.keys,
            target,
            passphrase: "a new pass phrase",
            kdf: KdfParams::TEST,
            lanes: BTreeMap::from([(String::new(), ends.get(target).copied().unwrap_or_default())]),
            removed: &BTreeSet::new(),
            seen: BTreeMap::new(),
            snapshots: Vec::new(),
        };
        admin::remove_device(&self.t, &removal, &mut self.pins).unwrap().key
    }
}

/// A device of an S3 or WebDAV vault.
struct RemoteDev {
    id: DeviceId,
    secret: DeviceSecret,
    keys: KeySet,
    pins: Pins,
    engine: Engine,
    local: MemoryStore,
    t: RemoteTransport,
    _state: tempfile::TempDir,
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// A revision written as `device`, sealed the 0.12 way with the first key
/// (what a removed device can still make).
fn forged(space: &SpaceId, key: &VaultKey, device: &DeviceId, path: &str, text: &str) -> Revision {
    let (file, rev) = (FileId::random().unwrap(), RevId::random().unwrap());
    let c = Content { path: path.into(), deleted: false, text: text.into(), from: "rogue".into(), at: 1, actor: None };
    let sealed = c.seal(KeySet::new(key).current_keys(), space, &file, &rev).unwrap();
    Revision { file, rev, parents: vec![], seq: 0, device: device.clone(), sealed, chain: vec![] }
}

/// Three devices that have synced and know each other; A has removed R
/// and both A and B took the new key.
fn removed_r() -> (Vault, Dev, Dev, Dev, VaultKey) {
    let v = Vault::new();
    let (mut a, mut b, mut r) = (v.device("a"), v.device("b"), v.device("r"));
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.pass(&v.space).is_none());
    }
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.pass(&v.space).is_none());
    }
    a.write("Old.md", "everyone saw this\n");
    a.sync();
    b.sync();
    r.sync();
    let k1 = a.remove(&v.space, &r.id);
    assert!(a.pass(&v.space).is_none());
    assert!(b.pass(&v.space).is_none());
    assert_eq!(b.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
    (v, a, b, r, k1)
}

/// Security review H1: the removed device lowers `space.json`; nothing new
/// is sealed with the first key, and sync says why it stopped.
#[test]
fn a_lowered_vault_level_never_makes_anyone_seal_with_the_old_key() {
    let (v, _a, mut b, _r, _) = removed_r();
    let sj = v.root.join(&v.space.0).join("space.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&sj).unwrap()).unwrap();
    json["min_reader"] = 1.into();
    json["format"] = 1.into();
    fs::write(&sj, serde_json::to_vec(&json).unwrap()).unwrap();
    // The app keeps the highest level it saw: the pass stops, saying why.
    b.t.set_floor(folder::LEVEL_HARDENED);
    b.write("Secret.md", "written after the removal\n");
    let e = b.engine.sync(&mut b.store, &b.t, TODAY).unwrap_err().to_string();
    assert!(e.contains("lowered"), "{e}");
    // Even without that floor, nothing is sealed the old way once the key changed.
    let state = v.dir.path().join("state-b2");
    fs::create_dir_all(&state).unwrap();
    let plain = FolderTransport::new(&v.root, &v.space, b.id.clone(), Some(&state)).with_keys(b.keys.clone());
    let e = b.engine.sync(&mut b.store, &plain, TODAY).unwrap_err().to_string();
    assert!(e.contains("lowered"), "{e}");
    assert!(!v.readable_with_first_key().iter().any(|t| t.contains("after the removal")));
}

/// Security review H2: the removed device makes up a device M, publishes a
/// record for it under the first key and makes M remove A. B, offline
/// until now, takes A's key, not the removed device's.
#[test]
fn a_made_up_device_cant_win_a_key_change() {
    let v = Vault::new();
    let (mut a, mut b, mut r) = (v.device("a"), v.device("b"), v.device("r"));
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.pass(&v.space).is_none());
    }
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.pass(&v.space).is_none());
    }
    let k1 = a.remove(&v.space, &r.id);
    // The removed device, with the first key and folder access.
    let k0 = KeySet::new(&v.key);
    let (m, sm) = (DeviceId::random().unwrap(), DeviceSecret::generate().unwrap());
    let mt = FolderTransport::new(&v.root, &v.space, m.clone(), None);
    mt.put_device_record(&DeviceRecord::make(&v.space, &m, &sm, "made up", &k0).unwrap()).unwrap();
    let (rogue, rogue_key) = EpochRecord::make(
        &v.space,
        &k0,
        &m,
        &sm,
        &[(b.id.clone(), b.secret.public()), (m.clone(), sm.public())],
        vec![Removed { device: a.id.clone(), lanes: BTreeMap::new() }],
        "p",
        KdfParams::TEST,
    )
    .unwrap();
    mt.put_key_record(&rogue).unwrap();
    assert!(b.pass(&v.space).is_none());
    assert_eq!(b.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
    assert_ne!(b.keys.raw(1).unwrap().as_bytes(), rogue_key.as_bytes());
    assert!(b.pins.get(&m).is_none(), "no device is pinned from an old-key record once a key change is waiting");
}

/// Security review H2, without a made-up device: the removed device removes
/// the device that removed it. Nobody takes either key until the person
/// chooses; then the chosen one.
#[test]
fn two_devices_removing_each_other_wait_for_the_person() {
    let v = Vault::new();
    let (mut a, mut b, mut r) = (v.device("a"), v.device("b"), v.device("r"));
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.pass(&v.space).is_none());
    }
    for d in [&mut a, &mut b, &mut r] {
        assert!(d.pass(&v.space).is_none());
    }
    let k1 = a.remove(&v.space, &r.id);
    let k0 = KeySet::new(&v.key);
    let (rogue, _) = EpochRecord::make(
        &v.space,
        &k0,
        &r.id,
        &r.secret,
        &[(b.id.clone(), b.secret.public()), (r.id.clone(), r.secret.public())],
        vec![Removed { device: a.id.clone(), lanes: BTreeMap::new() }],
        "p",
        KdfParams::TEST,
    )
    .unwrap();
    r.t.put_key_record(&rogue).unwrap();
    assert!(matches!(b.pass(&v.space), Some(KeyError::Conflict(_))));
    assert_eq!(b.keys.current(), 0);
    let trust = BTreeMap::from([(1, a.id.clone())]);
    assert!(b.pass_trusting(&v.space, &trust).is_none());
    assert_eq!(b.keys.raw(1).unwrap().as_bytes(), k1.as_bytes());
}

/// Security review H3: after its removal, R appends a line sealed with the
/// first key to B's lane, as B. Nobody takes it, and B notices. A device
/// that was offline during the removal still gets what it wrote then
/// taken, once it vouches for it; one whose lane R wrote into meanwhile
/// notices and doesn't vouch.
#[test]
fn lines_written_into_another_devices_lane_with_the_old_key_are_refused() {
    let v = Vault::new();
    let (mut a, mut b, mut c, mut d, mut r) = (v.device("a"), v.device("b"), v.device("c"), v.device("d"), v.device("r"));
    for _ in 0..2 {
        for x in [&mut a, &mut b, &mut c, &mut d, &mut r] {
            assert!(x.pass(&v.space).is_none());
        }
    }
    for x in [&mut a, &mut b, &mut c, &mut d] {
        x.write(&format!("{}.md", x.name), "mine\n");
        x.sync();
    }
    a.remove(&v.space, &r.id);
    assert!(a.pass(&v.space).is_none());
    assert!(b.pass(&v.space).is_none());
    // C and D are offline: they write under the old key, as they always did.
    c.write("FromC.md", "written while offline\n");
    c.sync();
    d.write("FromD.md", "written while offline\n");
    d.sync();
    let append = |dev: &DeviceId, path: &str| {
        let seg = walk(&v.lane(dev)).into_iter().rfind(|p| p.extension().is_some_and(|e| e == "jsonl")).unwrap();
        let line = serde_json::to_string(&forged(&v.space, &v.key, dev, path, "planted\n")).unwrap();
        let mut text = fs::read_to_string(&seg).unwrap();
        text.push_str(&line);
        text.push('\n');
        fs::write(&seg, text).unwrap();
    };
    // R writes as B (which took the new key) and as C (which hasn't).
    append(&b.id, "Injected.md");
    append(&c.id, "AlsoInjected.md");
    assert!(a.pass(&v.space).is_none());
    a.sync();
    assert!(a.text("Injected.md").is_none());
    assert!(a.text("AlsoInjected.md").is_none());
    assert!(a.text("FromD.md").is_none(), "waits until D vouches for it");
    assert!(a.t.waiting().is_some_and(|w| w.contains("old key")));
    // B sees a line in its lane it never wrote, even after a restart.
    let state = v.dir.path().join("state-b");
    b.t = FolderTransport::new(&v.root, &v.space, b.id.clone(), Some(&state)).with_keys(b.keys.clone()).with_settle(0);
    let _ = b.engine.sync(&mut b.store, &b.t, TODAY);
    assert!(b.t.problem().is_some());
    assert!(b.text("Injected.md").is_none());
    // D comes back, takes the new key and vouches for its lane: A takes
    // what D wrote offline.
    assert!(d.pass(&v.space).is_none());
    assert!(a.pass(&v.space).is_none());
    a.sync();
    assert_eq!(a.text("FromD.md").unwrap(), "written while offline\n");
    // C comes back to a line in its lane it never wrote: it says so, and
    // vouches for nothing, so neither its offline note nor R's line is taken.
    let state = v.dir.path().join("state-c");
    c.t = FolderTransport::new(&v.root, &v.space, c.id.clone(), Some(&state)).with_keys(c.keys.clone()).with_settle(0);
    let _ = c.engine.sync(&mut c.store, &c.t, TODAY);
    assert!(c.t.problem().is_some());
    assert!(c.text("AlsoInjected.md").is_none());
    assert!(c.pass(&v.space).is_none());
    assert_eq!(c.keys.current(), 1);
    assert!(a.pass(&v.space).is_none());
    a.sync();
    assert!(a.text("AlsoInjected.md").is_none() && a.text("FromC.md").is_none());
    // From now on, D's lane takes only lines under the new key.
    append(&d.id, "Later.md");
    a.sync();
    assert!(a.text("Later.md").is_none());
}

/// Security review H4: R plants a snapshot authenticated with the first
/// key, with one forged revision and a cut past every lane. Nobody takes
/// it, nobody's log is deleted, and sync goes on.
#[test]
fn a_planted_snapshot_is_never_taken() {
    let (v, mut a, mut b, r, _) = removed_r();
    let k0 = KeySet::new(&v.key);
    let ids = [a.id.clone(), b.id.clone(), r.id.clone()];
    let plant = |keys: &KeySet, generation: u32| {
        let name = compact::name(generation, &r.id);
        let dir = v.root.join(&v.space.0).join("snapshots").join(&name);
        fs::create_dir_all(&dir).unwrap();
        let mut part = serde_json::to_vec(&forged(&v.space, &v.key, &r.id, "Planted.md", "planted\n")).unwrap();
        part.push(b'\n');
        fs::write(dir.join("part-000001.jsonl"), &part).unwrap();
        let far = LanePos { seg: u32::MAX, line: u64::MAX, n: 0 };
        let mut m = Manifest {
            format: compact::FORMAT,
            space: v.space.clone(),
            lane: String::new(),
            generation,
            maker: r.id.clone(),
            made: 1,
            epoch: 0,
            cut: ids.iter().map(|d| (d.clone(), far)).collect(),
            parts: vec![Part { name: "part-000001.jsonl".into(), size: part.len() as u64, sha256: compact::sha256_hex(&part), lines: 1 }],
            starts: BTreeMap::new(),
            links: Vec::new(),
            stats: Stats::default(),
            mac: String::new(),
        };
        m.seal(keys);
        fs::write(dir.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
    };
    plant(&k0, 999_999);
    let a_files = walk(&v.lane(&a.id));
    a.sync();
    b.sync();
    assert!(a.text("Planted.md").is_none() && b.text("Planted.md").is_none());
    assert!(a.t.generation().is_none());
    assert_eq!(a.t.tidy(), 0);
    assert_eq!(walk(&v.lane(&a.id)), a_files);
    // Even under the current key (a device with it gone wrong), a cut past
    // what's in the folder isn't taken.
    plant(&a.keys, 999_998);
    a.sync();
    assert!(a.t.generation().is_none() && a.text("Planted.md").is_none());
    b.write("After.md", "still syncing\n");
    b.sync();
    a.sync();
    assert_eq!(a.text("After.md").unwrap(), "still syncing\n");
}

/// Security review H3 on S3 and WebDAV: an object uploaded as B with the
/// first key after the removal (and B's tip moved on to name it) isn't taken.
#[test]
fn objects_uploaded_as_another_device_with_the_old_key_are_refused() {
    let store = Arc::new(MemoryObjects::new());
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    quire_sync::remote::create_space_at(
        &*store,
        &space,
        &WrappedKey::wrap(&key, "p", KdfParams::TEST).unwrap(),
        quire_sync::remote::LEVEL_HARDENED,
    )
    .unwrap();
    let dev = |name: &str| {
        let state = tempfile::tempdir().unwrap();
        let id = DeviceId::random().unwrap();
        let keys = KeySet::new(&key);
        RemoteDev {
            t: RemoteTransport::new(Arc::clone(&store) as Arc<dyn ObjectStore>, &space, id.clone(), Some(state.path()))
                .unwrap()
                .with_keys(keys.clone()),
            engine: Engine::new(space.clone(), id.clone(), name, keys.clone(), SyncState::default()),
            id,
            secret: DeviceSecret::generate().unwrap(),
            keys,
            pins: Pins::default(),
            local: MemoryStore::default(),
            _state: state,
        }
    };
    let pass = |d: &mut RemoteDev| {
        admin::publish_self(&d.t, &space, &d.id, &d.secret, "x", &d.keys, &BTreeMap::new()).unwrap();
        let before = d.keys.current();
        let r = admin::refresh_keys(&d.t, &space, &d.id, Some(&d.secret), &d.keys, &mut d.pins).unwrap();
        d.keys = r.keys.clone();
        d.engine.set_keys(r.keys.clone());
        d.t.set_removed(r.removed.iter().map(|(x, y)| (x.clone(), y.lanes.get("").copied().unwrap_or_default())).collect());
        d.t.set_rules(r.rules(""));
        if d.keys.current() > before {
            admin::publish_self(&d.t, &space, &d.id, &d.secret, "x", &d.keys, &BTreeMap::new()).unwrap();
        }
        assert!(r.stuck.is_none());
    };
    let sync = |d: &mut RemoteDev| {
        d.engine.sync(&mut d.local, &d.t, TODAY).unwrap();
        d.t.flush().unwrap();
    };
    let (mut a, mut b, mut r) = (dev("a"), dev("b"), dev("r"));
    for _ in 0..2 {
        for d in [&mut a, &mut b, &mut r] {
            pass(d);
        }
    }
    b.local.write("B.md", "b\n").unwrap();
    sync(&mut b);
    sync(&mut a);
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
    pass(&mut a);
    pass(&mut b);
    // R uploads object n+1 as B and moves B's tip on to it.
    let tip_key = format!("quire-sync/{space}/tips/{}", b.id);
    let mut tip: serde_json::Value = serde_json::from_slice(&store.get(&tip_key, 1 << 20).unwrap().unwrap()).unwrap();
    let n = tip["n"].as_u64().unwrap() + 1;
    let obj = quire_sync::remote::encode_object(&forged(&space, &key, &b.id, "Injected.md", "planted\n"));
    store.put(&format!("quire-sync/{space}/devices/{}/{n:012}.rev", b.id), &obj, false).unwrap();
    tip["n"] = n.into();
    store.put(&tip_key, &serde_json::to_vec(&tip).unwrap(), false).unwrap();
    pass(&mut a);
    sync(&mut a);
    assert_eq!(a.local.read("B.md").unwrap(), "b\n");
    assert!(a.local.read("Injected.md").is_none());
}
