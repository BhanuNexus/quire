// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quire_vault::NotePath;

use crate::vault_sync::{SyncStatus, SyncTarget};
use crate::workspace::{UiRequest, Workspace};

const PASS: &str = "correct horse";
const MARK: &str = "MARKER-9d1f";

fn np(s: &str) -> NotePath {
    NotePath::new(s)
}

fn vault_dir(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Vault");
    for (p, t) in files {
        let abs = root.join(p);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, t).unwrap();
    }
    (dir, root)
}

fn open(root: &Path) -> Workspace {
    let mut ws = Workspace::new(crate::Settings::default());
    ws.open_vault(root).unwrap();
    ws.index().flush();
    ws
}

fn until(ws: &mut [&mut Workspace], what: &str, mut done: impl FnMut(&mut [&mut Workspace]) -> bool) {
    let start = Instant::now();
    while !done(ws) {
        assert!(start.elapsed() < Duration::from_secs(40), "timed out waiting for {what}");
        for w in ws.iter_mut() {
            w.tick(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn lock_and_wait(ws: &mut Workspace, folder: &str) -> String {
    ws.start_locking(folder, PASS, false).unwrap();
    until(&mut [&mut *ws], "the conversion", |w| w[0].lock_job().is_none());
    let (f, rk) = ws.take_recovery_key().expect("the recovery key is offered once");
    assert_eq!(f, folder);
    assert!(ws.take_recovery_key().is_none());
    rk
}

/// Files under `root` (`.quire/` and the trash too) holding `marker`, also
/// inside gzip files.
fn leaks(root: &Path, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
                continue;
            }
            let Ok(b) = std::fs::read(&p) else { continue };
            let mut text = b.clone();
            if b.starts_with(&[0x1f, 0x8b]) {
                let mut out = Vec::new();
                if flate2::read::GzDecoder::new(b.as_slice()).read_to_end(&mut out).is_ok() {
                    text = out;
                }
            }
            if text.windows(marker.len()).any(|w| w == marker.as_bytes()) {
                out.push(p.display().to_string());
            }
        }
    }
    out
}

#[test]
fn locking_a_folder_in_the_app_and_using_it() {
    let (_d, root) = vault_dir(&[
        ("Journal/Day.md", "# Day\nMARKER-9d1f first\n"),
        ("Journal/Old.md", "# Old\nMARKER-9d1f old\n"),
        ("Plain.md", "# Plain\nLinks to [[Day]].\n"),
    ]);
    let mut ws = open(&root);
    // History of the note before locking (plain, as any note's).
    ws.open_note(&np("Journal/Day.md")).unwrap();
    ws.history.flush();
    assert_eq!(ws.index().read().paths().len(), 3);
    let rk = lock_and_wait(&mut ws, "Journal");
    assert_eq!(rk.len(), 69);
    ws.index().flush();
    // Out of the vault index; in the in-memory one while unlocked.
    assert_eq!(ws.index().read().paths(), [np("Plain.md")]);
    until(&mut [&mut ws], "the locked index", |w| !w[0].locked_search("first", 5).is_empty());
    assert!(ws.index().read().search("MARKER", 5).is_empty());
    assert_eq!(ws.locked_quick_open("old", 5)[0].path, np("Journal/Old.md"));
    // Editing and saving an open locked note: encrypted on disk, history too.
    let id = ws.open_note(&np("Journal/Day.md")).unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# Day\nMARKER-9d1f second edit\n");
    assert!(ws.save(id));
    ws.history.flush();
    assert!(root.join("Journal/Day.md.qenc").exists() && !root.join("Journal/Day.md").exists());
    until(&mut [&mut ws], "the search to follow the edit", |w| !w[0].locked_search("second", 5).is_empty());
    assert!(ws.vault().unwrap().history(&np("Journal/Day.md")).iter().all(|s| quire_vault::lock::is_encrypted_name(s.file.as_str())));
    // A link from a plain note resolves to the locked note (never a new note).
    ws.open_note(&np("Plain.md")).unwrap();
    let opened = ws.open_link("Day", None);
    assert_eq!(opened, Some(id));
    // Lock now: the note closes, search forgets, opening shows the lock screen.
    let closed = ws.lock_folder_now("Journal");
    assert_eq!(closed, 1);
    assert!(ws.docs().iter().all(|d| d.path.as_ref() != Some(&np("Journal/Day.md"))));
    assert!(ws.locked_search("second", 5).is_empty());
    let _ = ws.take_ui_requests();
    assert!(ws.open_note(&np("Journal/Day.md")).is_none());
    assert!(ws.take_ui_requests().contains(&UiRequest::OpenFile(np("Journal/Day.md.qenc"))));
    ws.open_note(&np("Plain.md")).unwrap();
    assert!(ws.open_link("Day", None).is_none(), "a link to a locked note doesn't create one");
    assert!(!root.join("Day.md").exists());
    // Wrong passphrase, then the recovery key with a new passphrase.
    assert_eq!(ws.unlock_folder("Journal", "wrong horse", false).unwrap_err(), "Wrong passphrase.");
    ws.unlock_with_recovery_key("Journal", &rk, Some("new passphrase")).unwrap();
    ws.lock_folder_now("Journal");
    ws.unlock_folder("Journal", "new passphrase", false).unwrap();
    assert!(ws.vault().unwrap().read(&np("Journal/Day.md")).unwrap().contains("second edit"));
    ws.save_all();
    ws.history.flush();
    let found = leaks(&root, MARK);
    assert!(found.is_empty(), "plain text on disk: {found:#?}");
}

#[test]
fn idle_locking_and_the_status_bar() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    ws.settings.security.lock_after_idle_minutes = 1;
    ws.note_user_activity();
    assert!(ws.status_items().iter().any(|s| s.text.contains("Journal/")));
    let deadline = super::idle_deadline(&ws).unwrap();
    assert!(deadline > Instant::now() + Duration::from_secs(50));
    assert!(ws.next_deadline().is_some_and(|d| d <= deadline));
    ws.tick(Instant::now());
    assert!(ws.is_unlocked("Journal"));
    ws.tick(Instant::now() + Duration::from_secs(61));
    assert!(!ws.is_unlocked("Journal"));
    ws.settings.security.lock_after_idle_minutes = 0;
    ws.unlock_folder("Journal", PASS, false).unwrap();
    ws.tick(Instant::now() + Duration::from_hours(1));
    assert!(ws.is_unlocked("Journal"), "0 never locks");
}

#[test]
fn ai_off_stops_providers_and_agents() {
    let (_d, root) = vault_dir(&[("Plan.md", "# Plan\n")]);
    let mut ws = open(&root);
    ws.set_ai_off(true).unwrap();
    assert!(super::ai_off(&ws));
    assert!(ws.chat.is_none());
    ws.apply_ai_settings();
    assert!(ws.chat.is_none(), "settings don't turn it back on");
    let store = ws.agent_store().unwrap().clone();
    let a = quire_agent::Actor::agent("panel:anthropic", "Panel", quire_agent::Via::Panel, "d");
    assert_eq!(store.effective_mode(&a, &np("Plan.md"), quire_agent::OpKind::Read), quire_agent::Mode::Deny);
    ws.set_ai_off(false).unwrap();
    assert!(!super::ai_off(&ws));
}

#[test]
fn unlocking_for_good_from_the_app() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nback to plain\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    ws.lock_folder_now("Journal");
    assert!(ws.start_unlocking_for_good("Journal", Some("wrong horse")).is_err());
    ws.start_unlocking_for_good("Journal", Some(PASS)).unwrap();
    until(&mut [&mut ws], "decrypting", |w| w[0].lock_job().is_none());
    ws.index().flush();
    assert_eq!(std::fs::read_to_string(root.join("Journal/Day.md")).unwrap(), "# Day\nback to plain\n");
    assert!(ws.locked_folders().is_empty());
    assert!(ws.index().read().paths().contains(&np("Journal/Day.md")));
}

/// Two devices through a sync folder: the ciphertext and the key file
/// travel; the other device unlocks with the same passphrase; edits made
/// on both are both kept; nothing plain reaches the sync folder or either
/// vault.
#[test]
fn two_devices_through_a_sync_folder() {
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, shared) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("Dropbox"));
    for d in [&va, &vb, &shared] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(va.join("Journal")).unwrap();
    std::fs::write(va.join("Journal/Day.md"), "# Day\nMARKER-9d1f from A\n").unwrap();
    std::fs::write(va.join("Plain.md"), "# Plain\n").unwrap();
    let target = SyncTarget::Folder(shared.clone());
    let mut a = open(&va);
    lock_and_wait(&mut a, "Journal");
    a.sync_create(&target, "sync passphrase", "laptop").unwrap();
    let mut b = open(&vb);
    let found = Workspace::sync_list(&target).unwrap();
    b.sync_join(&target, &found[0].space, "sync passphrase", "phone").unwrap();
    until(&mut [&mut a, &mut b], "the locked folder to reach B", |_| {
        vb.join("Journal/Day.md.qenc").exists() && vb.join("Journal/quire-lock.json").exists() && vb.join("Plain.md").exists()
    });
    b.index().flush();
    assert!(b.index().read().paths().iter().all(|p| p.as_str() == "Plain.md"));
    assert_eq!(b.locked_folders().len(), 1);
    assert!(matches!(b.vault().unwrap().read(&np("Journal/Day.md")), Err(quire_vault::VaultError::Locked(_))));
    b.unlock_folder("Journal", PASS, false).unwrap();
    assert!(b.vault().unwrap().read(&np("Journal/Day.md")).unwrap().contains("from A"));
    // An edit on B reaches A, still encrypted.
    b.vault().unwrap().write(&np("Journal/Day.md"), "# Day\nMARKER-9d1f from A\nand B\n").unwrap();
    b.sync_nudge();
    until(&mut [&mut a, &mut b], "B's edit to reach A", |w| {
        w[0].vault().unwrap().read(&np("Journal/Day.md")).is_ok_and(|t| t.contains("and B"))
    });
    until(&mut [&mut a, &mut b], "both to settle", |w| w.iter().all(|x| matches!(x.sync.status, SyncStatus::Synced(_))));
    // Both edit offline at once: encrypted files can't merge, so both versions stay.
    a.stop_sync().unwrap();
    a.vault().unwrap().write(&np("Journal/Day.md"), "# Day\nA's line\n").unwrap();
    b.vault().unwrap().write(&np("Journal/Day.md"), "# Day\nB's line\n").unwrap();
    b.sync_nudge();
    until(&mut [&mut b], "B to push", |w| matches!(w[0].sync.status, SyncStatus::Synced(_)));
    let found = Workspace::sync_list(&target).unwrap();
    a.sync_join(&target, &found[0].space, "sync passphrase", "laptop").unwrap();
    until(&mut [&mut a, &mut b], "the race to settle", |w| {
        let v = w[0].vault().unwrap();
        let notes = v.locked_notes("Journal");
        let texts: Vec<String> = notes.iter().filter_map(|p| v.read(p).ok()).collect();
        texts.iter().any(|t| t.contains("A's line")) && texts.iter().any(|t| t.contains("B's line"))
    });
    // Each version is its own encrypted note now (`Day.md`, and `Day (2).md` or a conflict copy).
    let copies: Vec<NotePath> = a.vault().unwrap().locked_notes("Journal");
    assert_eq!(copies.len(), 2, "{copies:?}");
    for root in [&va, &vb, &shared] {
        let found = leaks(root, MARK);
        assert!(found.is_empty(), "plain text in {}: {found:#?}", root.display());
    }
}
