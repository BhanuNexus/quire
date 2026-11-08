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
    fs.write_atomic(&np("Journal/quire-lock.unlocking.json"), &fs.read(&np("Journal/quire-lock.json")).unwrap()).ok();
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
    assert_eq!(v.history_paths_under("Journal"), [old.clone(), np("Journal/Trips/Porto.md")]);
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
