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
    // The locked-folder pass runs in the background; it ends in a tick.
    let start = Instant::now();
    while ws.locked_pass_pending() {
        assert!(start.elapsed() < Duration::from_secs(20), "the locked-folder pass never ended");
        ws.tick(Instant::now());
        std::thread::sleep(Duration::from_millis(5));
    }
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
    assert_eq!(closed, Some(1));
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

// ------------------------------------------------------------ review 0.11

fn notices(ws: &Workspace) -> String {
    ws.notices(Duration::from_mins(10)).map(|n| n.text.clone()).collect::<Vec<_>>().join("\n")
}

/// Security review S-H1: a deleted key file never makes a save plain, and
/// the person is told.
#[test]
fn a_deleted_key_file_keeps_saves_encrypted_and_is_reported() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nMARKER-9d1f\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    let id = ws.open_note(&np("Journal/Day.md")).unwrap();
    std::fs::remove_file(root.join("Journal/quire-lock.json")).unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# Day\nMARKER-9d1f typed after the key file went\n");
    assert!(ws.save(id));
    ws.history.flush();
    assert!(!root.join("Journal/Day.md").exists(), "never plain");
    assert!(leaks(&root, MARK).is_empty());
    // Another window on this device: still locked, and says why.
    let mut other = open(&root);
    assert!(notices(&other).contains("key file"), "{}", notices(&other));
    assert!(other.locked_folders().iter().any(|f| f.key_missing));
    assert!(other.vault().unwrap().write(&np("Journal/New.md"), "MARKER-9d1f").is_err());
    let e = other.unlock_folder("Journal", PASS, false).unwrap_err();
    assert!(e.contains("key file"), "{e}");
    other.shutdown();
}

/// Data review H1: a wrong recovery key, with the key file missing, writes
/// nothing; the right one rebuilds the key file.
#[test]
fn a_wrong_recovery_key_writes_no_key_file() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n")]);
    let mut ws = open(&root);
    let rk = lock_and_wait(&mut ws, "Journal");
    ws.lock_folder_now("Journal");
    std::fs::remove_file(root.join("Journal/quire-lock.json")).unwrap();
    let wrong = quire_vault::lock::FolderKey::generate().unwrap().recovery_key();
    let e = ws.unlock_with_recovery_key("Journal", &wrong, Some("new passphrase")).unwrap_err();
    assert_eq!(e, "That key doesn't open this folder.");
    assert!(!root.join("Journal/quire-lock.json").exists());
    ws.unlock_with_recovery_key("Journal", &rk, Some("new passphrase")).unwrap();
    assert!(root.join("Journal/quire-lock.json").exists());
    ws.lock_folder_now("Journal");
    ws.unlock_folder("Journal", "new passphrase", false).unwrap();
}

/// Data review H2: the recovery key can be shown again, and a conversion
/// that stops after the key file was written still hands it over.
#[test]
fn the_recovery_key_is_never_lost() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n"), ("Box/A.md", "a")]);
    let mut ws = open(&root);
    let rk = lock_and_wait(&mut ws, "Journal");
    assert_eq!(ws.folder_recovery_key("Journal", "wrong horse").unwrap_err(), "Wrong passphrase.");
    assert_eq!(ws.folder_recovery_key("Journal", PASS).unwrap(), rk);
    // A file the conversion can't handle (its encrypted name is taken by a folder): it stops.
    std::fs::create_dir_all(root.join("Box/A.md.qenc/x")).unwrap();
    ws.start_locking("Box", PASS, false).unwrap();
    until(&mut [&mut ws], "the conversion", |w| w[0].lock_job().is_none());
    let text = notices(&ws);
    assert!(text.contains("Converting Box/ stopped") && !text.contains("Nothing was lost"), "{text}");
    let (f, shown) = ws.take_recovery_key().expect("shown although the conversion stopped");
    assert_eq!(f, "Box");
    assert_eq!(ws.folder_recovery_key("Box", PASS).unwrap(), shown);
}

/// Data review H4: edits that can't be saved are never lost to a lock or a
/// quit, and never written plain.
#[test]
fn unsaved_edits_in_a_locked_folder_survive_locking_and_quitting() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    let id = ws.open_note(&np("Journal/Day.md")).unwrap();
    // Saving fails from now on (its encrypted file's name is taken by a folder).
    std::fs::remove_file(root.join("Journal/Day.md.qenc")).unwrap();
    std::fs::create_dir_all(root.join("Journal/Day.md.qenc/x")).unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# Day\nMARKER-9d1f unsaved words\n");
    assert!(!ws.save(id));
    // Idle locking: not while the note can't be saved.
    ws.settings.security.lock_after_idle_minutes = 1;
    ws.note_user_activity();
    ws.tick(Instant::now() + Duration::from_secs(61));
    assert!(ws.is_unlocked("Journal"), "not locked over unsaved edits");
    assert!(notices(&ws).contains("wasn't locked after the idle time"));
    // Locking now: the edits are kept, encrypted, beside the note.
    assert_eq!(ws.lock_folder_now("Journal"), Some(1));
    assert!(!ws.is_unlocked("Journal"));
    ws.unlock_folder("Journal", PASS, false).unwrap();
    let v = ws.vault().unwrap().clone();
    let copy = v.locked_notes("Journal").into_iter().find(|p| p.stem().starts_with("Day (unsaved")).expect("a copy");
    assert!(v.read(&copy).unwrap().contains("unsaved words"));
    // Quitting: the same, for an unsaveable note still open.
    let id = ws.open_note(&copy).unwrap();
    std::fs::rename(root.join(format!("{copy}.qenc")), root.join("moved.bin")).unwrap();
    std::fs::create_dir_all(root.join(format!("{copy}.qenc/x"))).unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# Day\nMARKER-9d1f typed before quitting\n");
    assert!(!ws.save(id));
    ws.shutdown();
    let kept: Vec<String> = v.locked_notes("Journal").into_iter().filter_map(|p| v.read(&p).ok()).collect();
    assert!(kept.iter().any(|t| t.contains("typed before quitting")), "{kept:?}");
    assert!(leaks(&root, MARK).is_empty());
}

/// Data review H4: a note whose folder was locked elsewhere (no key here)
/// is listed, so the app asks before quitting.
#[test]
fn notes_that_cant_be_saved_are_listed_before_quitting() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    ws.open_note(&np("Journal/Day.md")).unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# Day\nnot saved\n");
    // Locked behind the window's back (another device, the CLI): no key here.
    ws.vault().unwrap().lock_folder_now("Journal");
    assert_eq!(ws.unsaveable_locked_notes(), ["Day"]);
    assert!(!ws.quit_confirmed());
    ws.confirm_quit();
    assert!(ws.quit_confirmed() && ws.take_quit_request() && !ws.take_quit_request());
}

/// Security review S-M4: a new note in a locked folder keeps a neutral name.
#[test]
fn notes_in_a_locked_folder_are_not_named_from_their_text() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    ws.new_note_in("Journal").unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# I think I'm leaving\n\nMARKER-9d1f\n");
    ws.save_all();
    let names: Vec<String> =
        std::fs::read_dir(root.join("Journal")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().any(|n| n == "Untitled.md.qenc"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("leaving")), "{names:?}");
}

/// Security review S-M6: moving a plain note into a locked folder, or a
/// folder locked on another device, clears what this device derived from
/// the plain text.
#[test]
fn derived_data_goes_when_a_note_moves_in_or_a_folder_locks_elsewhere() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nx\n"), ("Plain.md", "# Plain\nMARKER-9d1f\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    let cache = root.join(".quire/cache/claims/x");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("c.json"), "MARKER-9d1f").unwrap();
    ws.open_note(&np("Plain.md")).unwrap();
    ws.move_active("Journal").unwrap();
    assert!(!cache.exists(), "content-keyed caches are cleared");
    // Device B: a plain Journal/ with caches, then A's key file arrives.
    let (_e, b_root) = vault_dir(&[("Journal/Day.md", "# Day\nMARKER-9d1f\n")]);
    let mut b = open(&b_root);
    let b_cache = b_root.join(".quire/cache/claims/y");
    std::fs::create_dir_all(&b_cache).unwrap();
    std::fs::copy(root.join("Journal/quire-lock.json"), b_root.join("Journal/quire-lock.json")).unwrap();
    super::vault_event(&mut b, &quire_vault::VaultEvent::AttachmentChanged(np("Journal/quire-lock.json")));
    assert!(!b_cache.exists());
    assert!(b.vault().unwrap().known_locked().contains("Journal"));
}

/// Security review S-M10: a vault with AI off never gets an embedder, not
/// even before the first tick.
#[test]
fn ai_off_applies_before_any_embedder() {
    let (_d, root) = vault_dir(&[("Plan.md", "# Plan\n"), (".quire/shared/agent-policy.toml", "format = 1\nai = \"off\"\n")]);
    let mut ws = Workspace::new(crate::Settings { embeddings: "endpoint".into(), ..crate::Settings::default() });
    ws.open_vault_deferred(&root).unwrap();
    assert!(!ws.index().has_semantic_builder());
    assert!(ws.chat.is_none());
}

/// Security review S-M8: changing the key from the app gives a new
/// recovery key; the old passphrase and recovery key stop working.
#[test]
fn changing_a_folders_key_from_the_app() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\nMARKER-9d1f\n")]);
    let mut ws = open(&root);
    let old_rk = lock_and_wait(&mut ws, "Journal");
    ws.lock_folder_now("Journal");
    assert_eq!(ws.start_changing_key("Journal", "wrong horse", "a new passphrase").unwrap_err(), "Wrong passphrase.");
    ws.start_changing_key("Journal", PASS, "a new passphrase").unwrap();
    until(&mut [&mut ws], "the key change", |w| w[0].lock_job().is_none());
    let (f, new_rk) = ws.take_recovery_key().expect("a new recovery key is offered");
    assert_eq!(f, "Journal");
    assert_ne!(new_rk, old_rk);
    ws.lock_folder_now("Journal");
    assert_eq!(ws.unlock_folder("Journal", PASS, false).unwrap_err(), "Wrong passphrase.");
    assert!(ws.unlock_with_recovery_key("Journal", &old_rk, None).is_err());
    ws.unlock_with_recovery_key("Journal", &new_rk, None).unwrap();
    assert!(ws.vault().unwrap().read(&np("Journal/Day.md")).unwrap().contains(MARK));
    assert!(leaks(&root, MARK).is_empty());
}

/// 0.12: while a folder is unlocked its notes are in tasks, the agenda,
/// views and Bases, the graph, backlinks and reminders, from memory only;
/// agents, jobs and every cache still never see them, and nothing of them
/// reaches the disk in plain text. Locking takes them out again.
#[test]
fn unlocked_notes_join_the_features_from_memory_only() {
    let soon = (chrono::Local::now() + chrono::Duration::hours(3)).format("%Y-%m-%d %H:%M").to_string();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let day = format!(
        "---\nstatus: doing\nlocation: [38.7, -9.1]\n---\n# Day\nMARKER-9d1f see [[Plain]].\n- [ ] MARKER-9d1f call Bo 📅 {today}\n- [ ] MARKER-9d1f ship ⏰ {soon}\n"
    );
    let (dir, root) = vault_dir(&[
        ("Journal/Day.md", &day),
        ("Journal/Task.md", "---\ntype: task\nstatus: open\n---\n# MARKER-9d1f task note\n"),
        ("Plain.md", "# Plain\nLinks to [[Day]].\n- [ ] plain task\n"),
        ("Board.md", "# Board\n```quire-view\nfrom: all\nwhere: status = doing\n```\n"),
        ("Bills.base", "views:\n  - type: table\n    name: All\n    order: [file.name, status]\n"),
    ]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    ws.index().flush();
    until(&mut [&mut ws], "the in-memory index", |w| w[0].has_locked_overlay());
    let day_p = np("Journal/Day.md");
    // Tasks and the agenda.
    let tasks = ws.with_notes(|n| n.tasks(false));
    assert!(tasks.iter().any(|(p, t)| *p == day_p && t.text.contains("call Bo")), "{tasks:?}");
    let agenda = ws.agenda();
    assert!(agenda.today.iter().chain(&agenda.overdue).any(|i| i.path == day_p.as_str()), "{agenda:?}");
    assert!(ws.task_notes().iter().any(|(p, _)| p.as_str() == "Journal/Task.md"));
    // A view and a Base.
    let view = ws.run_view("from: all\nwhere: status = doing", None);
    assert!(view.as_ref().as_ref().unwrap().rows.iter().any(|r| r.path == day_p));
    let base = quire_import::obsidian::base::parse(&std::fs::read_to_string(root.join("Bills.base")).unwrap()).unwrap();
    let table = crate::bases::table(&ws, &base, 0, chrono::Local::now().naive_local());
    assert!(table.rows.iter().any(|r| r.path == day_p), "{:?}", table.rows.iter().map(|r| &r.path).collect::<Vec<_>>());
    let pins = crate::geo::pins(&ws, &view.as_ref().as_ref().unwrap().rows);
    assert_eq!(pins.len(), 1);
    // The graph and backlinks, both ways.
    let g = ws.link_graph(&crate::graph::Scope::All { orphans: false });
    let ix = |p: &str| g.nodes.iter().position(|n| n.path.as_str() == p).unwrap();
    let (a, b) = (ix("Plain.md"), ix("Journal/Day.md"));
    assert!(g.edges.contains(&(a.min(b), a.max(b))), "{g:?}");
    ws.open_note(&np("Plain.md")).unwrap();
    assert!(ws.insights().backlinks.iter().any(|b| b.from == day_p));
    assert!(ws.overlay_backlinks(&day_p).iter().any(|b| b.from.as_str() == "Plain.md"));
    // Reminders: scheduled from memory.
    crate::reminders::tick(&mut ws);
    assert!(ws.features.tasks.reminders.schedule.iter().any(|r| r.path == day_p.as_str()));
    // Agents, MCP, jobs and caches read the vault index and the vault
    // without keys: nothing of the folder is there.
    let main = ws.index().read();
    assert!(main.paths().iter().all(|p| !p.as_str().starts_with("Journal/")));
    assert!(main.search("MARKER", 5).is_empty());
    assert!(main.backlinks(&np("Plain.md")).is_empty());
    drop(main);
    assert!(ws.vault().unwrap().without_keys().read(&day_p).is_err());
    let store = ws.agent_store().unwrap().clone();
    for client in ["mcp:claude-code", "panel:anthropic", "acp:zed"] {
        let a = quire_agent::Actor::agent(client, client, quire_agent::Via::Panel, "d");
        assert_eq!(store.effective_mode(&a, &day_p, quire_agent::OpKind::Read), quire_agent::Mode::Deny, "{client}");
    }
    // The task timer on a locked task: its file outside the vault holds no text.
    let timer = dir.path().join("config").join("task-timer.json");
    ws.features.tasks.timer = crate::task_timer::TimerState::with_file(timer.clone());
    let offset = day.find("- [ ] MARKER-9d1f call").unwrap();
    let task = ws.task_ref(&day_p, Some(offset), "call Bo");
    assert!(ws.start_timer(task, false));
    assert!(timer.exists());
    assert!(leaks(&dir.path().join("config"), MARK).is_empty(), "{}", std::fs::read_to_string(&timer).unwrap());
    // Lint (a job: the vault without keys) doesn't call a link to a locked note broken.
    let lint_notes = [quire_agent::lint::LintNote::from_source(np("Plain.md"), "# Plain\nLinks to [[Day]].\n".into())];
    let found = crate::agent_state::lint::check_vault(&ws.vault().unwrap().without_keys(), &store, &lint_notes);
    assert!(found.iter().all(|f| f.check != quire_agent::inbox::LintCheck::BrokenLink), "{found:?}");
    // Nothing plain on disk, with every feature having read the folder.
    ws.save_all();
    ws.history.flush();
    let found = leaks(&root, MARK);
    assert!(found.is_empty(), "plain text on disk: {found:#?}");
    // Locking takes the notes out of every feature, and out of memory.
    ws.lock_folder_now("Journal");
    assert!(!ws.has_locked_overlay());
    assert!(ws.with_notes(|n| n.tasks(false)).iter().all(|(p, _)| *p != day_p));
    assert!(ws.agenda().today.iter().chain(&ws.agenda().overdue).all(|i| i.path != day_p.as_str()));
    assert!(ws.run_view("from: all\nwhere: status = doing", None).as_ref().as_ref().unwrap().rows.is_empty());
    assert!(ws.link_graph(&crate::graph::Scope::All { orphans: true }).nodes.iter().all(|n| n.path != day_p));
    assert!(ws.overlay_backlinks(&np("Plain.md")).is_empty());
    crate::reminders::tick(&mut ws);
    assert!(ws.features.tasks.reminders.schedule.iter().all(|r| r.path != day_p.as_str()));
}

/// 0.12: a reminder of a note in an unlocked folder fires from memory: it
/// never goes into the fired list on disk, and a snoozed one doesn't fire
/// once its folder is locked.
#[test]
fn reminders_of_locked_notes_fire_only_while_unlocked() {
    let soon = (chrono::Local::now() + chrono::Duration::hours(3)).format("%Y-%m-%d %H:%M").to_string();
    let (_d, root) = vault_dir(&[("Journal/Day.md", &format!("# Day\n- [ ] MARKER-9d1f ship ⏰ {soon}\n")), ("Plain.md", "# Plain\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    until(&mut [&mut ws], "the in-memory index", |w| w[0].has_locked_overlay());
    crate::reminders::tick(&mut ws);
    // Pretend time passed: it is due.
    let st = &mut ws.features.tasks.reminders;
    st.schedule[0].at = chrono::Local::now().naive_local() - chrono::Duration::minutes(1);
    st.checked_for_test(chrono::Local::now().naive_local() - chrono::Duration::minutes(2));
    let r = st.schedule[0].clone();
    assert!(crate::reminders::tick(&mut ws));
    assert_eq!(ws.features.tasks.reminders.fired.len(), 1);
    assert!(notices(&ws).contains("ship"));
    // Snoozed, then locked: it never fires, and its text leaves memory.
    ws.snooze_reminder(&r);
    ws.lock_folder_now("Journal");
    crate::reminders::tick(&mut ws);
    assert!(ws.features.tasks.reminders.fired.is_empty());
    assert!(ws.features.tasks.reminders.active().is_empty());
    assert!(leaks(&root, MARK).is_empty());
}

/// 0.12 review (locked-overlay low): typed links, the hierarchy and the trail read the same
/// joined notes as backlinks do, in memory only, and let go when the folder locks.
#[test]
fn typed_links_and_hierarchy_cross_the_overlay() {
    let (_d, root) = vault_dir(&[
        ("Plain.md", "---\nup: \"[[Day]]\"\n---\n# Plain\n"),
        ("Journal/Day.md", "# Day\nsupports:: [[Plain]]\n"),
        ("Journal/Kid.md", "---\nup: \"[[Plain]]\"\n---\n# Kid\n"),
    ]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    until(&mut [&mut ws], "the in-memory index", |w| w[0].has_locked_overlay());
    ws.open_note(&np("Plain.md")).unwrap();
    let rel = ws.note_relations();
    let items = |g: &str| -> Vec<String> {
        rel.groups.iter().filter(|x| x.label == g).flat_map(|x| x.items.iter().map(|i| i.path.as_str().to_string())).collect()
    };
    assert_eq!(items("Parents"), ["Journal/Day.md"], "{:?}", rel.groups);
    assert_eq!(items("Children"), ["Journal/Kid.md"], "{:?}", rel.groups);
    assert_eq!(rel.trails, [vec![(np("Journal/Day.md"), "Day".to_string()), (np("Plain.md"), "Plain".to_string())]]);
    // The graph names the typed edge between the two sides.
    let g = ws.link_graph(&crate::graph::Scope::All { orphans: true });
    assert!(g.types.iter().any(|t| t == "supports"), "{:?}", g.types);
    // Locked: the links to and from the folder go with it.
    ws.lock_folder_now("Journal");
    let rel = ws.note_relations();
    assert!(rel.groups.iter().all(|g| g.items.iter().all(|i| !i.path.as_str().starts_with("Journal/"))), "{:?}", rel.groups);
    assert!(rel.trails.is_empty());
}

/// 0.12: pictures in a locked folder show from memory while it is
/// unlocked; a picture pasted or dropped into a locked note is stored
/// encrypted in the folder's own attachments subfolder, never in the plain
/// attachments folder; export stays refused; nothing plain reaches the disk.
#[test]
fn pictures_in_locked_folders() {
    let png_with_marker = |extra: &str| {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(extra.as_bytes());
        b
    };
    let (dir, root) = vault_dir(&[("Journal/Day.md", "# Day\n![[photo.png]]\n"), ("Plain.md", "# Plain\n")]);
    std::fs::write(root.join("Journal/photo.png"), png_with_marker("MARKER-9d1f photo")).unwrap();
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    assert!(root.join("Journal/photo.png.qenc").exists() && !root.join("Journal/photo.png").exists());
    // Resolved by its plain name, read only from memory.
    let src = ws.attachment_source("photo.png").unwrap();
    assert_eq!(src, crate::attach::AttachmentSource::Locked(np("Journal/photo.png")));
    assert!(ws.attachment_path("photo.png").is_none(), "never a path to read as it is");
    assert_eq!(ws.read_locked_attachment(&np("Journal/photo.png")).unwrap(), png_with_marker("MARKER-9d1f photo"));
    // Pasting into the open locked note.
    ws.open_note(&np("Journal/Day.md")).unwrap();
    let pasted = ws.paste_image(1, 1, &[1, 2, 3, 255]).unwrap();
    assert!(pasted.as_str().starts_with("Journal/attachments/Pasted image "), "{pasted}");
    assert!(root.join(format!("{pasted}.qenc")).exists() && !root.join(pasted.as_str()).exists());
    assert!(!root.join("attachments").exists(), "nothing in the plain attachments folder");
    assert!(ws.active_doc().unwrap().text().contains(&format!("![[{}]]", pasted.file_name())));
    assert!(ws.read_locked_attachment(&pasted).unwrap().starts_with(b"\x89PNG"));
    // Dropping a file from outside the vault.
    let outside = dir.path().join("drop.png");
    std::fs::write(&outside, png_with_marker("MARKER-9d1f dropped")).unwrap();
    assert_eq!(ws.import_files(&[outside]), 1);
    assert!(root.join("Journal/attachments/drop.png.qenc").exists());
    assert_eq!(ws.attachment_source("drop.png"), Some(crate::attach::AttachmentSource::Locked(np("Journal/attachments/drop.png"))));
    // A plain note's export never finds the picture.
    assert!(super::refuse_export(&mut ws), "the locked note isn't exported");
    ws.save_all();
    ws.history.flush();
    let found = leaks(&root, MARK);
    assert!(found.is_empty(), "plain text on disk: {found:#?}");
    let plain_pngs: Vec<String> = walk(&root)
        .into_iter()
        .filter(|p| {
            !Path::new(p).extension().is_some_and(|e| e.eq_ignore_ascii_case("qenc"))
                && std::fs::read(p).is_ok_and(|b| b.starts_with(b"\x89PNG"))
        })
        .collect();
    assert!(plain_pngs.is_empty(), "{plain_pngs:?}");
    // Locked: nothing to show, nothing to add.
    ws.lock_folder_now("Journal");
    assert!(ws.read_locked_attachment(&np("Journal/photo.png")).is_none());
    ws.open_note(&np("Plain.md")).unwrap();
    assert!(ws.paste_image(1, 1, &[1, 2, 3, 255]).unwrap().as_str().starts_with("attachments/"), "a plain note's go where they did");
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                todo.push(e.path());
            } else {
                out.push(e.path().display().to_string());
            }
        }
    }
    out
}

#[test]
fn tidy_leaves_notes_in_locked_folders_alone() {
    let (_d, root) = vault_dir(&[("Journal/Day.md", "# Day\ntext   \n"), ("Plain.md", "# Plain\n")]);
    let mut ws = open(&root);
    lock_and_wait(&mut ws, "Journal");
    let id = ws.open_note(&np("Journal/Day.md")).unwrap();
    ws.active_doc_mut().unwrap().replace_minimally("# Day\ntext   \nmore   \n");
    ws.lint_active_note();
    assert_eq!(&*ws.active_doc().unwrap().text(), "# Day\ntext   \nmore   \n");
    // Nor on save, with "tidy on save" on.
    let mut lint = ws.switch_prefs().lint.clone();
    lint.on_save = true;
    assert!(ws.set_lint_options(lint));
    assert!(ws.save(id));
    assert_eq!(&*ws.vault().unwrap().read(&np("Journal/Day.md")).unwrap(), "# Day\ntext   \nmore   \n");
}
