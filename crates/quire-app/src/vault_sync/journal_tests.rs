//! The sync record on disk: commits reopen to what was committed, every
//! crash point of `docs/design/0.14-sync-speed.md` §3.5 leaves the old or
//! the new record, damage is kept aside and said, and 0.13's `state.json`
//! migrates and comes back.

use std::collections::BTreeMap;
use std::path::Path;

use quire_sync::bases::{KeptCopy13, LeftCopy13, State13, Tracked13};
use quire_sync::engine::{KeptCopy, LeftCopy};
use quire_sync::protocol::RevId;
use quire_sync::{Bases, DeleteMark, FileId, Print, Skipped, SyncState, Tracked};

use super::*;

const NAME: &str = "state.json";

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

fn tracked(path: &str, text: &str, bases: &mut PackBases) -> Tracked {
    let base = Print::of(text);
    if !is_attachment(path) {
        bases.put(base, text).unwrap();
    }
    Tracked {
        path: path.into(),
        synced_path: path.into(),
        head: RevId(format!("r-{}", &base.to_hex()[..8])),
        head_seq: 1,
        base,
        deleted: false,
        outside: false,
        absorbed: false,
        kept: None,
        up: Vec::new(),
        disk: None,
    }
}

/// Some note text of about `n` bytes.
fn text(rng: &mut Rng, n: usize) -> String {
    let mut out: String = (0..n / 8).map(|_| format!("w{:05} ", rng.next() % 100_000)).collect::<Vec<_>>().concat();
    out.push('\n');
    out
}

fn is_pack(name: &str) -> bool {
    Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("pack"))
}

fn open(root: &Path) -> Opened {
    Journal::open(root, NAME).unwrap()
}

/// Every note text `state` needs is there.
fn assert_texts(state: &SyncState, bases: &PackBases, texts: &BTreeMap<Print, String>) {
    for p in live_prints(state) {
        assert_eq!(bases.get(&p).unwrap().as_ref(), texts.get(&p), "the text of {p:?}");
    }
}

fn dir_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let d = state_dir(root, NAME);
    std::fs::read_dir(&d)
        .map(|rd| {
            rd.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap_or_default())).collect()
        })
        .unwrap_or_default()
}

/// A random change to `state`, its texts put in `bases`.
fn change(rng: &mut Rng, state: &mut SyncState, bases: &mut PackBases, texts: &mut BTreeMap<Print, String>) {
    let ids: Vec<FileId> = state.files.keys().cloned().collect();
    match rng.below(6) {
        0 | 1 => {
            let id = FileId(format!("f{:04}", rng.next() % 10_000));
            let path = if rng.below(5) == 0 { format!("pic{}.png", id.0) } else { format!("{}.md", id.0) };
            let n = 50 + rng.below(400);
            let t = text(rng, n);
            texts.insert(Print::of(&t), t.clone());
            state.files.insert(id, tracked(&path, &t, bases));
        }
        2 if !ids.is_empty() => {
            let id = &ids[rng.below(ids.len())];
            let path = state.files[id].path.clone();
            let n = 50 + rng.below(400);
            let t = text(rng, n);
            texts.insert(Print::of(&t), t.clone());
            let mut new = tracked(&path, &t, bases);
            new.kept = (rng.below(3) == 0).then(|| KeptCopy { path: format!("Away/{path}"), base: state.files[id].base });
            state.files.insert(id.clone(), new);
        }
        3 if !ids.is_empty() => {
            state.files.remove(&ids[rng.below(ids.len())]);
        }
        4 => state.cursor += 1 + rng.next() % 5,
        _ => {
            let t = text(rng, 60);
            texts.insert(Print::of(&t), t.clone());
            bases.put(Print::of(&t), &t).unwrap();
            state.left.insert(format!("left{}.md", rng.below(4)), LeftCopy { file: FileId("f0".into()), base: Some(Print::of(&t)) });
            state.deletes_sent = vec![DeleteMark { at: i64::try_from(rng.next() % 1000).unwrap(), held: 3, sent: 1 }];
        }
    }
}

#[test]
fn random_commits_reopen_to_what_was_committed() {
    for seed in 1..=12u64 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut o = open(root);
        let mut state = SyncState::default();
        let mut texts = BTreeMap::new();
        for round in 0..60 {
            for _ in 0..=rng.below(4) {
                change(&mut rng, &mut state, &mut o.bases, &mut texts);
            }
            o.journal.commit(&state, i64::from(round), &[]).unwrap();
            if rng.below(4) == 0 {
                drop(o);
                o = open(root);
                assert_eq!(o.state, state, "seed {seed} round {round}");
                assert_eq!(o.journal.verified_at(), i64::from(round));
                assert!(o.notices.is_empty(), "{:?}", o.notices);
            }
            assert_texts(&state, &o.bases, &texts);
        }
        // The packs stay within twice the live bytes and one pack.
        let live: u64 = live_prints(&state).iter().map(|p| encode_record(*p, &texts[p]).len() as u64).sum();
        let on_disk: u64 = dir_files(root).iter().filter(|(n, _)| is_pack(n)).map(|(_, b)| b.len() as u64).sum();
        assert!(on_disk <= 2 * live + PACK_LIMIT, "seed {seed}: {on_disk} bytes of packs for {live} live");
    }
}

#[test]
fn a_commit_with_nothing_new_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = open(dir.path());
    assert!(!o.journal.commit(&SyncState::default(), 0, &[]).unwrap(), "nothing yet: no folder");
    assert!(!state_dir(dir.path(), NAME).exists());
    let mut state = SyncState::default();
    state.files.insert(FileId("a".into()), tracked("A.md", "a\n", &mut o.bases));
    assert!(o.journal.commit(&state, 0, &[]).unwrap());
    let before = dir_files(dir.path());
    assert!(!o.journal.commit(&state, 0, &[]).unwrap());
    // A base put again (the same text pulled again) changes nothing either.
    o.bases.put(Print::of("a\n"), "a\n").unwrap();
    assert!(!o.journal.commit(&state, 0, &[]).unwrap());
    assert_eq!(dir_files(dir.path()), before);
    drop(o);
    let mut o = open(dir.path());
    assert!(!o.journal.commit(&state, 0, &[]).unwrap());
    assert_eq!(dir_files(dir.path()), before, "opening a good record changes nothing");
}

/// A 0.13 `state.json` with kept copies, left copies, skipped notes and
/// deletes sent.
fn fixture_013(cursor: u64) -> (State13, Vec<u8>) {
    let t = |path: &str, base: &str| Tracked13 {
        path: path.into(),
        synced_path: path.into(),
        head: RevId("r1".into()),
        head_seq: 4,
        base: base.into(),
        deleted: false,
        outside: false,
        absorbed: false,
        kept: None,
        up: vec![RevId("r0".into())],
    };
    let mut kept = t("Work/Plan.md", "plan\n");
    kept.outside = true;
    kept.kept = Some(KeptCopy13 { path: "Work/Plan.md".into(), base: "plan, kept\n".into() });
    let old = State13 {
        cursor,
        files: BTreeMap::from([
            (FileId("f1".into()), t("Ideas.md", "# Ideas\nmany\n")),
            (FileId("f2".into()), t("pic.png", &format!("sha256:{}", Print::of("iVBOR").to_hex()))),
            (FileId("f3".into()), kept),
        ]),
        left: BTreeMap::from([("Old.md".into(), LeftCopy13 { file: FileId("f1".into()), base: Some("old\n".into()) })]),
        skipped: BTreeMap::from([(FileId("f4".into()), Skipped { path: "a:b.md".into(), rev: RevId("r9".into()), why: "a colon".into() })]),
        deletes_sent: vec![DeleteMark { at: 5, held: 10, sent: 2 }],
    };
    let bytes = serde_json::to_vec(&old).unwrap();
    (old, bytes)
}

fn put_013(root: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(sync_dir(root)).unwrap();
    std::fs::write(sync_dir(root).join(NAME), bytes).unwrap();
}

#[test]
fn a_013_state_migrates_once_and_reads_back_equal() {
    let dir = tempfile::tempdir().unwrap();
    let (old, bytes) = fixture_013(12);
    put_013(dir.path(), &bytes);
    let o = open(dir.path());
    let mut mem = quire_sync::MemoryBases::default();
    let want = old.clone().into_state(&mut mem).unwrap();
    assert_eq!(o.state, want);
    assert!(o.notices.is_empty(), "{:?}", o.notices);
    assert!(!sync_dir(dir.path()).join(NAME).exists(), "the 0.13 file is gone once migrated");
    assert!(o.state.files.values().all(|t| t.disk.is_none()), "every note is read once by the first pass");
    // Every text, back from the packs, equals the 0.13 file's.
    assert_eq!(State13::from_state(&o.state, &o.bases), Some(old));
    assert_eq!(peek(dir.path(), NAME), want);
    drop(o);
    let o = open(dir.path());
    assert_eq!(o.state, want, "opened again, not migrated again");
    assert!(o.notices.is_empty());
}

#[test]
fn an_older_quire_syncing_the_same_vault_is_named() {
    let dir = tempfile::tempdir().unwrap();
    put_013(dir.path(), &fixture_013(1).1);
    let mut o = open(dir.path());
    assert!(o.notices.is_empty());
    let mut s = o.state.clone();
    s.cursor = 50;
    o.journal.commit(&s, 0, &[]).unwrap();
    drop(o);
    // A downgrade and back: the older Quire started afresh and saved its own.
    put_013(dir.path(), &fixture_013(2).1);
    let o = open(dir.path());
    assert_eq!(o.state.cursor, 2, "the older file wins");
    assert_eq!(o.notices, vec![OLDER_RAN_ONCE.to_string()]);
    drop(o);
    for cursor in [3, 4] {
        put_013(dir.path(), &fixture_013(cursor).1);
        let o = open(dir.path());
        assert_eq!(o.state.cursor, cursor);
        assert_eq!(o.notices, vec![OLDER_RUNS.to_string()], "from the second time, both Quires are named");
    }
}

#[test]
fn every_crash_or_error_during_a_migration_leaves_one_valid_state() {
    let (old, bytes) = fixture_013(12);
    let want = old.into_state(&mut quire_sync::MemoryBases::default()).unwrap();
    for crash in [true, false] {
        for n in 0.. {
            let dir = tempfile::tempdir().unwrap();
            put_013(dir.path(), &bytes);
            inject(Some(n), crash);
            let r = Journal::open(dir.path(), NAME);
            let fired = fault_fired();
            inject(None, false);
            if !fired {
                assert!(r.is_ok());
                break;
            }
            drop(r);
            let o = open(dir.path());
            assert_eq!(o.state, want, "step {n} crash {crash}");
            assert!(State13::from_state(&o.state, &o.bases).is_some(), "every text is there");
            assert!(!sync_dir(dir.path()).join(NAME).exists());
        }
    }
}

/// A record with notes in several packs, then a change that drops most of
/// them (so packs are compacted) and adds new texts, with a fault at
/// every step in turn.
#[test]
fn every_crash_or_error_during_a_commit_leaves_the_old_or_the_new_state() {
    for crash in [true, false] {
        for n in 0.. {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let mut rng = Rng(7);
            let mut o = open(root);
            let mut texts = BTreeMap::new();
            let mut old = SyncState::default();
            for i in 0..40 {
                let t = text(&mut rng, 300);
                texts.insert(Print::of(&t), t.clone());
                old.files.insert(FileId(format!("f{i:02}")), tracked(&format!("{i}.md"), &t, &mut o.bases));
                if i % 8 == 7 {
                    o.journal.commit(&old, 0, &[]).unwrap();
                }
            }
            o.journal.commit(&old, 0, &[]).unwrap();
            let mut new = old.clone();
            new.files.retain(|id, _| id.0.ends_with('3') || id.0.ends_with('7'));
            for i in 40..60 {
                let t = text(&mut rng, 300);
                texts.insert(Print::of(&t), t.clone());
                new.files.insert(FileId(format!("f{i:02}")), tracked(&format!("{i}.md"), &t, &mut o.bases));
            }
            new.cursor = 9;
            inject(Some(n), crash);
            let r = o.journal.commit(&new, 0, &[]);
            let fired = fault_fired();
            inject(None, false);
            if !fired {
                assert!(r.is_ok());
                break;
            }
            if !crash {
                // Not a crash: the session goes on, and its next commit holds.
                if r.is_err() {
                    o.journal.commit(&new, 0, &[]).unwrap();
                }
                drop(o);
                let o = open(root);
                assert_eq!(o.state, new, "step {n}: the retried commit");
                assert_texts(&o.state, &o.bases, &texts);
                continue;
            }
            drop(o);
            let o = open(root);
            assert!(o.state == old || o.state == new, "step {n}: neither the old nor the new state");
            assert!(o.notices.is_empty(), "a crash is no damage: {:?}", o.notices);
            assert_texts(&o.state, &o.bases, &texts);
        }
    }
}

#[test]
fn every_crash_during_a_checkpoint_leaves_the_old_or_the_new_state() {
    for n in 0.. {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut o = open(root);
        let mut state = SyncState::default();
        let mut texts = BTreeMap::new();
        let mut rng = Rng(3);
        // Grow the journal to just under its limit.
        let mut i = 0;
        loop {
            let t = text(&mut rng, 40);
            texts.insert(Print::of(&t), t.clone());
            state.files.insert(FileId(format!("f{i:03}")), tracked(&format!("{i}.md"), &t, &mut o.bases));
            i += 1;
            let before = o.journal.generation;
            o.journal.commit(&state, 0, &[]).unwrap();
            if o.journal.generation == before && o.journal.log_len > JOURNAL_MIN * 3 / 4 {
                break;
            }
        }
        let old = state.clone();
        let generation = o.journal.generation;
        // The next notes, their texts put before the fault is set.
        let mut more = Vec::new();
        for _ in 0..50 {
            let t = text(&mut rng, 40);
            texts.insert(Print::of(&t), t.clone());
            more.push((FileId(format!("f{i:03}")), tracked(&format!("{i}.md"), &t, &mut o.bases)));
            i += 1;
        }
        inject(Some(n), true);
        let mut res = Ok(true);
        for (id, t) in more {
            if o.journal.generation != generation || res.is_err() {
                break;
            }
            state.files.insert(id, t);
            res = o.journal.commit(&state, 0, &[]);
        }
        let fired = fault_fired();
        inject(None, false);
        drop(o);
        let o = open(root);
        assert_texts(&o.state, &o.bases, &texts);
        assert!(o.notices.is_empty(), "{:?}", o.notices);
        if !fired {
            assert_eq!(o.state, state);
            break;
        }
        assert!(o.state.files.len() >= old.files.len() && o.state.files.len() <= state.files.len(), "step {n}");
        assert!(o.state.files.iter().all(|(id, t)| state.files.get(id) == Some(t)), "step {n}: only committed entries");
    }
}

#[test]
fn a_purged_text_leaves_every_pack_in_the_same_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = open(dir.path());
    let mut state = SyncState::default();
    // Three versions of a secret note, each committed, and other notes.
    for v in 1..=3 {
        let t = format!("secret day, version {v}\n");
        state.files.insert(FileId("s".into()), tracked("Journal/Day.md", &t, &mut o.bases));
        for i in 0..5 {
            state.files.insert(FileId(format!("p{i}")), tracked(&format!("{i}.md"), &format!("plain {i} {v}\n"), &mut o.bases));
        }
        o.journal.commit(&state, 0, &[]).unwrap();
    }
    let raw_has = |root: &Path, needle: &str| {
        let raw: Vec<u8> = dir_files(root).into_iter().filter(|(n, _)| is_pack(n)).flat_map(|(_, b)| b).collect();
        String::from_utf8_lossy(&raw).contains(needle) || pack_texts(root, NAME).iter().any(|t| t.contains(needle))
    };
    assert!(raw_has(dir.path(), "secret day"));
    // The folder is locked: the scrub clears the base and names it.
    let cleared = std::mem::replace(&mut state.files.get_mut(&FileId("s".into())).unwrap().base, Print::of(""));
    assert!(o.journal.commit(&state, 0, &[cleared]).unwrap());
    assert!(!raw_has(dir.path(), "secret day"), "no version of the locked note's text in any pack");
    for i in 0..5 {
        assert_eq!(o.bases.get(&Print::of(&format!("plain {i} 3\n"))).unwrap().as_deref(), Some(format!("plain {i} 3\n").as_str()));
    }
    // A purge with nothing to purge (shared text) keeps it.
    drop(o);
    let o = open(dir.path());
    assert_eq!(o.state, state);
}

// ------------------------------------------------------------------ damage

fn committed_record(root: &Path) -> (SyncState, BTreeMap<Print, String>) {
    let mut o = open(root);
    let mut state = SyncState::default();
    let mut texts = BTreeMap::new();
    for i in 0..6 {
        let t = format!("note {i}\n");
        texts.insert(Print::of(&t), t.clone());
        state.files.insert(FileId(format!("f{i}")), tracked(&format!("{i}.md"), &t, &mut o.bases));
        o.journal.commit(&state, 0, &[]).unwrap();
    }
    (state, texts)
}

fn only(root: &Path, prefix: &str) -> PathBuf {
    let d = state_dir(root, NAME);
    let mut v: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    v.sort();
    v.pop().unwrap()
}

#[test]
fn a_record_that_cant_be_read_now_is_an_error_and_nothing_changes() {
    let dir = tempfile::tempdir().unwrap();
    committed_record(dir.path());
    let ck = only(dir.path(), "checkpoint-");
    let bytes = std::fs::read(&ck).unwrap();
    // A checkpoint that can't be read (a folder in its place stands for a
    // permission or disk error).
    std::fs::remove_file(&ck).unwrap();
    std::fs::create_dir_all(&ck).unwrap();
    let before = dir_files(dir.path());
    let e = Journal::open(dir.path(), NAME).unwrap_err();
    assert!(e.contains("Couldn't read what sync did last"), "{e}");
    assert_eq!(dir_files(dir.path()), before, "nothing tidied or moved");
    std::fs::remove_dir(&ck).unwrap();
    std::fs::write(&ck, bytes).unwrap();
    assert!(Journal::open(dir.path(), NAME).is_ok());
}

#[test]
fn a_record_with_no_good_checkpoint_is_kept_aside_whole() {
    let dir = tempfile::tempdir().unwrap();
    committed_record(dir.path());
    let ck = only(dir.path(), "checkpoint-");
    let mut bytes = std::fs::read(&ck).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 1;
    std::fs::write(&ck, bytes).unwrap();
    let packs_before: Vec<_> = dir_files(dir.path()).into_iter().filter(|(n, _)| is_pack(n)).collect();
    let o = open(dir.path());
    assert_eq!(o.state, SyncState::default());
    assert!(o.notices.iter().any(|n| n.contains("started afresh")), "{:?}", o.notices);
    let aside = sync_dir(dir.path()).join("state.d.unreadable");
    for (name, b) in packs_before {
        assert_eq!(std::fs::read(aside.join(name)).unwrap(), b, "kept whole, packs included");
    }
}

#[test]
fn a_damaged_newest_checkpoint_falls_back_to_the_one_before() {
    let dir = tempfile::tempdir().unwrap();
    let (state, texts) = committed_record(dir.path());
    let ck = only(dir.path(), "checkpoint-");
    let n: u32 = numbered(&ck.file_name().unwrap().to_string_lossy(), "checkpoint-", ".json").unwrap();
    // A newer checkpoint written over the same record, then damaged.
    let good = std::fs::read(&ck).unwrap();
    let newer = state_dir(dir.path(), NAME).join(checkpoint_name(n + 1));
    std::fs::write(&newer, &good[..good.len() - 9]).unwrap();
    let o = open(dir.path());
    assert_eq!(o.state, state);
    assert!(o.notices.iter().any(|m| m.contains("Part of sync's record")), "{:?}", o.notices);
    assert!(state_dir(dir.path(), NAME).join(format!("{}.unreadable", checkpoint_name(n + 1))).exists());
    assert_texts(&o.state, &o.bases, &texts);
}

#[test]
fn a_torn_journal_tail_is_cut_silently() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = committed_record(dir.path());
    let j = only(dir.path(), "journal-");
    let mut f = OpenOptions::new().append(true).open(&j).unwrap();
    f.write_all(b"{\"h\":\"00").unwrap();
    drop(f);
    let len = std::fs::metadata(&j).unwrap().len();
    let looked = peek(dir.path(), NAME);
    assert_eq!(looked, state);
    assert_eq!(std::fs::metadata(&j).unwrap().len(), len, "a look changes nothing");
    let o = open(dir.path());
    assert_eq!(o.state, state);
    assert!(o.notices.is_empty());
    assert_eq!(std::fs::metadata(&j).unwrap().len(), len - 8);
}

#[test]
fn a_bad_journal_line_before_good_ones_stops_there_and_is_said() {
    let dir = tempfile::tempdir().unwrap();
    committed_record(dir.path());
    let j = only(dir.path(), "journal-");
    let text = std::fs::read_to_string(&j).unwrap();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    assert!(lines.len() >= 3, "{}", lines.len());
    let mut bad = lines.clone();
    let broken = lines[1].replacen("\"h\":\"", "\"h\":\"f", 1).replacen('0', "", 1);
    bad[1] = &broken;
    std::fs::write(&j, bad.concat()).unwrap();
    let o = open(dir.path());
    // The state up to the bad line: the first commit's and the first line's.
    assert!(o.state.files.len() < 6 && !o.state.files.is_empty(), "{}", o.state.files.len());
    assert!(o.notices.iter().any(|m| m.contains("Part of sync's record")), "{:?}", o.notices);
    assert!(
        std::fs::read_dir(state_dir(dir.path(), NAME))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".jsonl.unreadable"))
    );
    let state = o.state.clone();
    drop(o);
    let o = open(dir.path());
    assert_eq!(o.state, state, "saved as a new checkpoint");
    assert!(o.notices.is_empty());
}

#[test]
fn a_damaged_pack_record_is_a_missing_base_and_said_once() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = committed_record(dir.path());
    let pack = only(dir.path(), "bases-");
    let mut bytes = std::fs::read(&pack).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x55;
    std::fs::write(&pack, &bytes).unwrap();
    let o = open(dir.path());
    assert_eq!(o.state, state);
    let got: Vec<Option<String>> = live_prints(&state).iter().map(|p| o.bases.get(p).unwrap()).collect();
    assert_eq!(got.iter().filter(|g| g.is_none()).count(), 1, "one base is missing, the others are there");
    assert!(o.bases.take_notice().is_some());
    assert!(o.bases.take_notice().is_none(), "said once");
    // A short pack: its records past the end are missing, not an error.
    std::fs::write(&pack, &bytes[..10]).unwrap();
    let o = open(dir.path());
    assert!(live_prints(&state).iter().all(|p| o.bases.get(p).is_ok()));
}

#[test]
fn a_record_from_a_newer_quire_is_an_error_not_a_fresh_start() {
    let dir = tempfile::tempdir().unwrap();
    committed_record(dir.path());
    let ck = only(dir.path(), "checkpoint-");
    let text = std::fs::read_to_string(&ck).unwrap().replacen("{\"format\":1,", "{\"format\":2,", 1);
    std::fs::write(&ck, text).unwrap();
    let e = Journal::open(dir.path(), NAME).unwrap_err();
    assert!(e.contains("newer Quire"), "{e}");
}

#[test]
fn leftovers_are_tidied_only_after_the_record_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, texts) = committed_record(dir.path());
    let d = state_dir(dir.path(), NAME);
    let pack = only(dir.path(), "bases-");
    let len = std::fs::metadata(&pack).unwrap().len();
    OpenOptions::new().append(true).open(&pack).unwrap().write_all(b"half a record").unwrap();
    std::fs::write(d.join(pack_name(90)), b"a pack no record names").unwrap();
    std::fs::write(d.join(journal_name(0)), b"an old journal").unwrap();
    let o = open(dir.path());
    assert_eq!(o.state, state);
    assert_eq!(std::fs::metadata(&pack).unwrap().len(), len, "cut back to what the record names");
    assert!(!d.join(pack_name(90)).exists() && !d.join(journal_name(0)).exists());
    assert_texts(&o.state, &o.bases, &texts);
}

#[test]
fn records_move_with_the_selection_and_go_with_sync() {
    let dir = tempfile::tempdir().unwrap();
    committed_record(dir.path());
    rename_record(dir.path(), NAME, "state-partial.json").unwrap();
    assert!(!state_dir(dir.path(), NAME).exists() && state_dir(dir.path(), "state-partial.json").exists());
    assert_eq!(peek(dir.path(), "state-partial.json").files.len(), 6);
    assert!(exists(dir.path(), "state-partial.json"));
    remove(dir.path(), "state-partial.json");
    assert!(!exists(dir.path(), "state-partial.json"));
}
