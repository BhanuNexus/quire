// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use super::*;
use crate::{MemFs, Result, Vault, VaultError};

const PASS: &str = "correct horse";
const SECRET: &str = "MARKER-diary-7f3a";
const SECRET2: &str = "MARKER-photo-91c2";

fn np(s: &str) -> NotePath {
    NotePath::new(s)
}

fn vault_with(files: &[(&str, &str)]) -> (Vault, Arc<MemFs>) {
    let fs = Arc::new(MemFs::with_files(files.iter().copied()));
    (Vault::new(Arc::clone(&fs) as Arc<dyn Storage>), fs)
}

fn journal() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Journal/Day 1.md", "# Day 1\nMARKER-diary-7f3a\n"),
        ("Journal/Trips/Lisbon.md", "# Lisbon\nWent to [[Day 1]].\n"),
        ("Journal/photo.jpg", "MARKER-photo-91c2"),
        ("Plain.md", "# Plain\nSee [[Day 1]].\n"),
        ("Templates/Daily.md", "# {{title}}\n"),
    ]
}

fn lock(v: &Vault) -> ConvertReport {
    lock_folder(v, "Journal", Some(PASS), Kdf::TEST, 1).unwrap()
}

#[test]
fn locking_encrypts_every_file_and_hides_the_folder_from_notes() {
    let (v, fs) = vault_with(&journal());
    let r = lock(&v);
    assert_eq!(r.files, 3);
    let rk = r.recovery_key.expect("a new folder shows its recovery key");
    let listed: Vec<String> = fs.list().unwrap().iter().map(|p| p.as_str().to_string()).collect();
    assert!(listed.contains(&"Journal/Day 1.md.qenc".to_string()) && listed.contains(&"Journal/quire-lock.json".to_string()));
    assert!(!listed.iter().any(|p| p == "Journal/Day 1.md" || p == "Journal/photo.jpg"), "{listed:?}");
    for p in fs.list().unwrap() {
        let b = fs.read_bytes(&p).unwrap();
        assert!(!contains(&b, SECRET) && !contains(&b, SECRET2), "{p} holds plain text");
    }
    // The index never sees a locked note, locked or not.
    let notes: Vec<String> = v.notes().unwrap().iter().map(|p| p.as_str().to_string()).collect();
    assert_eq!(notes, ["Plain.md"]);
    assert_eq!(v.locked_notes("Journal"), [np("Journal/Day 1.md"), np("Journal/Trips/Lisbon.md")]);
    // Unlocked (the converting vault keeps the key): read and write by plain name.
    assert!(v.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET));
    assert_eq!(v.read_file(&np("Journal/photo.jpg")).unwrap(), SECRET2.as_bytes());
    v.write(&np("Journal/Day 1.md"), "# Day 1\nMARKER-diary-7f3a edited\n").unwrap();
    assert!(!fs.exists(&np("Journal/Day 1.md")));
    // Locked: nothing reads, nothing writes, names stay listed.
    assert!(v.lock_folder_now("Journal"));
    assert!(matches!(v.read(&np("Journal/Day 1.md")), Err(VaultError::Locked(f)) if f == "Journal"));
    assert!(matches!(v.write(&np("Journal/New.md"), "x"), Err(VaultError::Locked(_))));
    assert!(v.exists(&np("Journal/Day 1.md")));
    assert_eq!(v.locked_notes("Journal").len(), 2);
    // Other vault copies (another process) have no keys either.
    assert!(matches!(v.without_keys().read(&np("Journal/Trips/Lisbon.md")), Err(VaultError::Locked(_))));
    // Wrong passphrase, then the right one, then the recovery key.
    assert!(matches!(v.unlock("Journal", "wrong horse"), Err(VaultError::Locked(m)) if m.contains("wrong passphrase")));
    v.unlock("Journal", PASS).unwrap();
    assert!(v.read(&np("Journal/Day 1.md")).unwrap().contains("edited"));
    v.lock_folder_now("Journal");
    let bad = FolderKey::generate().unwrap();
    assert!(v.unlock_with_key("Journal", bad).is_err());
    v.unlock_with_key("Journal", FolderKey::from_recovery_key(&rk).unwrap()).unwrap();
    assert!(v.read(&np("Journal/Trips/Lisbon.md")).unwrap().contains("Lisbon"));
}

#[test]
fn nothing_plain_can_be_written_into_a_locked_folder() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    // Through the storage directly (another program's write path, the CLI, sync).
    for p in ["Journal/x.md", "Journal/Trips/y.md", "Journal/z.png"] {
        assert!(matches!(fs.write_atomic(&np(p), SECRET), Err(VaultError::Locked(_))), "{p}");
    }
    assert!(matches!(fs.rename(&np("Plain.md"), &np("Journal/Plain.md")), Err(VaultError::Locked(_))));
    // A nested key file is refused too; encrypted files and app data aren't.
    assert!(fs.write_atomic(&np("Journal/Trips/quire-lock.json"), "{}").is_err());
    assert!(fs.write_atomic(&np("Journal/x.md.qenc"), "ciphertext").is_ok());
    assert!(fs.write_atomic(&np(".quire/cache/x"), "ok").is_ok());
    // Creating a note there needs the folder unlocked and encrypts it.
    let p = v.create_note("Journal", "Today", "MARKER-diary-7f3a new").unwrap();
    assert_eq!(p, np("Journal/Today.md"));
    assert!(fs.exists(&np("Journal/Today.md.qenc")) && !fs.exists(&p));
    assert_eq!(v.unique_path("Journal", "Today").unwrap(), np("Journal/Today 1.md"), "names count encrypted files");
    v.lock_folder_now("Journal");
    assert!(v.create_note("Journal", "Later", "x").is_err());
}

#[test]
fn history_of_a_locked_note_is_encrypted() {
    let (v, fs) = vault_with(&journal());
    let day = np("Journal/Day 1.md");
    v.snapshot(&day, "# Day 1\nMARKER-diary-7f3a old version\n", 1_000, true).unwrap();
    v.snapshot(&day, "# Day 1\nMARKER-diary-7f3a\n", 900_000, true).unwrap();
    assert_eq!(v.history(&day).len(), 2);
    let r = lock(&v);
    assert_eq!(r.history, 2);
    let h = v.history(&day);
    assert_eq!(h.len(), 2);
    assert!(h.iter().all(|s| s.file.as_str().ends_with(".md.gz.qenc")));
    assert!(v.read_snapshot(&h[1]).unwrap().contains("old version"));
    // New saves write encrypted snapshots, and no provenance (it holds a hash of the text).
    let prov = serde_json::json!({"actor": "x"});
    assert!(v.snapshot_with(&day, "# Day 1\nMARKER-diary-7f3a newest\n", 2_000_000, true, Some(&prov)).unwrap());
    let h = v.history(&day);
    assert_eq!(h.len(), 3);
    assert!(is_encrypted_name(h[0].file.as_str()) && h[0].prov_file.is_none());
    assert_eq!(v.read_snapshot(&h[0]).unwrap(), "# Day 1\nMARKER-diary-7f3a newest\n");
    assert_no_marker_in(&fs);
    v.lock_folder_now("Journal");
    assert!(matches!(v.read_snapshot(&h[0]), Err(VaultError::Locked(_))));
    assert!(v.snapshot(&day, "typed while locked?", 3_000_000, true).is_err(), "no plain snapshot of a locked note");
    // Unlocking for good brings it back as plain history.
    let r = unlock_folder_permanently(&v, "Journal", Some(PASS)).unwrap();
    assert_eq!((r.files, r.history), (3, 3));
    let h = v.history(&day);
    assert!(h.iter().all(|s| s.file.as_str().ends_with(".md.gz")), "{h:?}");
    assert_eq!(v.read_snapshot(&h[2]).unwrap(), "# Day 1\nMARKER-diary-7f3a old version\n");
    assert!(v.notes().unwrap().contains(&day), "a plain folder again");
    assert!(fs.trashed().iter().any(|p| p.as_str() == "Journal/Day 1.md.qenc"), "the encrypted files are the backup");
    assert!(fs.trashed().iter().any(|p| p.as_str() == "Journal/quire-lock.unlocking.json"));
    assert!(v.locked_folders().is_empty());
}

#[test]
fn damaged_moved_and_swapped_files_are_reported() {
    let (v, fs) = vault_with(&[("A/x.md", "MARKER-a"), ("B/y.md", "MARKER-b"), ("A/z.md", "zzz")]);
    lock_folder(&v, "A", Some(PASS), Kdf::TEST, 1).unwrap();
    lock_folder(&v, "B", Some(PASS), Kdf::TEST, 1).unwrap();
    let x = np("A/x.md.qenc");
    let good = fs.read_bytes(&x).unwrap();
    // A flipped bit.
    let mut bad = good.clone();
    let n = bad.len() - 3;
    bad[n] ^= 1;
    fs.write_bytes_atomic(&x, &bad).unwrap();
    assert!(matches!(v.read(&np("A/x.md")), Err(VaultError::Damaged { reason, .. }) if reason.contains("damaged")));
    // Cut short.
    fs.write_bytes_atomic(&x, &good[..30]).unwrap();
    assert!(matches!(v.read(&np("A/x.md")), Err(VaultError::Damaged { reason, .. }) if reason.contains("cut short")));
    fs.write_bytes_atomic(&x, &good).unwrap();
    // Moved into another locked folder: refused, not misread.
    fs.write_bytes_atomic(&np("B/x.md.qenc"), &good).unwrap();
    assert!(matches!(v.read(&np("B/x.md")), Err(VaultError::Damaged { reason, .. }) if reason.contains("another locked folder")));
    // A history snapshot passed off as a note.
    v.snapshot(&np("A/z.md"), "zzz v2", 10, true).unwrap();
    let snap = v.history(&np("A/z.md"))[0].file.clone();
    fs.write_bytes_atomic(&x, &fs.read_bytes(&snap).unwrap()).unwrap();
    assert!(matches!(v.read(&np("A/x.md")), Err(VaultError::Damaged { .. })));
    // Never shown as empty text.
    assert!(v.read(&np("A/x.md")).is_err());
}

#[test]
fn passphrase_change_recovery_and_a_lost_key_file() {
    let (v, fs) = vault_with(&journal());
    let rk = lock(&v).recovery_key.unwrap();
    v.change_passphrase("Journal", None, "a new passphrase", 5).unwrap();
    assert!(v.change_passphrase("Journal", None, "short", 5).is_err());
    v.lock_folder_now("Journal");
    assert!(v.unlock("Journal", PASS).is_err());
    v.unlock("Journal", "a new passphrase").unwrap();
    v.lock_folder_now("Journal");
    // The key file is lost: the folder is still known as locked, and the recovery key alone rebuilds it.
    fs.remove(&np("Journal/quire-lock.json")).unwrap();
    assert!(v.locked_folders().iter().any(|f| f.folder.path == "Journal" && f.key_missing));
    let key = FolderKey::from_recovery_key(&rk).unwrap();
    v.change_passphrase("Journal", Some(key), "rebuilt passphrase", 6).unwrap();
    v.unlock("Journal", "rebuilt passphrase").unwrap();
    assert!(v.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET));
}

#[test]
fn moving_notes_in_out_and_within() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    v.snapshot(&np("Plain.md"), "# Plain\nMARKER-plain-history\n", 10, true).unwrap();
    // In: encrypted, plain original removed (not trashed), history encrypted.
    let r = v.move_note(&np("Plain.md"), "Journal").unwrap();
    assert_eq!(r.new_path, np("Journal/Plain.md"));
    assert!(fs.exists(&np("Journal/Plain.md.qenc")) && !fs.exists(&np("Plain.md")));
    assert!(fs.trashed().is_empty());
    assert!(is_encrypted_name(v.history(&np("Journal/Plain.md"))[0].file.as_str()));
    // Within: the encrypted file is renamed.
    v.rename_note(&np("Journal/Plain.md"), "Renamed").unwrap();
    assert!(fs.exists(&np("Journal/Renamed.md.qenc")));
    assert!(v.read(&np("Journal/Renamed.md")).unwrap().contains("Plain"));
    // Out: decrypted, the encrypted file to the trash, history decrypted.
    v.move_note(&np("Journal/Renamed.md"), "").unwrap();
    assert!(v.read(&np("Renamed.md")).unwrap().contains("Plain"));
    assert!(v.history(&np("Renamed.md"))[0].file.as_str().ends_with(".md.gz"));
    // Locked: no moves.
    v.lock_folder_now("Journal");
    assert!(v.move_note(&np("Journal/Day 1.md"), "").is_err());
    assert!(v.move_note(&np("Renamed.md"), "Journal").is_err());
}

#[test]
fn refuses_folders_it_cant_lock() {
    let (v, _) = vault_with(&journal());
    for bad in ["", "/", ".quire", "Templates", "Templates/Sub", "a/../b"] {
        assert!(refuse_to_lock(&v, bad).is_some(), "{bad:?}");
    }
    // 0.14, quire_base::path::Rule::VaultFolder: a drive and control
    // characters too (outside the vault, or a name no system shows).
    for bad in ["C:/x", "c:x", "a\u{7}b"] {
        assert!(refuse_to_lock(&v, bad).is_some(), "{bad:?}");
    }
    lock(&v);
    assert!(refuse_to_lock(&v, "Journal/Trips").is_some(), "inside");
    assert!(refuse_to_lock(&v, "Journal").is_none(), "again: finishes");
    let (v2, _) = vault_with(&[("A/B/x.md", "x")]);
    lock_folder(&v2, "A/B", Some(PASS), Kdf::TEST, 1).unwrap();
    assert!(refuse_to_lock(&v2, "A").is_some(), "contains");
    assert!(lock_folder(&v2, "Z", Some("short"), Kdf::TEST, 1).is_err(), "weak passphrase");
}

// ------------------------------------------------------------ interruption

/// A storage that fails every change after the first `n`, as if the
/// process stopped there (each change is atomic, as on disk).
struct Crash {
    inner: Arc<MemFs>,
    left: AtomicUsize,
}

impl Crash {
    fn step(&self) -> Result<()> {
        if self.left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_err() {
            return Err(VaultError::Io { path: "crash".into(), source: std::io::Error::other("the process stopped here") });
        }
        Ok(())
    }
}

impl Storage for Crash {
    fn list(&self) -> Result<Vec<NotePath>> {
        self.inner.list()
    }
    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        self.inner.read_bytes(path)
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> Result<()> {
        self.step()?;
        self.inner.write_bytes_atomic(path, bytes)
    }
    fn rename(&self, from: &NotePath, to: &NotePath) -> Result<()> {
        self.step()?;
        self.inner.rename(from, to)
    }
    fn trash(&self, path: &NotePath) -> Result<()> {
        self.step()?;
        self.inner.trash(path)
    }
    fn exists(&self, path: &NotePath) -> bool {
        self.inner.exists(path)
    }
    fn modified(&self, path: &NotePath) -> Result<SystemTime> {
        self.inner.modified(path)
    }
    fn absolute(&self, _: &NotePath) -> Option<PathBuf> {
        None
    }
    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        self.inner.list_under(folder)
    }
    fn remove(&self, path: &NotePath) -> Result<()> {
        self.step()?;
        self.inner.remove(path)
    }
    fn set_locked_folders(&self, folders: &[String]) {
        self.inner.set_locked_folders(folders);
    }
}

/// Each original file, readable with the passphrase in one form or the other.
fn all_readable(fs: &Arc<MemFs>, originals: &[(&str, &str)]) {
    let v = Vault::new(Arc::clone(fs) as Arc<dyn Storage>);
    if v.key_file("Journal").is_ok() {
        v.unlock("Journal", PASS).unwrap();
    }
    for (p, text) in originals {
        let got = v.read_file(&np(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
        assert_eq!(got, text.as_bytes(), "{p}");
    }
}

#[test]
fn locking_interrupted_at_any_step_loses_nothing_and_finishes() {
    let files = journal();
    let day = np("Journal/Day 1.md");
    let mut n = 0;
    loop {
        let fs = Arc::new(MemFs::with_files(files.iter().copied()));
        let plain = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        plain.snapshot(&day, "# Day 1\nMARKER-diary-7f3a v0\n", 1, true).unwrap();
        let crashing = Vault::new(Arc::new(Crash { inner: Arc::clone(&fs), left: AtomicUsize::new(n) }));
        let done = lock_folder(&crashing, "Journal", Some(PASS), Kdf::TEST, 1).is_ok();
        all_readable(&fs, &files);
        // Resume, as a fresh process would (no keys).
        let v = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        if fs.exists(&np("Journal/quire-lock.json")) || !done {
            lock_folder(&v, "Journal", Some(PASS), Kdf::TEST, 1).unwrap();
        }
        all_readable(&fs, &files);
        assert_no_marker_in(&fs);
        assert!(plain_left(&v).is_empty());
        assert_eq!(v.history(&day).len(), 1);
        assert!(v.read_snapshot(&v.history(&day)[0]).unwrap().ends_with("v0\n"));
        if done {
            break;
        }
        n += 1;
        assert!(n < 500, "the conversion never finishes");
    }
    assert!(n > 8, "the conversion has steps to interrupt ({n})");
}

#[test]
fn unlocking_for_good_interrupted_at_any_step_loses_nothing_and_finishes() {
    let files = journal();
    let mut n = 0;
    loop {
        let fs = Arc::new(MemFs::with_files(files.iter().copied()));
        let v = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        v.snapshot(&np("Journal/Day 1.md"), "# Day 1\nv0\n", 1, true).unwrap();
        lock_folder(&v, "Journal", Some(PASS), Kdf::TEST, 1).unwrap();
        let crashing = Vault::new(Arc::new(Crash { inner: Arc::clone(&fs), left: AtomicUsize::new(n) }));
        let done = unlock_folder_permanently(&crashing, "Journal", Some(PASS)).is_ok();
        all_readable(&fs, &files);
        let v = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        if !done {
            unlock_folder_permanently(&v, "Journal", Some(PASS)).unwrap();
        }
        assert!(v.locked_folders().is_empty());
        for (p, text) in &files {
            assert_eq!(fs.read_bytes(&np(p)).unwrap(), text.as_bytes(), "{p} plain again");
        }
        assert!(!fs.list().unwrap().iter().any(|p| is_encrypted_name(p.as_str())));
        assert_eq!(v.read_snapshot(&v.history(&np("Journal/Day 1.md"))[0]).unwrap(), "# Day 1\nv0\n");
        if done {
            break;
        }
        n += 1;
        assert!(n < 500, "the conversion never finishes");
    }
    assert!(n > 5, "{n}");
}

// ------------------------------------------------------------ plain text never reaches the disk

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Also looks inside gzip files (history snapshots): compressed plain text
/// is plain text.
fn leaks(bytes: &[u8], marker: &str) -> bool {
    if contains(bytes, marker) {
        return true;
    }
    let mut out = Vec::new();
    bytes.starts_with(&[0x1f, 0x8b]) && flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).is_ok() && contains(&out, marker)
}

fn assert_no_marker_in(fs: &MemFs) {
    for p in fs.list_all() {
        let b = fs.read_bytes(&p).unwrap();
        for m in ["MARKER-diary", SECRET2] {
            assert!(!leaks(&b, m), "{p} holds plain text of a locked note");
        }
    }
}

/// Every file under `root`, `.quire/` and the trash included.
pub(crate) fn scan_for(root: &Path, markers: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if let Ok(b) = std::fs::read(&p) {
                for m in markers {
                    if leaks(&b, m) {
                        found.push(format!("{} ({m})", p.display()));
                    }
                }
            }
        }
    }
    found
}

#[test]
fn on_a_real_disk_no_plain_text_after_edits_saves_history_and_moves() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (p, text) in journal() {
        let abs = root.join(p);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, text).unwrap();
    }
    let v = Vault::open_local(root).unwrap();
    lock(&v);
    let day = np("Journal/Day 1.md");
    for i in 0..5 {
        let text = format!("# Day 1\nMARKER-diary-7f3a edit {i}\n");
        v.write(&day, &text).unwrap();
        v.snapshot(&day, &text, i64::from(i) * 600_000, true).unwrap();
    }
    v.create_note("Journal", "MARKER-diary title is visible", "MARKER-diary-7f3a body").unwrap();
    v.move_note(&np("Plain.md"), "Journal").unwrap();
    v.rename_note(&np("Journal/Plain.md"), "Moved").unwrap();
    v.trash(&np("Journal/Trips/Lisbon.md")).unwrap();
    v.lock_folder_now("Journal");
    // File names aren't protected (documented); contents are.
    let found: Vec<String> = scan_for(root, &[SECRET, SECRET2, "edit 3"]).into_iter().collect();
    assert!(found.is_empty(), "{found:#?}");
    assert!(
        std::fs::read_dir(root.join("Journal")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("MARKER-diary title"))
    );
}

#[test]
fn folders_in_a_listing() {
    let l: Vec<NotePath> =
        ["A/quire-lock.json", "A/B/quire-lock.json", "C/D/quire-lock.unlocking.json", "E/x.md", "F/quire-lock.json.bak"].map(np).to_vec();
    let f = folders_in(&l);
    assert_eq!(
        f,
        [LockedFolder { path: "A".into(), unlocking: false }, LockedFolder { path: "C/D".into(), unlocking: true }],
        "nested ones are part of the outer one"
    );
    assert!(in_any(&f, "A/B/x.md") && in_any(&f, "C/D/y.md") && !in_any(&f, "C/y.md") && !in_any(&f, "AB/x.md"));
    assert_eq!(plain_name(&np("J/a.md.qenc")), Some(np("J/a.md")));
    assert_eq!(plain_name(&np("J/a.md")), None);
    assert!(is_key_file("J/quire-lock.json") && !is_key_file("J/quire-lock.json.bak"));
}

#[test]
fn renaming_a_locked_note_updates_links_in_plain_notes() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    let r = v.rename_note(&np("Journal/Day 1.md"), "First day").unwrap();
    assert_eq!(r.new_path, np("Journal/First day.md"));
    assert!(fs.exists(&np("Journal/First day.md.qenc")) && !fs.exists(&np("Journal/Day 1.md.qenc")));
    assert_eq!(r.updated, [np("Plain.md")]);
    assert_eq!(v.read(&np("Plain.md")).unwrap(), "# Plain\nSee [[First day]].\n");
    assert!(v.read(&np("Journal/First day.md")).unwrap().contains(SECRET));
}

// ------------------------------------------------------------ review 0.11 (security S-H1, S-H2, S-M7, S-L5; data H1, M8)

/// A fresh process on the same disk (its own storage, no keys), which
/// remembers the vault's locked folders from `local`.
fn reopen(root: &Path, local: &Path) -> Vault {
    let v = Vault::open_local(root).unwrap();
    v.remember_locked_folders(Some(local));
    v
}

fn disk_with_journal() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    for (p, text) in journal() {
        let abs = root.join(p);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, text).unwrap();
    }
    (dir, root)
}

#[test]
fn deleting_the_key_file_never_turns_a_locked_folder_plain() {
    let (dir, root) = disk_with_journal();
    let local = dir.path().join("device/locked-folders.json");
    let v = reopen(&root, &local);
    lock(&v);
    let day = np("Journal/Day 1.md");
    // Someone with write access deletes the key file while the note is open here.
    std::fs::remove_file(root.join("Journal/quire-lock.json")).unwrap();
    v.write(&day, "# Day 1\nMARKER-diary-7f3a typed after\n").unwrap();
    assert!(!root.join("Journal/Day 1.md").exists(), "the save stays encrypted");
    v.create_note("Journal", "New", "MARKER-diary-7f3a new note").unwrap();
    assert!(root.join("Journal/New.md.qenc").exists() && !root.join("Journal/New.md").exists());
    assert!(v.locked_folders().iter().any(|f| f.folder.path == "Journal" && f.key_missing));
    // The storage itself refuses too, for a note that has an encrypted copy and for a new one.
    let fs = v.storage();
    assert!(matches!(fs.write_atomic(&day, "plain"), Err(VaultError::Locked(_))));
    assert!(matches!(fs.write_atomic(&np("Journal/Other.md"), "plain"), Err(VaultError::Locked(_))));
    // A fresh process (this device's own record; the vault's record deleted too) refuses plain writes.
    std::fs::remove_file(root.join(SHARED_RECORD)).unwrap();
    let v2 = reopen(&root, &local);
    assert_eq!(v2.known_locked().folders().len(), 1);
    assert!(matches!(v2.write(&np("Journal/Later.md"), "MARKER-diary-7f3a"), Err(VaultError::Locked(_))));
    assert!(matches!(v2.storage().write_atomic(&np("Journal/Later.md"), "x"), Err(VaultError::Locked(_))));
    assert!(!v2.notes().unwrap().iter().any(|p| p.as_str().starts_with("Journal/")));
    // The vault's record was brought back from this device's.
    assert!(root.join(SHARED_RECORD).exists());
    let found = scan_for(&root, &[SECRET, "typed after"]);
    assert!(found.is_empty(), "{found:#?}");
    drop(dir);
}

#[test]
fn a_planted_unlocking_file_is_not_an_unlock() {
    let (dir, root) = disk_with_journal();
    let local = dir.path().join("device/locked-folders.json");
    let v = reopen(&root, &local);
    lock(&v);
    std::fs::rename(root.join("Journal/quire-lock.json"), root.join("Journal/quire-lock.unlocking.json")).unwrap();
    let f = v.locked_folder(&np("Journal/Day 1.md")).unwrap();
    assert!(!f.unlocking, "this process didn't start unlocking it");
    v.write(&np("Journal/Day 1.md"), "# Day 1\nMARKER-diary-7f3a after the rename\n").unwrap();
    assert!(!root.join("Journal/Day 1.md").exists());
    assert!(v.read(&np("Journal/Day 1.md")).unwrap().contains("after the rename"));
    let v2 = reopen(&root, &local);
    assert!(matches!(v2.write(&np("Journal/Day 1.md"), "x"), Err(VaultError::Locked(_))));
    assert!(matches!(v2.storage().write_atomic(&np("Journal/Day 9.md"), "x"), Err(VaultError::Locked(_))));
    // Unlocking it for good, as the person asks, still works (and finishes the rename).
    unlock_folder_permanently(&v2, "Journal", Some(PASS)).unwrap();
    assert!(v2.read(&np("Journal/Day 1.md")).unwrap().contains("after the rename"));
    assert!(reopen(&root, &local).known_locked().folders().is_empty());
    assert!(scan_for(&root.join("Journal"), &["after the rename"]).len() == 1, "plain again, as asked");
    drop(dir);
}

#[test]
fn a_folder_unlocked_for_good_elsewhere_is_forgotten_but_not_while_anything_is_left() {
    let (dir, root) = disk_with_journal();
    let a_local = dir.path().join("a/locked-folders.json");
    let b_local = dir.path().join("b/locked-folders.json");
    let a = reopen(&root, &a_local);
    lock(&a);
    let b = reopen(&root, &b_local);
    assert!(b.known_locked().contains("Journal"));
    // Only the key file gone: B keeps refusing.
    let kf = std::fs::read(root.join("Journal/quire-lock.json")).unwrap();
    std::fs::remove_file(root.join("Journal/quire-lock.json")).unwrap();
    assert_eq!(reopen(&root, &b_local).remember_locked_folders(Some(&b_local)), ["Journal"]);
    std::fs::write(root.join("Journal/quire-lock.json"), kf).unwrap();
    // A unlocks it for good: B forgets it when it next opens the vault.
    unlock_folder_permanently(&a, "Journal", Some(PASS)).unwrap();
    let b = reopen(&root, &b_local);
    assert!(b.known_locked().folders().is_empty());
    b.write(&np("Journal/Plain now.md"), "x").unwrap();
    drop(dir);
}

#[test]
fn a_plain_file_planted_next_to_an_encrypted_note_never_replaces_it() {
    let (dir, root) = disk_with_journal();
    let v = Vault::open_local(&root).unwrap();
    lock(&v);
    let day = np("Journal/Day 1.md");
    // Planted around Quire's refusal (another program, a device without locked folders).
    std::fs::write(root.join("Journal/Day 1.md"), "ATTACKER TEXT").unwrap();
    // Read: the encrypted note.
    let text = v.read(&day).unwrap();
    assert!(text.contains(SECRET), "{text}");
    // Saved over what was read: the encrypted note; the plain file is left alone.
    assert!(v.write_if(&day, Some(&text), "# Day 1\nMARKER-diary-7f3a saved\n").unwrap());
    assert_eq!(std::fs::read_to_string(root.join("Journal/Day 1.md")).unwrap(), "ATTACKER TEXT");
    assert!(v.read(&day).unwrap().contains("saved"));
    // Reported as a conflict, not as a file to encrypt.
    let left = plain_left(&v);
    assert_eq!(left.len(), 1);
    assert_eq!((left[0].files.len(), left[0].conflicts.clone()), (0, vec![day.clone()]));
    // Locking again doesn't seal it over the note: both stay.
    let r = lock_folder(&v, "Journal", Some(PASS), Kdf::TEST, 2).unwrap();
    assert_eq!(r.conflicts, std::slice::from_ref(&day));
    assert!(v.read(&day).unwrap().contains("saved"));
    assert!(root.join("Journal/Day 1.md").exists());
    // Trashing the note moves its encrypted file, never the planted text over it.
    // Keeping both: the plain one is encrypted under a new name.
    let kept = keep_both(&v, "Journal").unwrap();
    assert_eq!(kept, [np("Journal/Day 1 (plain copy).md")]);
    assert_eq!(v.read(&kept[0]).unwrap(), "ATTACKER TEXT");
    assert!(!root.join("Journal/Day 1.md").exists() && v.read(&day).unwrap().contains("saved"));
    assert!(plain_left(&v).is_empty());
    drop(dir);
}

#[test]
fn an_interrupted_lock_keeps_a_newer_encrypted_edit_from_another_device() {
    // Device A's lock stops after every file is encrypted, before the plain ones are removed.
    let files = journal();
    let mut n = 0;
    let fs = loop {
        let fs = Arc::new(MemFs::with_files(files.iter().copied()));
        let crashing = Vault::new(Arc::new(Crash { inner: Arc::clone(&fs), left: AtomicUsize::new(n) }));
        let _ = lock_folder(&crashing, "Journal", Some(PASS), Kdf::TEST, 1);
        let all = ["Journal/Day 1.md", "Journal/Trips/Lisbon.md", "Journal/photo.jpg"];
        if all.iter().all(|p| fs.exists(&np(p)) && fs.exists(&stored(&np(p)))) {
            break fs;
        }
        n += 1;
        assert!(n < 200);
    };
    // Device B (another vault, same files) edits the note: a new encrypted copy.
    let b = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    b.unlock("Journal", PASS).unwrap();
    let day = np("Journal/Day 1.md");
    assert!(b.read(&day).unwrap().contains(SECRET), "the encrypted copy is the note");
    b.write(&day, "# Day 1\nv2 from device B\n").unwrap();
    // A resumes: its plain original is what it recorded, so the newer encrypted copy wins.
    let a = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    let r = lock_folder(&a, "Journal", Some(PASS), Kdf::TEST, 2).unwrap();
    assert!(r.conflicts.is_empty(), "{r:?}");
    assert_eq!(a.read(&day).unwrap(), "# Day 1\nv2 from device B\n");
    assert!(plain_left(&a).is_empty());
    assert_no_marker_in(&fs);
}

#[test]
fn unlocking_for_good_keeps_both_when_the_plain_file_differs() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    let day = np("Journal/Day 1.md");
    fs.write_atomic(&np("Journal/quire-lock.unlocking.json"), &fs.read(&np("Journal/quire-lock.json")).unwrap()).unwrap();
    fs.remove(&np("Journal/quire-lock.json")).unwrap();
    // As if an earlier run wrote it and it was edited since (or it came from elsewhere).
    let converting = v.converting("Journal");
    fs.write_atomic(&day, "other text").unwrap();
    drop(converting);
    let r = unlock_folder_permanently(&v, "Journal", Some(PASS)).unwrap();
    assert_eq!(r.conflicts, [np("Journal/Day 1 (encrypted copy).md")]);
    assert_eq!(fs.read(&day).unwrap(), "other text");
    assert!(fs.read(&np("Journal/Day 1 (encrypted copy).md")).unwrap().contains(SECRET));
}

#[test]
fn a_wrong_recovery_key_never_becomes_the_folder_key() {
    let (v, fs) = vault_with(&journal());
    let rk = lock(&v).recovery_key.unwrap();
    v.lock_folder_now("Journal");
    // Another device: the encrypted files arrived, the key file didn't.
    let b_fs = Arc::new(MemFs::new());
    for p in fs.list().unwrap().into_iter().filter(|p| is_encrypted_name(p.as_str())) {
        b_fs.write_bytes_atomic(&p, &fs.read_bytes(&p).unwrap()).unwrap();
    }
    let b = Vault::new(Arc::clone(&b_fs) as Arc<dyn Storage>);
    let wrong = FolderKey::generate().unwrap();
    let e = b.change_passphrase("Journal", Some(wrong), "new passphrase", 2).unwrap_err();
    assert!(e.to_string().contains("recovery key"), "{e}");
    assert!(!b_fs.exists(&np("Journal/quire-lock.json")), "nothing written");
    b.change_passphrase("Journal", Some(FolderKey::from_recovery_key(&rk).unwrap()), "new passphrase", 2).unwrap();
    b.unlock("Journal", "new passphrase").unwrap();
    assert!(b.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET));
    // A folder with no encrypted file and nothing remembered: refused, with the reason.
    let (c, _) = vault_with(&[("Empty/x.txt", "")]);
    let e = c.change_passphrase("Empty", Some(FolderKey::generate().unwrap()), "new passphrase", 2).unwrap_err();
    assert!(e.to_string().contains("no encrypted file"), "{e}");
    // An empty locked folder this device remembers: checked against the remembered check value.
    let (e_vault, efs) = vault_with(&[("Box/a.md", "a")]);
    let rk = lock_folder(&e_vault, "Box", Some(PASS), Kdf::TEST, 1).unwrap().recovery_key.unwrap();
    for p in efs.list().unwrap().into_iter().filter(|p| is_encrypted_name(p.as_str())) {
        efs.remove(&p).unwrap();
    }
    efs.remove(&np("Box/quire-lock.json")).unwrap();
    e_vault.lock_folder_now("Box");
    assert!(e_vault.change_passphrase("Box", Some(FolderKey::generate().unwrap()), "new passphrase", 3).is_err());
    assert!(!efs.exists(&np("Box/quire-lock.json")));
    e_vault.change_passphrase("Box", Some(FolderKey::from_recovery_key(&rk).unwrap()), "new passphrase", 3).unwrap();
    // The recovery key alone unlocks a folder whose key file is lost (checked against its files).
    let (f_vault, ffs) = vault_with(&journal());
    let rk = lock(&f_vault).recovery_key.unwrap();
    f_vault.lock_folder_now("Journal");
    ffs.remove(&np("Journal/quire-lock.json")).unwrap();
    assert!(f_vault.unlock_with_key("Journal", FolderKey::generate().unwrap()).is_err());
    f_vault.unlock_with_key("Journal", FolderKey::from_recovery_key(&rk).unwrap()).unwrap();
    assert!(f_vault.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET));
}

#[test]
fn locking_encrypts_the_history_of_notes_deleted_or_renamed_before() {
    let (v, fs) = vault_with(&journal());
    let old = np("Journal/Old.md");
    v.write(&old, "x").unwrap();
    v.snapshot(&old, "# Old\nMARKER-diary-7f3a deleted before locking\n", 10, true).unwrap();
    fs.remove(&old).unwrap();
    v.rename_note(&np("Journal/Trips/Lisbon.md"), "Porto").unwrap();
    v.snapshot(&np("Journal/Trips/Porto.md"), "# Lisbon\nMARKER-diary renamed\n", 20, true).unwrap();
    assert_eq!(v.history_paths_under("Journal").unwrap(), [old.clone(), np("Journal/Trips/Porto.md")]);
    lock(&v);
    assert_no_marker_in(&fs);
    assert!(v.read_snapshot(&v.history(&old)[0]).unwrap().contains("deleted before locking"));
}

#[test]
fn hidden_folders_inside_a_locked_folder_are_part_of_it() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    assert!(matches!(fs.write_atomic(&np("Journal/.drafts/x.md"), SECRET), Err(VaultError::Locked(_))));
    assert!(fs.write_atomic(&np(".quire/cache/y"), "ok").is_ok());
}

/// Security review S-M8: a new folder key, so an old key file with a leaked
/// passphrase (or the old recovery key) opens nothing written since.
#[test]
fn changing_the_key_shuts_out_old_key_files_and_old_recovery_keys() {
    let (v, fs) = vault_with(&journal());
    let day = np("Journal/Day 1.md");
    v.snapshot(&day, "# Day 1\nMARKER-diary-7f3a v0\n", 1, true).unwrap();
    let old_rk = lock(&v).recovery_key.unwrap();
    let old_key_file = fs.read(&np("Journal/quire-lock.json")).unwrap();
    let r = change_key(&v, "Journal", "a new passphrase", 9).unwrap();
    assert_eq!((r.files, r.history), (3, 1));
    let new_rk = r.recovery_key.unwrap();
    assert_ne!(new_rk, old_rk);
    // The old key (from the old key file and passphrase) opens none of the files now.
    let old_key = KeyFile::parse(&old_key_file).unwrap().unwrap_key(PASS).unwrap();
    let id = KeyFile::parse(&old_key_file).unwrap().id().unwrap();
    for p in fs.list().unwrap().into_iter().filter(|p| is_encrypted_name(p.as_str())) {
        assert!(old_key.open(Kind::File, id, &fs.read_bytes(&p).unwrap()).is_err(), "{p}");
    }
    let fresh = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    assert!(fresh.unlock("Journal", PASS).is_err());
    assert!(fresh.unlock_with_key("Journal", FolderKey::from_recovery_key(&old_rk).unwrap()).is_err());
    fresh.unlock("Journal", "a new passphrase").unwrap();
    assert!(fresh.read(&day).unwrap().contains(SECRET));
    assert!(fresh.read_snapshot(&fresh.history(&day)[0]).unwrap().contains("v0"));
    // A device still holding the old key writes a note: still readable here (the previous key is kept).
    let late = old_key.seal(Kind::File, id, b"written with the old key").unwrap();
    fs.write_bytes_atomic(&np("Journal/Late.md.qenc"), &late).unwrap();
    assert_eq!(fresh.read(&np("Journal/Late.md")).unwrap(), "written with the old key");
    assert_no_marker_in(&fs);
}

#[test]
fn changing_the_key_interrupted_at_any_step_finishes() {
    let files = journal();
    let mut n = 0;
    loop {
        let fs = Arc::new(MemFs::with_files(files.iter().copied()));
        let v = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        lock(&v);
        let crashing = Vault::new(Arc::new(Crash { inner: Arc::clone(&fs), left: AtomicUsize::new(n) }));
        crashing.unlock("Journal", PASS).unwrap();
        let done = change_key(&crashing, "Journal", "a new passphrase", 9).is_ok();
        // Whatever happened, every file opens with one passphrase or the other, and running it again finishes.
        let again = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        if again.unlock("Journal", "a new passphrase").is_err() {
            again.unlock("Journal", PASS).unwrap();
            assert!(!done);
            continue_or_finish(&again, &files, &fs);
        } else {
            for (p, text) in &files {
                assert_eq!(again.read_file(&np(p)).unwrap(), text.as_bytes(), "{p}");
            }
            if !done {
                change_key(&again, "Journal", "a new passphrase", 10).unwrap();
            }
            assert!(!again.key_file("Journal").unwrap().1.rotating);
        }
        if done {
            break;
        }
        n += 1;
        assert!(n < 200);
    }
    assert!(n > 3, "{n}");
}

fn continue_or_finish(v: &Vault, files: &[(&str, &str)], fs: &Arc<MemFs>) {
    // The key file wasn't replaced: nothing changed yet; change it now.
    let r = change_key(v, "Journal", "a new passphrase", 10).unwrap();
    assert!(r.recovery_key.is_some());
    for (p, text) in files {
        assert_eq!(v.read_file(&np(p)).unwrap(), text.as_bytes(), "{p}");
    }
    assert_no_marker_in(fs);
}

// ------------------------------------------------------------ 0.13.2

/// Reliability scan item 5: a key change that stops at any step leaves a
/// way in: before the key file changed, the old passphrase and recovery
/// key; after, the new passphrase and the new recovery key, which
/// `begin_change_key` hands over before any file changes. Running it
/// again with another new passphrase uses that one, never ignores it.
#[test]
fn a_key_change_that_stops_anywhere_always_leaves_a_way_in() {
    let files = journal();
    let mut n = 0;
    loop {
        let fs = Arc::new(MemFs::with_files(files.iter().copied()));
        let v = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        let old_rk = lock(&v).recovery_key.unwrap();
        let crashing = Vault::new(Arc::new(Crash { inner: Arc::clone(&fs), left: AtomicUsize::new(n) }));
        crashing.unlock("Journal", PASS).unwrap();
        let begun = begin_change_key(&crashing, "Journal", "a new passphrase", 9);
        let done = begun.as_ref().is_ok_and(|_| finish_change_key(&crashing, "Journal").is_ok());
        let fresh = || Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
        let rotating = fresh().key_file("Journal").unwrap().1.rotating;
        match &begun {
            // The key file wasn't changed: everything as before.
            Err(_) => {
                assert!(!rotating);
                fresh().unlock("Journal", PASS).unwrap();
                fresh().unlock_with_key("Journal", FolderKey::from_recovery_key(&old_rk).unwrap()).unwrap();
                all_readable(&fs, &files);
            }
            Ok(new_rk) => {
                assert_ne!(*new_rk, old_rk);
                // The new passphrase and the recovery key handed over both open every file.
                for way in 0..2 {
                    let w = fresh();
                    if way == 0 {
                        w.unlock("Journal", "a new passphrase").unwrap();
                    } else {
                        w.unlock_with_key("Journal", FolderKey::from_recovery_key(new_rk).unwrap()).unwrap();
                    }
                    for (p, text) in &files {
                        assert_eq!(w.read_file(&np(p)).unwrap(), text.as_bytes(), "{p} (n = {n})");
                    }
                }
                assert!(fresh().unlock("Journal", PASS).is_err(), "the old passphrase is replaced");
                if !done {
                    assert!(rotating);
                    // Run again with another new passphrase: that one is kept, the key stays.
                    let again = fresh();
                    again.unlock("Journal", "a new passphrase").unwrap();
                    let rk = begin_change_key(&again, "Journal", "another passphrase", 10).unwrap();
                    assert_eq!(rk, *new_rk, "the same new key");
                    finish_change_key(&again, "Journal").unwrap();
                    let last = fresh();
                    assert!(last.unlock("Journal", "a new passphrase").is_err());
                    last.unlock("Journal", "another passphrase").unwrap();
                    assert!(!last.key_file("Journal").unwrap().1.rotating);
                    for (p, text) in &files {
                        assert_eq!(last.read_file(&np(p)).unwrap(), text.as_bytes(), "{p}");
                    }
                    // Without the previous key, only the new key opens everything now.
                    let only_new = fresh();
                    only_new.unlock_with_key("Journal", FolderKey::from_recovery_key(new_rk).unwrap()).unwrap();
                    only_new.keys().set_previous("Journal", None);
                    for (p, text) in &files {
                        assert_eq!(only_new.read_file(&np(p)).unwrap(), text.as_bytes(), "{p}");
                    }
                }
            }
        }
        assert_no_marker_in(&fs);
        if done {
            break;
        }
        n += 1;
        assert!(n < 200);
    }
    assert!(n > 3, "{n}");
}

/// What [`Hook`] runs once.
type Before = Box<dyn FnOnce(&MemFs) + Send>;

/// A storage that runs `before` once, just before the first conditional
/// write to `when` (as if another writer got there first).
struct Hook {
    inner: Arc<MemFs>,
    when: NotePath,
    before: std::sync::Mutex<Option<Before>>,
}

impl Storage for Hook {
    fn list(&self) -> Result<Vec<NotePath>> {
        self.inner.list()
    }
    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        self.inner.read_bytes(path)
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> Result<()> {
        self.inner.write_bytes_atomic(path, bytes)
    }
    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        if *path == self.when
            && let Some(h) = self.before.lock().unwrap().take()
        {
            h(&self.inner);
        }
        self.inner.write_bytes_if(path, still, bytes)
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
    fn absolute(&self, _: &NotePath) -> Option<PathBuf> {
        None
    }
    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        self.inner.list_under(folder)
    }
    fn remove(&self, path: &NotePath) -> Result<()> {
        self.inner.remove(path)
    }
    fn set_locked_folders(&self, folders: &[String]) {
        self.inner.set_locked_folders(folders);
    }
}

fn hooked(files: &[(&str, &str)], when: &str) -> (Vault, Arc<MemFs>, Arc<Hook>) {
    let fs = Arc::new(MemFs::with_files(files.iter().copied()));
    let hook = Arc::new(Hook { inner: Arc::clone(&fs), when: np(when), before: std::sync::Mutex::new(None) });
    (Vault::new(Arc::clone(&hook) as Arc<dyn Storage>), fs, hook)
}

/// Reliability scan item 8: a save that lands while the folder is being
/// locked (the editor writes the encrypted file and removes the plain
/// one) is never written over, and doesn't stop the lock.
#[test]
fn a_save_during_locking_is_kept() {
    let (v, fs, hook) = hooked(&journal(), "Journal/Day 1.md.qenc");
    begin_lock(&v, "Journal", Some(PASS), Kdf::TEST, 1).unwrap();
    let (id, key) = v.keys().get("Journal").unwrap();
    let newer = key.seal(Kind::File, id, b"# Day 1\nsaved while locking\n").unwrap();
    *hook.before.lock().unwrap() = Some(Box::new(move |fs: &MemFs| {
        fs.write_bytes_atomic(&np("Journal/Day 1.md.qenc"), &newer).unwrap();
        fs.remove(&np("Journal/Day 1.md")).unwrap();
    }));
    lock_folder(&v, "Journal", Some(PASS), Kdf::TEST, 1).unwrap();
    assert_eq!(v.read(&np("Journal/Day 1.md")).unwrap(), "# Day 1\nsaved while locking\n");
    assert!(plain_left(&v).is_empty());
    assert_no_marker_in(&fs);
    // A plain file saved (encrypted) away before the lock reads it is skipped, not an error.
    let (v, _fs) = vault_with(&journal());
    begin_lock(&v, "Journal", Some(PASS), Kdf::TEST, 1).unwrap();
    v.write(&np("Journal/Day 1.md"), "# Day 1\nsaved first\n").unwrap();
    lock(&v);
    assert_eq!(v.read(&np("Journal/Day 1.md")).unwrap(), "# Day 1\nsaved first\n");
}

/// Reliability scan item 8: unlocking for good never writes over a plain
/// file saved meanwhile; both are kept.
#[test]
fn a_save_during_unlocking_for_good_is_kept() {
    let (v, fs, hook) = hooked(&journal(), "Journal/Day 1.md");
    lock(&v);
    *hook.before.lock().unwrap() = Some(Box::new(|fs: &MemFs| {
        fs.write_bytes_atomic(&np("Journal/Day 1.md"), b"saved while unlocking").unwrap();
    }));
    let r = unlock_folder_permanently(&v, "Journal", None).unwrap();
    assert_eq!(fs.read(&np("Journal/Day 1.md")).unwrap(), "saved while unlocking");
    assert_eq!(r.conflicts, [np("Journal/Day 1 (encrypted copy).md")]);
    assert!(fs.read(&np("Journal/Day 1 (encrypted copy).md")).unwrap().contains(SECRET));
}

/// Reliability scan item 8: a note trashed or moved during a key change
/// doesn't stop it.
#[test]
fn a_note_moved_during_a_key_change_is_skipped() {
    let (v, fs, hook) = hooked(&journal(), "Journal/Day 1.md.qenc");
    lock(&v);
    *hook.before.lock().unwrap() = Some(Box::new(|fs: &MemFs| {
        fs.rename(&np("Journal/Trips/Lisbon.md.qenc"), &np("Journal/Trips/Porto.md.qenc")).unwrap();
    }));
    // Listing first, then the move, then the reads.
    let r = change_key(&v, "Journal", "a new passphrase", 9).unwrap();
    assert!(!v.key_file("Journal").unwrap().1.rotating || r.files < 3);
    // Running it again finishes whatever was skipped.
    change_key(&v, "Journal", "a new passphrase", 10).unwrap();
    let fresh = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    fresh.unlock("Journal", "a new passphrase").unwrap();
    fresh.keys().set_previous("Journal", None);
    assert!(fresh.read(&np("Journal/Trips/Porto.md")).unwrap().contains("Lisbon"));
}

/// Reliability scan, interrupted lock: plain files left next to identical
/// encrypted copies are not "kept both" as duplicates.
#[test]
fn keep_both_makes_no_copy_of_a_file_identical_to_its_encrypted_one() {
    let files = journal();
    let mut n = 0;
    let fs = loop {
        let fs = Arc::new(MemFs::with_files(files.iter().copied()));
        let crashing = Vault::new(Arc::new(Crash { inner: Arc::clone(&fs), left: AtomicUsize::new(n) }));
        let _ = lock_folder(&crashing, "Journal", Some(PASS), Kdf::TEST, 1);
        if fs.exists(&np("Journal/Day 1.md")) && fs.exists(&stored(&np("Journal/Day 1.md"))) {
            break fs;
        }
        n += 1;
        assert!(n < 200);
    };
    let v = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    v.unlock("Journal", PASS).unwrap();
    assert!(!plain_left(&v)[0].conflicts.is_empty());
    let kept = keep_both(&v, "Journal").unwrap();
    assert!(kept.is_empty(), "{kept:?}");
    assert!(!fs.exists(&np("Journal/Day 1.md")));
    assert!(!fs.list().unwrap().iter().any(|p| p.as_str().contains("plain copy")));
    assert!(v.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET));
}

// ------------------------------------------------------------ 0.13.2 review (D-M4)

/// Review D-M4, scenario 1: a file written with the key from before the
/// last key change (by a device that still had it) after that change
/// ended is brought to the current key before a new key change drops the
/// old key; afterwards every file opens with the newest key file alone.
#[test]
fn a_new_key_change_first_finishes_files_left_under_the_previous_key() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    let k1_file = fs.read(&np("Journal/quire-lock.json")).unwrap();
    let (k1, id) = (KeyFile::parse(&k1_file).unwrap().unwrap_key(PASS).unwrap(), KeyFile::parse(&k1_file).unwrap().id().unwrap());
    change_key(&v, "Journal", "second passphrase", 9).unwrap();
    // A lagging device saves with K1 after the change ended (rotating is clear).
    assert!(!v.key_file("Journal").unwrap().1.rotating);
    fs.write_bytes_atomic(&np("Journal/Late.md.qenc"), &k1.seal(Kind::File, id, b"late, under K1").unwrap()).unwrap();
    let w = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    w.unlock("Journal", "second passphrase").unwrap();
    change_key(&w, "Journal", "third passphrase", 10).unwrap();
    // Only the newest key file: its key and its one previous key open everything.
    let fresh = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    fresh.unlock("Journal", "third passphrase").unwrap();
    assert_eq!(fresh.read(&np("Journal/Late.md")).unwrap(), "late, under K1");
    fresh.keys().set_previous("Journal", None);
    assert_eq!(fresh.read(&np("Journal/Late.md")).unwrap(), "late, under K1", "under the newest key itself");
    for (p, text) in journal().iter().filter(|(p, _)| p.starts_with("Journal/")) {
        assert_eq!(fresh.read_file(&np(p)).unwrap(), text.as_bytes(), "{p}");
    }
}

/// A storage whose reads of one file fail after the first, and whose
/// conditional writes to it never land (as a file held open on Windows).
struct HeldFile {
    inner: Arc<MemFs>,
    path: NotePath,
    reads: AtomicUsize,
}

impl Storage for HeldFile {
    fn list(&self) -> Result<Vec<NotePath>> {
        self.inner.list()
    }
    fn read_bytes(&self, path: &NotePath) -> Result<Vec<u8>> {
        if *path == self.path && self.reads.fetch_add(1, Ordering::SeqCst) > 0 {
            return Err(VaultError::Io { path: path.to_string(), source: std::io::Error::other("held by another program") });
        }
        self.inner.read_bytes(path)
    }
    fn write_bytes_atomic(&self, path: &NotePath, bytes: &[u8]) -> Result<()> {
        self.inner.write_bytes_atomic(path, bytes)
    }
    fn write_bytes_if(&self, path: &NotePath, still: &dyn Fn(Option<&[u8]>) -> bool, bytes: &[u8]) -> Result<bool> {
        if *path == self.path {
            return Ok(false);
        }
        self.inner.write_bytes_if(path, still, bytes)
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
    fn absolute(&self, _: &NotePath) -> Option<PathBuf> {
        None
    }
    fn list_under(&self, folder: &str) -> Result<Vec<NotePath>> {
        self.inner.list_under(folder)
    }
    fn remove(&self, path: &NotePath) -> Result<()> {
        self.inner.remove(path)
    }
    fn set_locked_folders(&self, folders: &[String]) {
        self.inner.set_locked_folders(folders);
    }
}

/// Review D-M4, scenario 2: a file that can't be read when the key change
/// checks what is left is not counted as done; the change stays unfinished
/// (the previous key kept) and finishes once the file can be read.
#[test]
fn a_file_that_cant_be_read_keeps_the_key_change_unfinished() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    let held = Vault::new(Arc::new(HeldFile { inner: Arc::clone(&fs), path: np("Journal/Day 1.md.qenc"), reads: AtomicUsize::new(0) }));
    held.unlock("Journal", PASS).unwrap();
    begin_change_key(&held, "Journal", "a new passphrase", 9).unwrap();
    assert!(finish_change_key(&held, "Journal").is_err());
    assert!(v.key_file("Journal").unwrap().1.rotating, "not marked done");
    let again = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    again.unlock("Journal", "a new passphrase").unwrap();
    assert!(again.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET), "through the kept previous key");
    finish_change_key(&again, "Journal").unwrap();
    again.keys().set_previous("Journal", None);
    assert!(again.read(&np("Journal/Day 1.md")).unwrap().contains(SECRET));
}

/// Review D-M4, scenario 3: another device's key change that arrives while
/// this one runs is never stamped done by it.
#[test]
fn a_key_file_replaced_during_a_key_change_is_not_marked_done() {
    let (v, fs, hook) = hooked(&journal(), "Journal/Day 1.md.qenc");
    lock(&v);
    let k1 = v.keys().get("Journal").unwrap().1;
    begin_change_key(&v, "Journal", "a new passphrase", 9).unwrap();
    // Device B's key change (K1 → K3) reaches this vault mid-way.
    let kf = v.key_file("Journal").unwrap().1;
    let k3 = FolderKey::generate().unwrap();
    let mut theirs = KeyFile::new(kf.id().unwrap(), &k3, "device B passphrase", Kdf::TEST, 11).unwrap();
    theirs.set_previous(&k3, &k1).unwrap();
    theirs.rotating = true;
    let theirs = theirs.to_json();
    let written = theirs.clone();
    *hook.before.lock().unwrap() = Some(Box::new(move |fs: &MemFs| fs.write_atomic(&np("Journal/quire-lock.json"), &written).unwrap()));
    assert!(finish_change_key(&v, "Journal").is_err());
    assert_eq!(fs.read(&np("Journal/quire-lock.json")).unwrap(), theirs, "B's key file is left as B wrote it");
}

// ------------------------------------------------------------ 0.13.2 final review

/// Final review F-L1: a process still holding the folder's key from before
/// two key changes made elsewhere can't seal a new note with it (no key
/// file would hold that key); unlocked again, it writes under the current
/// key, and the note opens with the current passphrase.
#[test]
fn a_key_changed_elsewhere_is_not_used_to_write() {
    let (cli, fs) = vault_with(&journal());
    lock(&cli);
    let app = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    app.unlock("Journal", PASS).unwrap();
    app.write(&np("Journal/Before.md"), "written with the first key").unwrap();
    change_key(&cli, "Journal", "second passphrase", 9).unwrap();
    // Even right after one change, the old key no longer writes.
    assert!(matches!(app.write(&np("Journal/New.md"), "x"), Err(VaultError::Locked(_))));
    change_key(&cli, "Journal", "third passphrase", 10).unwrap();
    let new = np("Journal/New.md");
    assert!(matches!(app.write(&new, "a note written with a stale key"), Err(VaultError::Locked(_))));
    assert!(app.snapshot(&new, "a version", 11, true).is_err(), "history isn't sealed with it either");
    assert!(!fs.exists(&stored(&new)));
    // Unlocked again: written under the current key.
    app.unlock("Journal", "third passphrase").unwrap();
    app.write(&new, "a note written after unlocking again").unwrap();
    let fresh = Vault::new(Arc::clone(&fs) as Arc<dyn Storage>);
    fresh.unlock("Journal", "third passphrase").unwrap();
    fresh.keys().set_previous("Journal", None);
    assert_eq!(fresh.read(&new).unwrap(), "a note written after unlocking again");
    assert_eq!(fresh.read(&np("Journal/Before.md")).unwrap(), "written with the first key");
}

/// Final review F-I4: a damaged file blocks a new key change with a
/// message that names it, says the key wasn't changed, and says what to
/// do; once it is moved out, the key change goes through.
#[test]
fn a_damaged_file_blocking_a_key_change_is_named() {
    let (v, fs) = vault_with(&journal());
    lock(&v);
    change_key(&v, "Journal", "second passphrase", 9).unwrap();
    let mut bad = fs.read_bytes(&np("Journal/Day 1.md.qenc")).unwrap();
    *bad.last_mut().unwrap() ^= 1;
    fs.write_bytes_atomic(&np("Journal/Bad.md.qenc"), &bad).unwrap();
    let before = fs.read(&np("Journal/quire-lock.json")).unwrap();
    let e = begin_change_key(&v, "Journal", "third passphrase", 10).unwrap_err().to_string();
    assert!(e.contains("Journal/Bad.md.qenc") && e.contains("were not changed") && e.contains("move it out"), "{e}");
    assert_eq!(fs.read(&np("Journal/quire-lock.json")).unwrap(), before, "the key file is as it was");
    fs.remove(&np("Journal/Bad.md.qenc")).unwrap();
    change_key(&v, "Journal", "third passphrase", 11).unwrap();
}
