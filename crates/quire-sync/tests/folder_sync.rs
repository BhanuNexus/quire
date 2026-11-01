// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Devices syncing through a shared folder that a slow, unreliable sync tool
//! copies between them: files arrive late, out of order and half-written.
//! Every device must still end up with the same notes, and no text anyone
//! wrote may be lost.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fs;
use std::path::{Path, PathBuf};

use quire_sync::folder::{self, FolderTransport};
use quire_sync::{DeviceId, Engine, KdfParams, LocalStore, MemoryStore, Report, SpaceId, SyncState, VaultKey, WrappedKey};

const TODAY: &str = "2026-11-01";

struct Device {
    id: DeviceId,
    root: PathBuf,
    state: PathBuf,
    engine: Engine,
    store: MemoryStore,
    transport: FolderTransport,
}

/// Each device has its own copy of the sync folder; [`Cloud::spread`] plays
/// the sync tool, copying each device's logs to the others.
struct Cloud {
    _dir: tempfile::TempDir,
    space: SpaceId,
    key: VaultKey,
    devs: Vec<Device>,
}

fn copy_prefix(from: &Path, to: &Path, len: u64) {
    let bytes = fs::read(from).unwrap();
    let have = fs::metadata(to).map_or(0, |m| m.len());
    let len = usize::try_from(len.max(have)).unwrap().min(bytes.len());
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    // Sync tools usually replace a file whole; a reader can still catch one
    // mid-download, which writing a prefix models.
    fs::write(to, &bytes[..len]).unwrap();
}

impl Cloud {
    fn new(n: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        let wrapped = WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap();
        let devs = (0..n)
            .map(|i| {
                let root = dir.path().join(format!("replica{i}"));
                folder::create_space(&root, &space, &wrapped).unwrap();
                let state = dir.path().join(format!("state{i}"));
                fs::create_dir_all(&state).unwrap();
                let id = DeviceId::random().unwrap();
                Device {
                    transport: FolderTransport::new(&root, &space, id.clone(), Some(&state)),
                    engine: Engine::new(space.clone(), id.clone(), format!("device {i}"), key.keys(), SyncState::default()),
                    store: MemoryStore::default(),
                    id,
                    root,
                    state,
                }
            })
            .collect();
        Self { _dir: dir, space, key, devs }
    }

    fn log_dir(&self, replica: usize, owner: usize) -> PathBuf {
        self.devs[replica].root.join(&self.space.0).join("devices").join(&self.devs[owner].id.0)
    }

    /// Copy logs between devices. `rng = None` copies everything.
    fn spread(&self, mut rng: Option<&mut Rng>) {
        for to in 0..self.devs.len() {
            for owner in 0..self.devs.len() {
                if owner == to {
                    continue;
                }
                let Ok(files) = fs::read_dir(self.log_dir(owner, owner)) else { continue };
                let mut files: Vec<PathBuf> = files.flatten().map(|e| e.path()).collect();
                files.sort();
                for f in files {
                    let target = self.log_dir(to, owner).join(f.file_name().unwrap());
                    let full = fs::metadata(&f).unwrap().len();
                    let len = match rng.as_deref_mut() {
                        None => full,
                        Some(r) => match r.below(4) {
                            0 => continue,
                            1 => r.next() % (full + 1),
                            _ => full,
                        },
                    };
                    copy_prefix(&f, &target, len);
                }
            }
        }
    }

    fn sync(&mut self, i: usize) -> Report {
        let d = &mut self.devs[i];
        d.engine.sync(&mut d.store, &d.transport, TODAY).unwrap()
    }

    /// Spread everything and sync everyone until nothing changes.
    fn settle(&mut self) {
        for round in 0..12 {
            self.spread(None);
            let mut changed = false;
            for i in 0..self.devs.len() {
                let r = self.sync(i);
                if round >= 8 && std::env::var_os("SYNC_TRACE").is_some() {
                    eprintln!("round {round} dev {i}: {r:?}\n  {:?}\n  {:?}", self.devs[i].store.0, self.devs[i].engine.state.files);
                }
                changed |= r.changed_anything();
            }
            if !changed {
                return;
            }
        }
        panic!("devices never settled");
    }

    fn put(&mut self, i: usize, path: &str, text: &str) {
        self.devs[i].store.write(path, text).unwrap();
    }

    fn read(&self, i: usize, path: &str) -> Option<String> {
        self.devs[i].store.read(path)
    }

    fn assert_agree(&self) {
        for d in &self.devs[1..] {
            assert_eq!(d.store, self.devs[0].store);
        }
    }
}

#[test]
fn notes_renames_and_deletes_travel() {
    let mut c = Cloud::new(2);
    c.put(0, "Plan.md", "# Plan\nShip it.\n");
    c.put(0, "Ideas.md", "# Ideas\n");
    c.settle();
    c.assert_agree();
    let text = c.devs[0].store.0.remove("Plan.md").unwrap();
    c.put(0, "Projects/Plan.md", &text);
    c.devs[1].store.0.remove("Ideas.md");
    c.settle();
    c.assert_agree();
    assert_eq!(c.devs[0].store.list(), vec!["Projects/Plan.md".to_string()]);
}

#[test]
fn edits_made_before_either_device_saw_the_other_merge() {
    let mut c = Cloud::new(2);
    c.put(0, "Plan.md", "# Plan\none\ntwo\nthree\n");
    c.settle();
    // Both edit and sync while the sync tool hasn't copied anything: each
    // device's revision builds on the same one.
    c.put(0, "Plan.md", "# Plan\nONE\ntwo\nthree\n");
    c.put(1, "Plan.md", "# Plan\none\ntwo\nTHREE\n");
    c.sync(0);
    c.sync(1);
    c.settle();
    c.assert_agree();
    assert_eq!(c.read(0, "Plan.md").unwrap(), "# Plan\nONE\ntwo\nTHREE\n");
}

#[test]
fn overlapping_edits_before_either_device_saw_the_other_keep_both() {
    let mut c = Cloud::new(3);
    c.put(0, "Plan.md", "# Plan\nShip it.\n");
    c.settle();
    c.put(0, "Plan.md", "# Plan\nShip it on Friday.\n");
    c.put(1, "Plan.md", "# Plan\nShip it to everyone.\n");
    c.sync(0);
    c.sync(1);
    // The third device sees one side first, then the other.
    c.spread(None);
    c.sync(2);
    c.settle();
    c.assert_agree();
    let all: String = c.devs[0].store.0.values().cloned().collect();
    assert!(all.contains("Ship it on Friday.") && all.contains("Ship it to everyone."), "{all}");
    assert_eq!(c.devs[0].store.list().len(), 2, "one note and one conflict copy: {:?}", c.devs[0].store);
}

#[test]
fn three_devices_racing_on_the_same_lines_keep_each_version_exactly_once() {
    let mut c = Cloud::new(3);
    c.put(0, "Plan.md", "# Plan\nShip it.\n");
    c.settle();
    for (i, v) in ["Friday", "Monday", "never"].iter().enumerate() {
        c.put(i, "Plan.md", &format!("# Plan\nShip it {v}.\n"));
        c.sync(i);
    }
    c.settle();
    c.assert_agree();
    let texts: Vec<&String> = c.devs[0].store.0.values().collect();
    assert_eq!(texts.len(), 3, "the note and one copy per losing device: {:?}", c.devs[0].store);
    for v in ["Friday", "Monday", "never"] {
        let n = texts.iter().filter(|t| t.contains(&format!("Ship it {v}."))).count();
        assert_eq!(n, 1, "{v} appears {n} times: {:?}", c.devs[0].store);
    }
}

#[test]
fn an_edit_beats_a_delete_made_at_the_same_time() {
    let mut c = Cloud::new(2);
    c.put(0, "Plan.md", "# Plan\n");
    c.settle();
    c.devs[0].store.0.remove("Plan.md");
    c.put(1, "Plan.md", "# Plan\nStill needed.\n");
    c.sync(0);
    c.sync(1);
    c.settle();
    c.assert_agree();
    assert_eq!(c.read(0, "Plan.md").unwrap(), "# Plan\nStill needed.\n");
}

#[test]
fn a_restart_doesnt_upload_or_rewrite_anything() {
    let mut c = Cloud::new(2);
    c.put(0, "A.md", "a\n");
    c.put(1, "B.md", "b\n");
    c.settle();
    let logs = |c: &Cloud| -> u64 {
        (0..2).map(|i| fs::read_dir(c.log_dir(i, i)).unwrap().flatten().map(|f| f.metadata().unwrap().len()).sum::<u64>()).sum()
    };
    let before = logs(&c);
    let d = &mut c.devs[1];
    let state: SyncState = serde_json::from_str(&serde_json::to_string(&d.engine.state).unwrap()).unwrap();
    d.transport = FolderTransport::new(&d.root, &c.space, d.id.clone(), Some(&d.state));
    d.engine = Engine::new(c.space.clone(), d.id.clone(), "device 1", c.key.keys(), state);
    let r = c.sync(1);
    assert!(!r.changed_anything(), "{r:?}");
    assert_eq!(logs(&c), before);
}

#[test]
fn the_folder_holds_no_names_or_text() {
    let mut c = Cloud::new(1);
    c.put(0, "Secret Project/Launch Plan.md", "# Launch\nCodename BLUEBIRD\n");
    c.settle();
    let log = fs::read_dir(c.log_dir(0, 0)).unwrap().flatten().map(|f| fs::read_to_string(f.path()).unwrap()).collect::<String>();
    assert!(!log.is_empty());
    for leak in ["Secret", "Launch", "BLUEBIRD", ".md"] {
        assert!(!log.contains(leak), "the folder shows {leak:?}");
    }
}

// ------------------------------------------------------------------ random simulations

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

const NAMES: [&str; 5] = ["A.md", "B.md", "Notes/C.md", "D.md", "pic.png"];

fn random_edit(store: &mut MemoryStore, rng: &mut Rng, token: &mut u64, additive: bool) -> Option<String> {
    *token += 1;
    let word = format!("w{token}");
    let paths = store.list();
    match rng.below(if additive { 3 } else { 6 }) {
        0 | 1 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            let mut lines: Vec<String> = store.read(&p).unwrap().lines().map(str::to_string).collect();
            let at = rng.below(lines.len() + 1);
            lines.insert(at, word.clone());
            store.write(&p, &(lines.join("\n") + "\n")).unwrap();
            Some(word)
        }
        0..=2 => {
            let p = NAMES[rng.below(NAMES.len())];
            let mut text = store.read(p).unwrap_or_default();
            text.push_str(&word);
            text.push('\n');
            store.write(p, &text).unwrap();
            Some(word)
        }
        3 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            let mut lines: Vec<String> = store.read(&p).unwrap().lines().map(str::to_string).collect();
            if !lines.is_empty() {
                let i = rng.below(lines.len());
                lines[i] = word;
            }
            store.write(&p, &(lines.join("\n") + "\n")).unwrap();
            None
        }
        4 if !paths.is_empty() => {
            store.0.remove(&paths[rng.below(paths.len())]);
            None
        }
        5 if !paths.is_empty() => {
            let p = paths[rng.below(paths.len())].clone();
            let to = NAMES[rng.below(NAMES.len())];
            if !store.0.contains_key(to) {
                let t = store.0.remove(&p).unwrap();
                store.write(to, &t).unwrap();
            }
            None
        }
        _ => None,
    }
}

fn simulate(seed: u64, n: usize, steps: usize, additive: bool) {
    let mut c = Cloud::new(n);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let (mut token, mut written) = (0, Vec::new());
    for _ in 0..steps {
        let i = rng.below(n);
        match rng.below(4) {
            0 => {
                c.sync(i);
            }
            1 => c.spread(Some(&mut rng)),
            _ => written.extend(random_edit(&mut c.devs[i].store, &mut rng, &mut token, additive)),
        }
    }
    c.settle();
    for d in &c.devs[1..] {
        assert_eq!(d.store, c.devs[0].store, "seed {seed}: devices disagree");
    }
    if additive {
        let all: String = c.devs[0].store.0.values().cloned().collect::<Vec<_>>().join("\n");
        for w in &written {
            assert!(all.split_whitespace().any(|x| x == w), "seed {seed}: lost {w}\n{all}");
        }
    }
}

fn seeds() -> std::ops::RangeInclusive<u64> {
    match std::env::var("SYNC_SEED").ok().and_then(|s| s.parse().ok()) {
        Some(s) => s..=s,
        None => 1..=std::env::var("SYNC_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(150),
    }
}

#[test]
fn random_edits_through_a_slow_folder_never_lose_text() {
    for seed in seeds() {
        simulate(seed, 3, 60, true);
    }
}

#[test]
fn random_edits_renames_and_deletes_through_a_slow_folder_converge() {
    for seed in seeds() {
        simulate(seed, 3, 70, false);
    }
}

#[test]
fn history_includes_the_version_that_lost_a_tie() {
    let mut c = Cloud::new(2);
    c.put(0, "Plan.md", "# Plan\nShip it.\n");
    c.settle();
    c.put(0, "Plan.md", "# Plan\nShip it Friday.\n");
    c.put(1, "Plan.md", "# Plan\nShip it Monday.\n");
    c.sync(0);
    c.sync(1);
    c.settle();
    let d = &c.devs[0];
    let h = d.engine.history(&d.transport, "Plan.md").unwrap();
    for v in ["Ship it.", "Ship it Friday.", "Ship it Monday."] {
        assert!(h.iter().any(|x| x.text.contains(v)), "{v} missing from history");
    }
}
