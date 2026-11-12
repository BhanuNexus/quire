// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13 sync hardening through a synced folder (`docs/design/0.13-sync.md`):
//! upgraded vaults, compaction, removing a device, revisions lost for good.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use quire_sync::admin::{self, Removal};
use quire_sync::compact::Policy;
use quire_sync::folder::{self, Compacted, FolderTransport};
use quire_sync::keyring::{DeviceSecret, KeyError};
use quire_sync::{DeviceId, Engine, KdfParams, KeySet, LocalStore, MemoryStore, Pins, SpaceId, SyncState, Transport, VaultKey, WrappedKey};

const TODAY: &str = "2026-11-12";
/// Fold everything that isn't needed.
const ALL: Policy = Policy { keep_days: 0, min_versions: 1 };

struct Dev {
    id: DeviceId,
    name: String,
    secret: DeviceSecret,
    keys: KeySet,
    pins: Pins,
    engine: Engine,
    store: MemoryStore,
    t: FolderTransport,
    state: PathBuf,
}

struct Vault {
    dir: tempfile::TempDir,
    root: PathBuf,
    space: SpaceId,
    key: VaultKey,
}

impl Vault {
    fn new(level: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = folder::sync_root(&dir.path().join("cloud"));
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        let wrapped = WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap();
        folder::create_space_at(&root, &space, &wrapped, level).unwrap();
        Self { dir, root, space, key }
    }

    fn device(&self, name: &str) -> Dev {
        self.device_at(name, &self.root)
    }

    fn device_at(&self, name: &str, root: &Path) -> Dev {
        let id = DeviceId::random().unwrap();
        let state = self.dir.path().join(format!("state-{name}"));
        fs::create_dir_all(&state).unwrap();
        let keys = KeySet::new(&self.key);
        Dev {
            engine: Engine::new(self.space.clone(), id.clone(), name, keys.clone(), SyncState::default()),
            t: FolderTransport::new(root, &self.space, id.clone(), Some(&state)).with_keys(keys.clone()).with_settle(0),
            id,
            name: name.into(),
            secret: DeviceSecret::generate().unwrap(),
            keys,
            pins: Pins::default(),
            store: MemoryStore::default(),
            state,
        }
    }
}

impl Dev {
    fn sync(&mut self) {
        self.engine.sync(&mut self.store, &self.t, TODAY).unwrap();
    }

    fn write(&mut self, path: &str, text: &str) {
        self.store.write(path, text).unwrap();
    }

    fn text(&self, path: &str) -> Option<String> {
        self.store.read(path)
    }

    fn publish(&self, space: &SpaceId) {
        admin::publish_self(&self.t, space, &self.id, &self.secret, &self.name, &self.keys).unwrap();
    }

    /// Take key changes, as the app does at each pass.
    fn refresh(&mut self, space: &SpaceId) -> Option<KeyError> {
        let r = admin::refresh_keys(&self.t, space, &self.id, Some(&self.secret), &self.keys, &mut self.pins).unwrap();
        self.keys = r.keys.clone();
        self.engine.set_keys(r.keys);
        self.t.set_removed(r.removed.iter().map(|(d, x)| (d.clone(), x.lanes.get("").copied().unwrap_or_default())).collect());
        r.stuck.map(|(_, why)| why)
    }

    /// Lines of every log file, raw.
    fn lines(root: &Path, space: &SpaceId) -> Vec<String> {
        let mut out = Vec::new();
        for f in walk(&root.join(&space.0).join("devices")) {
            if f.extension().is_some_and(|e| e == "jsonl") {
                out.extend(fs::read_to_string(&f).unwrap().lines().map(str::to_string));
            }
        }
        out
    }
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

fn copy_tree(from: &Path, to: &Path) {
    for f in walk(from) {
        let rel = f.strip_prefix(from).unwrap();
        let dest = to.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(&f, &dest).unwrap();
    }
}

fn first_byte_of_sealed(line: &str) -> u8 {
    let r: quire_sync::Revision = serde_json::from_str(line).unwrap();
    r.sealed[0]
}

#[test]
fn an_upgraded_vault_binds_headers_keeps_old_readers_out_and_reads_old_revisions() {
    let v = Vault::new(1);
    let (mut a, mut b) = (v.device("laptop"), v.device("desktop"));
    a.write("Plan.md", "one\n");
    a.sync();
    b.sync();
    assert!(Dev::lines(&v.root, &v.space).iter().all(|l| first_byte_of_sealed(l) == 1), "an old vault keeps the old sealing");
    // B is a 0.13 device (it publishes a record); a third device that wrote
    // revisions never did: it's listed as old before the upgrade.
    b.publish(&v.space);
    let mut old = v.device("old phone");
    old.sync();
    old.write("Phone.md", "from 0.12\n");
    old.sync();
    let listed = admin::old_devices(&a.t, &v.space, &a.id, &a.keys).unwrap();
    assert_eq!(listed.iter().map(|d| d.id.clone()).collect::<Vec<_>>(), vec![old.id.clone()]);
    admin::upgrade(&a.t).unwrap();
    // What Quire 0.9–0.12 check in space.json: refused, with "newer Quire".
    let sj: serde_json::Value = serde_json::from_slice(&fs::read(v.root.join(&v.space.0).join("space.json")).unwrap()).unwrap();
    assert!(sj["min_reader"].as_u64().unwrap() > 2 && sj["format"].as_u64().unwrap() > 1);
    a.write("Plan.md", "one\ntwo\n");
    a.sync();
    b.sync();
    assert_eq!(b.text("Plan.md").unwrap(), "one\ntwo\n");
    assert_eq!(b.text("Phone.md").unwrap(), "from 0.12\n", "revisions sealed before the upgrade still read");
    let seg = walk(&v.root.join(&v.space.0).join("devices").join(&a.id.0)).into_iter().last().unwrap();
    let last = fs::read_to_string(&seg).unwrap().lines().last().unwrap().to_string();
    assert_eq!(first_byte_of_sealed(&last), 2, "new revisions are sealed with their whole header and epoch");
    // A storage that rewrites a bound header: the line is skipped, not used.
    let r: quire_sync::Revision = serde_json::from_str(&last).unwrap();
    let forged = quire_sync::Revision { parents: vec![], ..r };
    let mut text = fs::read_to_string(&seg).unwrap();
    text.push_str(&serde_json::to_string(&forged).unwrap());
    text.push('\n');
    fs::write(&seg, text).unwrap();
    let mut c = v.device("new");
    c.sync();
    assert_eq!(c.text("Plan.md").unwrap(), "one\ntwo\n");
}

/// A 0.12 sync folder, recorded: `space.json`, and one device's log with
/// three revisions of `Fixture.md` and one of `Other.md`, sealed the 0.12
/// way (`v1`). Passphrase `fixture pass` (test KDF settings).
#[test]
fn a_recorded_012_folder_is_read_by_013_before_and_after_upgrading() {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/folder-0.12");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Quire Sync");
    copy_tree(&fx, &root);
    let spaces = folder::list_spaces(&root).unwrap();
    assert_eq!(spaces.len(), 1);
    let space = spaces[0].space.clone();
    let wrapped = folder::wrapped_key(&root, &space).unwrap();
    let keys = quire_sync::keyring::join_with_passphrase(&wrapped, &[], "fixture pass").unwrap();
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let id = DeviceId::random().unwrap();
    let t = FolderTransport::new(&root, &space, id.clone(), Some(&state)).with_keys(keys.clone());
    let mut e = Engine::new(space, id, "0.13", keys, SyncState::default());
    let mut store = MemoryStore::default();
    e.sync(&mut store, &t, TODAY).unwrap();
    assert_eq!(store.read("Fixture.md").unwrap(), "first\nsecond\nthird\n");
    assert_eq!(store.read("Other.md").unwrap(), "other note\n");
    admin::upgrade(&t).unwrap();
    store.write("Fixture.md", "first\nsecond\nthird\nfourth\n").unwrap();
    e.sync(&mut store, &t, TODAY).unwrap();
    let hist = e.history(&t, "Fixture.md").unwrap();
    assert_eq!(hist.len(), 4, "the 0.12 versions and the new one");
}

#[test]
fn a_long_offline_device_catches_up_after_compaction_and_keeps_its_edits() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let (mut a, mut b) = (v.device("a"), v.device("b"));
    a.write("Shared.md", "intro\n\nmiddle\n\nend\n");
    a.write("Quiet.md", "untouched\n");
    a.sync();
    // C syncs once, then goes offline for months with an edit of its own.
    let mut c = v.device("c");
    c.sync();
    c.write("Shared.md", "intro\n\nmiddle\n\nend\nadded offline\n");
    for i in 0..30 {
        a.write("Shared.md", &format!("intro {i}\n\nmiddle\n\nend\n"));
        a.sync();
        b.sync();
        b.write(&format!("B{i}.md"), &format!("note {i}\n"));
        b.sync();
    }
    a.sync();
    let before = Dev::lines(&v.root, &v.space).len();
    let Compacted::Done(m) = a.t.compact(ALL, None, true).unwrap() else { panic!("no snapshot") };
    assert!(m.stats.dropped > 20, "{:?}", m.stats);
    // Settled (0 s in tests): every device deletes its own old logs.
    a.t.tidy();
    b.t.tidy();
    // Force new segments so old ones can go: write past 1 MiB is slow, so check the snapshot is used instead.
    b.sync();
    a.sync();
    assert_eq!(b.text("Shared.md").unwrap(), "intro 29\n\nmiddle\n\nend\n");
    c.sync();
    // C's offline line is merged three-way onto the current text.
    assert_eq!(c.text("Shared.md").unwrap(), "intro 29\n\nmiddle\n\nend\nadded offline\n");
    assert_eq!(c.text("Quiet.md").unwrap(), "untouched\n");
    assert_eq!(c.text("B29.md").unwrap(), "note 29\n");
    a.sync();
    b.sync();
    assert_eq!(a.text("Shared.md"), c.text("Shared.md"));
    assert_eq!(b.text("Shared.md"), c.text("Shared.md"));
    // A newcomer starts from the snapshot.
    let mut d = v.device("d");
    d.sync();
    assert_eq!(d.store, a.store);
    assert!(before > m.stats.kept as usize);
}

#[test]
fn compaction_drops_old_logs_and_bounds_history() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let mut a = v.device("a");
    // Enough to roll over a few 1 MiB segments.
    let big = "x".repeat(200_000);
    for i in 0..30 {
        a.write("Big.md", &format!("{i}\n{big}\n"));
        a.sync();
    }
    let segs = || walk(&v.root.join(&v.space.0).join("devices").join(&a.id.0)).len();
    let before = segs();
    assert!(before >= 3, "{before}");
    let Compacted::Done(_) = a.t.compact(Policy { keep_days: 0, min_versions: 3 }, None, true).unwrap() else { panic!() };
    assert!(a.t.tidy() >= before - 1);
    assert!(segs() < before);
    // History keeps the newest versions.
    let h = a.engine.history(&a.t, "Big.md").unwrap();
    assert!((3..=4).contains(&h.len()), "{}", h.len());
    a.write("Big.md", "after\n");
    a.sync();
    let mut b = v.device("b");
    b.sync();
    assert_eq!(b.text("Big.md").unwrap(), "after\n");
}

#[test]
fn two_devices_compacting_at_once_agree_on_one_snapshot() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let (mut a, mut b) = (v.device("a"), v.device("b"));
    for i in 0..10 {
        a.write("A.md", &format!("a {i}\n"));
        a.sync();
        b.write("B.md", &format!("b {i}\n"));
        b.sync();
    }
    a.sync();
    b.sync();
    // Each makes one before seeing the other's: write B's into a hidden
    // place first, as a slow sync tool would deliver it later.
    let Compacted::Done(ma) = a.t.compact(ALL, None, true).unwrap() else { panic!() };
    let snaps = v.root.join(&v.space.0).join("snapshots");
    let held = v.dir.path().join("held");
    fs::rename(snaps.join(ma.name()), &held).unwrap();
    let Compacted::Done(mb) = b.t.compact(ALL, None, true).unwrap() else { panic!() };
    fs::rename(&held, snaps.join(ma.name())).unwrap();
    let winner = if a.id < b.id { ma.name() } else { mb.name() };
    for d in [&mut a, &mut b] {
        d.sync();
        assert_eq!(d.t.generation().unwrap().0.name(), winner);
    }
    let mut c = v.device("c");
    c.sync();
    assert_eq!(c.t.generation().unwrap().0.name(), winner);
    assert_eq!((c.text("A.md").unwrap(), c.text("B.md").unwrap()), ("a 9\n".into(), "b 9\n".into()));
    // Once settled, the loser goes.
    a.t.tidy();
    let left: Vec<String> = fs::read_dir(&snaps).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, vec![winner]);
    // A third one isn't made while another of the same generation is there.
    assert!(matches!(b.t.compact(ALL, None, true).unwrap(), Compacted::Done(_)), "the next generation is fine once settled");
}

#[test]
fn a_compaction_cut_short_is_ignored_and_done_again() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let mut a = v.device("a");
    for i in 0..5 {
        a.write("N.md", &format!("{i}\n"));
        a.sync();
    }
    let snaps = v.root.join(&v.space.0).join("snapshots");
    // A crash mid-compaction leaves only a temporary folder: never read.
    fs::create_dir_all(snaps.join(format!(".000001-{}.tmp", a.id))).unwrap();
    fs::write(snaps.join(format!(".000001-{}.tmp", a.id)).join("part-000001.jsonl"), "{half").unwrap();
    let mut b = v.device("b");
    b.sync();
    assert!(b.t.generation().is_none());
    assert_eq!(b.text("N.md").unwrap(), "4\n");
    // Done again: the leftover is replaced.
    let Compacted::Done(m) = a.t.compact(ALL, None, true).unwrap() else { panic!("a fresh compaction") };
    assert!(!snaps.join(format!(".000001-{}.tmp", a.id)).exists());
    // A part still downloading on another device: the snapshot isn't used
    // there yet, and the logs are read as before.
    let part = snaps.join(m.name()).join(&m.parts[0].name);
    let whole = fs::read(&part).unwrap();
    fs::write(&part, &whole[..whole.len() / 2]).unwrap();
    let mut c = v.device("c");
    c.sync();
    assert!(c.t.generation().is_none());
    assert_eq!(c.text("N.md").unwrap(), "4\n");
    // Nor is a second one made meanwhile.
    assert!(matches!(c.t.compact(ALL, None, true).unwrap(), Compacted::Busy(_)));
    fs::write(&part, &whole).unwrap();
    c.sync();
    assert_eq!(c.t.generation().unwrap().0.name(), m.name());
    assert_eq!(c.text("N.md").unwrap(), "4\n");
}

#[test]
fn an_edit_built_on_a_folded_revision_is_kept_beside_the_note() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let mut a = v.device("a");
    a.write("N.md", "v0\n");
    a.sync();
    // C works on its own copy of the folder (a sync tool that stopped).
    let away = v.dir.path().join("away");
    copy_tree(&v.root, &away);
    let mut c = v.device_at("c", &away);
    c.sync();
    for i in 1..6 {
        a.write("N.md", &format!("v{i}\n"));
        a.sync();
    }
    c.write("N.md", "v0 edited on c\n");
    c.sync();
    let Compacted::Done(_) = a.t.compact(ALL, None, true).unwrap() else { panic!() };
    // The sync tool catches up both ways.
    copy_tree(&away.join(&v.space.0).join("devices").join(&c.id.0), &v.root.join(&v.space.0).join("devices").join(&c.id.0));
    let _ = fs::remove_dir_all(&away);
    copy_tree(&v.root, &away);
    c.t = FolderTransport::new(&away, &v.space, c.id.clone(), Some(&c.state)).with_keys(c.keys.clone()).with_settle(0);
    c.sync();
    a.sync();
    // Nobody's text is lost: the current note, and C's version beside it.
    let all_c: Vec<String> = c.store.0.values().cloned().collect();
    assert!(all_c.iter().any(|t| t == "v5\n"), "{all_c:?}");
    assert!(all_c.iter().any(|t| t == "v0 edited on c\n"), "{all_c:?}");
}

#[test]
fn removing_a_device_shuts_it_out_of_new_content_and_ignores_what_it_writes() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let (mut a, mut b, mut r) = (v.device("a"), v.device("b"), v.device("lost laptop"));
    for d in [&a, &b, &r] {
        d.publish(&v.space);
    }
    a.write("Old.md", "everyone saw this\n");
    a.sync();
    b.sync();
    r.sync();
    let ends = a.t.lane_ends().unwrap();
    let removal = Removal {
        space: &v.space,
        me: &a.id,
        secret: &a.secret,
        keys: &a.keys,
        target: &r.id,
        passphrase: "a new pass phrase",
        kdf: KdfParams::TEST,
        lanes: BTreeMap::from([(String::new(), ends.get(&r.id).copied().unwrap_or_default())]),
        removed: &BTreeSet::new(),
    };
    let done = admin::remove_device(&a.t, &removal, &mut a.pins).unwrap();
    assert_eq!(done.members.len(), 2);
    assert!(done.left_out.is_empty());
    assert!(a.refresh(&v.space).is_none());
    assert_eq!(a.keys.current(), 1);
    a.write("New.md", "after the removal\n");
    a.sync();
    assert!(b.refresh(&v.space).is_none());
    b.sync();
    assert_eq!(b.text("New.md").unwrap(), "after the removal\n");
    // The removed device: told so, still has what it had, can't read the new note.
    assert_eq!(r.refresh(&v.space), Some(KeyError::Removed));
    assert_eq!(r.text("Old.md").unwrap(), "everyone saw this\n");
    let res = r.engine.sync(&mut r.store, &r.t, TODAY);
    assert!(r.text("New.md").is_none(), "{res:?}");
    let raw = fs::read_to_string(walk(&v.root.join(&v.space.0).join("devices").join(&a.id.0)).pop().unwrap()).unwrap();
    assert!(!raw.contains("after the removal"));
    // What it writes from now on is ignored by the others (a modified
    // Quire writing with the old key, straight into its lane).
    let mut rogue = Engine::new(v.space.clone(), r.id.clone(), "rogue", KeySet::new(&v.key), SyncState::default());
    let mut s = MemoryStore::default();
    s.write("Rogue.md", "planted\n").unwrap();
    let rt = FolderTransport::new(&v.root, &v.space, r.id.clone(), None);
    let _ = rogue.sync(&mut s, &rt, TODAY);
    a.sync();
    b.sync();
    assert!(a.text("Rogue.md").is_none() && b.text("Rogue.md").is_none());
    // A device joining later uses the new passphrase.
    let w = folder::wrapped_key(&v.root, &v.space).unwrap();
    let recs = a.t.key_records();
    assert!(quire_sync::keyring::join_with_passphrase(&w, &recs, "pass phrase").is_err());
    let joined = quire_sync::keyring::join_with_passphrase(&w, &recs, "a new pass phrase").unwrap();
    assert_eq!(joined.current(), 1);
    let mut n = v.device("new");
    n.keys = joined.clone();
    n.engine.set_keys(joined.clone());
    n.t.set_keys(joined);
    n.t.set_removed(BTreeMap::from([(r.id.clone(), ends.get(&r.id).copied().unwrap_or_default())]));
    n.sync();
    assert_eq!(n.text("New.md").unwrap(), "after the removal\n");
    assert_eq!(n.text("Old.md").unwrap(), "everyone saw this\n");
    assert!(n.text("Rogue.md").is_none());
}

#[test]
fn a_removal_cut_short_is_picked_up_on_the_next_pass() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let (mut a, mut b, r) = (v.device("a"), v.device("b"), v.device("r"));
    for d in [&a, &b, &r] {
        d.publish(&v.space);
    }
    // The record is written, then the app stops before keeping the new key.
    let removal = Removal {
        space: &v.space,
        me: &a.id,
        secret: &a.secret,
        keys: &a.keys,
        target: &r.id,
        passphrase: "p2",
        kdf: KdfParams::TEST,
        lanes: BTreeMap::new(),
        removed: &BTreeSet::new(),
    };
    let done = admin::remove_device(&a.t, &removal, &mut a.pins).unwrap();
    assert_eq!(a.keys.current(), 0, "not kept yet");
    // Next start: the record is found and taken, by the maker too.
    assert!(a.refresh(&v.space).is_none());
    assert!(b.refresh(&v.space).is_none());
    assert_eq!(a.keys.raw(1).unwrap().as_bytes(), done.key.as_bytes());
    assert_eq!(b.keys.raw(1).unwrap().as_bytes(), done.key.as_bytes());
    // A record whose file is cut short (a crash while writing; a sync tool
    // mid-copy) is ignored, not half-used.
    let dir = v.root.join(&v.space.0).join("keys");
    let f = fs::read_dir(&dir).unwrap().flatten().next().unwrap().path();
    let whole = fs::read(&f).unwrap();
    fs::write(&f, &whole[..whole.len() / 3]).unwrap();
    let mut c = v.device("c");
    assert!(c.refresh(&v.space).is_none());
    assert_eq!(c.keys.current(), 0);
}

#[test]
fn a_revision_lost_for_good_doesnt_hold_its_note_back() {
    let v = Vault::new(folder::LEVEL_HARDENED);
    let mut a = v.device("a");
    for i in 0..4 {
        a.write("N.md", &format!("version {i}\n"));
        a.sync();
    }
    // The second revision's line is damaged for good.
    let seg = walk(&v.root.join(&v.space.0).join("devices").join(&a.id.0)).pop().unwrap();
    let text = fs::read_to_string(&seg).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[1] = "{damaged".into();
    fs::write(&seg, lines.join("\n") + "\n").unwrap();
    let mut b = v.device("b");
    b.sync();
    assert_eq!(b.text("N.md").unwrap(), "version 3\n");
    // And editing it from there works for both.
    b.write("N.md", "version 3\nfrom b\n");
    b.sync();
    a.sync();
    assert_eq!(a.text("N.md").unwrap(), "version 3\nfrom b\n");
}

#[test]
fn removal_needs_an_upgraded_vault() {
    let v = Vault::new(1);
    let (mut a, r) = (v.device("a"), v.device("r"));
    let removal = Removal {
        space: &v.space,
        me: &a.id,
        secret: &a.secret,
        keys: &a.keys,
        target: &r.id,
        passphrase: "p",
        kdf: KdfParams::TEST,
        lanes: BTreeMap::new(),
        removed: &BTreeSet::new(),
    };
    let e = admin::remove_device(&a.t, &removal, &mut a.pins).unwrap_err();
    assert!(e.to_string().contains("Upgrade"), "{e}");
    assert!(a.t.level() < admin::HARDENED);
    let _ = a.t.pull(0);
}
