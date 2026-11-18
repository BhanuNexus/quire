//! Tests for the vault sync glue: the store adapter, passes, setup and the
//! worker. Moved out of `mod.rs` unchanged; split them by part later.

use super::*;

#[test]
fn vault_store_never_overwrites_a_newer_save() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_local(dir.path()).unwrap();
    vault.write(&NotePath::new("A.md"), "one").unwrap();
    let mut store = VaultStore::new(vault.clone());
    snapshot_reads(&mut store).unwrap();
    // The editor saves after the snapshot…
    vault.write(&NotePath::new("A.md"), "edited in the app").unwrap();
    // …so the engine's write is refused.
    assert!(store.write("A.md", "from sync").is_err());
    assert_eq!(vault.read(&NotePath::new("A.md")).unwrap(), "edited in the app");
    snapshot_reads(&mut store).unwrap();
    store.write("A.md", "from sync").unwrap();
    assert_eq!(store.list().unwrap(), vec!["A.md".to_string()]);
    // Deletes go to the trash.
    snapshot_reads(&mut store).unwrap();
    store.remove("A.md").unwrap();
    assert!(store.read("A.md").is_none());
}

fn wait_for(ws: &mut [&mut Workspace], what: &str, mut done: impl FnMut(&[&mut Workspace]) -> bool) {
    let start = Instant::now();
    while !done(ws) {
        if start.elapsed() > Duration::from_secs(30) {
            // Say what each side sees, so a failure on another OS can be diagnosed.
            let report: Vec<String> = ws
                .iter()
                .map(|w| {
                    let files = w.vault().and_then(|v| v.storage().list().ok()).unwrap_or_default();
                    format!("{:?} files={files:?}", w.sync.status)
                })
                .collect();
            panic!("timed out waiting for {what}: {report:#?}");
        }
        for w in ws.iter_mut() {
            w.tick(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn two_vaults_sync_through_a_shared_folder() {
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, shared) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("Dropbox"));
    for d in [&va, &vb, &shared] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Plan.md"), "# Plan\nShip it.\n![[logo.png]]\n").unwrap();
    std::fs::write(va.join("logo.png"), [0x89u8, b'P', b'N', b'G', 7, 7]).unwrap();
    let target = SyncTarget::Folder(shared.clone());

    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create(&target, "correct horse battery", "laptop").unwrap();
    assert_eq!(a.sync.config.as_ref().and_then(|c| c.folder.clone()), Some(shared.join("Quire Sync")));

    let mut b = Workspace::new(crate::Settings::default());
    b.open_vault(&vb).unwrap();
    let found = Workspace::sync_list(&target).unwrap();
    assert_eq!(found.len(), 1);
    assert!(b.sync_join(&target, &found[0].space, "wrong passphrase", "phone").is_err());
    b.sync_join(&target, &found[0].space, "correct horse battery", "phone").unwrap();
    wait_for(&mut [&mut a, &mut b], "the note and its image to reach B", |_| {
        std::fs::read_to_string(vb.join("Plan.md")).is_ok_and(|t| t.contains("Ship it."))
            && std::fs::read(vb.join("logo.png")).is_ok_and(|b| b == [0x89u8, b'P', b'N', b'G', 7, 7])
    });

    // An edit on B travels back to A; B's worker notices the save, A's notices the folder change.
    std::fs::write(vb.join("Plan.md"), "# Plan\nShip it.\nFrom the phone.\n").unwrap();
    b.sync_nudge();
    wait_for(&mut [&mut a, &mut b], "the edit to reach A", |_| {
        std::fs::read_to_string(va.join("Plan.md")).is_ok_and(|t| t.contains("From the phone."))
    });
    wait_for(&mut [&mut a, &mut b], "both to settle", |ws| ws.iter().all(|w| matches!(w.sync.status, SyncStatus::Synced(_))));
    // Every device's versions come back for the history dialog.
    let rx = a.fetch_sync_history(&NotePath::new("Plan.md")).unwrap();
    let versions = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
    assert!(versions.iter().any(|v| v.device == "laptop" && !v.text.contains("phone")), "{versions:?}");
    assert!(versions.iter().any(|v| v.device == "phone" && v.text.contains("From the phone.")), "{versions:?}");
    // The sync folder holds only encrypted logs.
    let space_dir = shared.join("Quire Sync").join(&found[0].space.0);
    let logs: String = walk(&space_dir).iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect();
    assert!(!logs.contains("Plan") && !logs.contains("Ship it"));
    b.stop_sync().unwrap();
    assert_eq!(b.sync.status, SyncStatus::Off);
}

#[test]
fn debug_output_leaves_secrets_out() {
    let t = SyncTarget::Server { url: "https://sync.example".into(), token: "TOKEN-SECRET-123".into() };
    let text = format!("{t:?}");
    assert!(text.contains("sync.example") && !text.contains("TOKEN-SECRET-123"), "{text}");
    let r = DeviceRemoved { recovery_key: "RK-SECRET-456".into(), left_out: vec!["phone".into()], advice: "x".into() };
    let text = format!("{r:?}");
    assert!(text.contains("phone") && !text.contains("RK-SECRET-456"), "{text}");
    let d = SetupDone::Created("RK-SECRET-789".into());
    assert!(!format!("{d:?}").contains("RK-SECRET-789"));
}

fn run_setup(ws: &mut Workspace) -> Vec<SetupDone> {
    let start = Instant::now();
    let mut out = Vec::new();
    while ws.sync_setup_running().is_some() {
        assert!(start.elapsed() < Duration::from_mins(1), "setup timed out");
        ws.tick(Instant::now());
        out.extend(ws.take_sync_setup());
        std::thread::sleep(Duration::from_millis(20));
    }
    ws.tick(Instant::now());
    out.extend(ws.take_sync_setup());
    out
}

fn setup_dirs() -> (tempfile::TempDir, PathBuf, PathBuf, SyncTarget) {
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, shared) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("Shared"));
    for d in [&va, &vb, &shared] {
        std::fs::create_dir_all(d).unwrap();
    }
    (dir, va, vb, SyncTarget::Folder(shared))
}

#[test]
fn setup_result_is_applied_though_settings_closed() {
    let (_d, va, _vb, target) = setup_dirs();
    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create_in_background(target, "correct horse battery".into(), "laptop".into()).unwrap();
    // Nobody polls the dialog: only the workspace tick runs.
    let start = Instant::now();
    while a.sync_setup_running().is_some() {
        assert!(start.elapsed() < Duration::from_mins(1));
        a.tick(Instant::now());
        std::thread::sleep(Duration::from_millis(20));
    }
    a.tick(Instant::now());
    assert!(a.sync.config.is_some(), "sync started");
    assert!(a.sync_recovery_key().is_some());
    let done = a.take_sync_setup();
    assert!(matches!(done.as_slice(), [SetupDone::Created(rk)] if !rk.is_empty()), "{done:?}");
    assert!(a.notices(Duration::from_mins(10)).any(|n| n.text.contains("Sync is on")));
    assert!(!format!("{done:?}").contains("recovery key:"), "debug hides the key");
}

#[test]
fn only_one_setup_step_runs_at_a_time() {
    let (_d, va, _vb, target) = setup_dirs();
    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create_in_background(target.clone(), "correct horse battery".into(), "laptop".into()).unwrap();
    assert!(a.sync_list_in_background(target.clone()).is_err());
    assert!(a.sync_create_in_background(target.clone(), "correct horse battery".into(), "laptop".into()).is_err());
    assert!(a.sync_join_in_background(target, SpaceId("x".into()), "s".into(), "d".into()).is_err());
    assert_eq!(a.sync_setup_running(), Some(SetupKind::Create));
    run_setup(&mut a);
    assert_eq!(a.sync_setup_running(), None);
}

#[test]
fn setup_result_for_another_vault_is_not_applied() {
    let (_d, va, vb, target) = setup_dirs();
    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create_in_background(target, "correct horse battery".into(), "laptop".into()).unwrap();
    a.open_vault(&vb).unwrap();
    let done = run_setup(&mut a);
    assert!(done.is_empty(), "nothing for the dialog: {done:?}");
    assert!(a.sync.config.is_none(), "sync not started on the vault now open");
    assert!(SyncConfig::load(&vb).is_none());
    assert!(SyncConfig::load(&va).is_some(), "the first vault was set up on disk");
    assert!(a.notices(Duration::from_mins(10)).any(|n| n.text.contains("another vault is open")));
}

#[test]
fn setup_failure_is_reported() {
    let (_d, va, _vb, target) = setup_dirs();
    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create_in_background(target, "short".into(), "laptop".into()).unwrap();
    let done = run_setup(&mut a);
    assert!(matches!(done.as_slice(), [SetupDone::Failed(SetupKind::Create, _)]), "{done:?}");
    assert!(a.notices(Duration::from_mins(10)).any(|n| n.text.contains("Sync setup failed")));
    // A worker that dies without answering is reported too.
    let (tx, rx) = crossbeam_channel::bounded::<SetupResult>(1);
    drop(tx);
    a.sync.setup.running = Some((SetupKind::Join, va, rx));
    let done = run_setup(&mut a);
    assert!(matches!(done.as_slice(), [SetupDone::Failed(SetupKind::Join, e)] if e.contains("stopped")), "{done:?}");
}

/// Drain a headless worker's events while ticking the app.
fn wait_both(a: &mut Workspace, b: &SyncWorker, what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(40), "timed out waiting for {what}: app {:?}", a.sync.status);
        a.tick(Instant::now());
        let _ = b.events();
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn files(root: &Path) -> Vec<String> {
    let v = Vault::open_local(root).unwrap();
    let mut out: Vec<String> = v.storage().list().unwrap().into_iter().map(|p| p.as_str().to_string()).collect();
    out.sort();
    out
}

#[test]
fn a_headless_device_and_the_app_converge() {
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, shared) = (dir.path().join("Desktop"), dir.path().join("Server"), dir.path().join("Syncthing"));
    for d in [&va, &vb, &shared] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Plan.md"), "# Plan\none\ntwo\nthree\n").unwrap();
    let target = SyncTarget::Folder(shared);
    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create(&target, "correct horse battery", "laptop").unwrap();

    // The headless side joins the way `quire-cli sync join` does.
    let space = sync_list(&target).unwrap().remove(0).space;
    assert!(sync_join_at(&vb, &target, &space, "wrong passphrase", "server", &Selection::default()).is_err());
    sync_join_at(&vb, &target, &space, "correct horse battery", "server", &Selection::default()).unwrap();
    let b = SyncWorker::start_headless(&vb, Duration::from_secs(5)).unwrap();
    wait_both(&mut a, &b, "the note to reach the server", || {
        std::fs::read_to_string(vb.join("Plan.md")).is_ok_and(|t| t.contains("three"))
    });

    // Each side's edits reach the other.
    std::fs::write(vb.join("From server.md"), "written headless\n").unwrap();
    b.nudge();
    wait_both(&mut a, &b, "the server's note to reach the app", || va.join("From server.md").exists());
    // Both change the same note before seeing each other: merged, or
    // both versions kept; never a lost line.
    std::fs::write(va.join("Plan.md"), "# Plan\nONE (laptop)\ntwo\nthree\n").unwrap();
    std::fs::write(vb.join("Plan.md"), "# Plan\none\ntwo\nthree\nfour (server)\n").unwrap();
    a.sync_nudge();
    b.nudge();
    let all_text = |root: &Path| -> String { files(root).iter().filter_map(|p| std::fs::read_to_string(root.join(p)).ok()).collect() };
    wait_both(&mut a, &b, "both edits on both sides, and the same files", || {
        let (ta, tb) = (all_text(&va), all_text(&vb));
        files(&va) == files(&vb) && [&ta, &tb].iter().all(|t| t.contains("ONE (laptop)") && t.contains("four (server)"))
    });
    // Off by default: an agent's suggestion on the server stays there,
    // and no review history is written on either side.
    let server_store =
        quire_agent::AgentStore::open(&Vault::open_local(&vb).unwrap(), crate::device::this_device(), Arc::new(quire_agent::SystemClock));
    let agent = quire_agent::Actor::agent("oauth:claude.ai", "Claude", quire_agent::Via::McpHttp, &server_store.device().id);
    server_store
        .submit(
            &agent,
            quire_agent::ChangeSet::one(quire_agent::Change::Edit { path: NotePath::new("Plan.md"), new_text: "suggested\n".into() }),
        )
        .unwrap();
    std::fs::write(vb.join("Marker.md"), "after the suggestion\n").unwrap();
    b.nudge();
    wait_both(&mut a, &b, "the marker to reach the app", || va.join("Marker.md").exists());
    assert!(!va.join(".quire/shared/agent").exists() && !vb.join(".quire/shared/agent").exists());
    assert!(a.agent_store().unwrap().proposals(&quire_agent::ProposalFilter::pending()).is_empty());
    let summary = sync_summary_at(&vb).unwrap();
    assert_eq!((summary.config.device_name.as_str(), summary.has_secrets), ("server", true));
    assert!(summary.last.is_some_and(|l| l.state == "synced" || l.state == "syncing"));
    b.stop_and_wait();
    // A one-off pass works too, and finds nothing left to do.
    let r = sync_once_at(&vb).unwrap();
    assert_eq!(r.pushed, 0);
}

fn remote_targets() -> Vec<(remote::mock::MockServer, SyncTarget)> {
    use quire_sync::remote::mock::{Kind, MockServer};
    let s3 =
        MockServer::start(Kind::S3 { bucket: "notes".into(), access_key: "AK".into(), secret_key: "SK/+".into(), region: "auto".into() });
    let s3_target = SyncTarget::Remote {
        profile: RemoteProfile::S3 {
            endpoint: s3.origin(),
            region: "auto".into(),
            bucket: "notes".into(),
            prefix: "q".into(),
            virtual_hosted: false,
            allow_http: false,
        },
        secret: RemoteSecret::S3 { access_key: "AK".into(), secret_key: "SK/+".into(), session_token: None },
    };
    let dav = MockServer::start(Kind::WebDav { user: "asha".into(), password: "pw".into() });
    let dav_target = SyncTarget::Remote {
        profile: RemoteProfile::Webdav { url: dav.dav_url(), allow_http: false },
        secret: RemoteSecret::Webdav { user: "asha".into(), password: "pw".into() },
    };
    vec![(s3, s3_target), (dav, dav_target)]
}

/// 0.12: a vault syncs through S3 and WebDAV as through a folder: create,
/// join (a wrong passphrase refused), selective sync, settings, new
/// credentials, and no secret in the vault.
#[test]
fn s3_and_webdav_vaults_sync_end_to_end() {
    for (srv, target) in remote_targets() {
        let dir = tempfile::tempdir().unwrap();
        let (va, vb, vc) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("C"));
        for d in [&va, &vb, &vc] {
            std::fs::create_dir_all(d.join("Work")).unwrap();
        }
        std::fs::write(va.join("Plan.md"), "# Plan\none\n").unwrap();
        std::fs::write(va.join("Work/Task.md"), "work\n").unwrap();
        assert!(sync_test(&target).unwrap().starts_with("Connected"));
        sync_create_at(&va, &target, "correct horse battery", "laptop", &Selection::default()).unwrap();
        sync_once_at(&va).unwrap();
        let space = sync_list(&target).unwrap().remove(0).space;
        assert!(sync_join_at(&vb, &target, &space, "wrong passphrase", "phone", &Selection::default()).is_err());
        sync_join_at(&vb, &target, &space, "correct horse battery", "phone", &Selection::default()).unwrap();
        let only_work = Selection::new(&["Work".into()], &[]).unwrap();
        sync_join_at(&vc, &target, &space, "correct horse battery", "server", &only_work).unwrap();
        for v in [&vb, &vc] {
            sync_once_at(v).unwrap();
        }
        assert_eq!(files(&va), files(&vb));
        assert_eq!(files(&vc), ["Work/Task.md"], "selective sync holds only Work/");
        // Edits both ways, and the settings switch (no older readers to keep out).
        std::fs::write(vb.join("From phone.md"), "hi\n").unwrap();
        std::fs::write(vc.join("Work/Task.md"), "work\ndone\n").unwrap();
        sync_share_at(&va, true).unwrap();
        for _ in 0..2 {
            for v in [&va, &vb, &vc] {
                sync_once_at(v).unwrap();
            }
        }
        assert_eq!(files(&va), files(&vb));
        assert_eq!(std::fs::read_to_string(va.join("Work/Task.md")).unwrap(), "work\ndone\n");
        assert!(!vc.join("Plan.md").exists());
        // config.json holds no secret, and Quire 0.11 can't take it for a server.
        let cfg_text = std::fs::read_to_string(va.join(".quire/sync/config.json")).unwrap();
        for secret in ["SK/+", "\"pw\"", "correct horse"] {
            assert!(!cfg_text.contains(secret), "{cfg_text}");
        }
        assert!(cfg_text.contains("remote_device") && !cfg_text.contains("\"device\""), "{cfg_text}");
        let summary = sync_summary_at(&va).unwrap();
        assert!(summary.has_secrets);
        assert!(summary.config.transport_label().contains("S3 bucket notes/q") || summary.config.transport_label().contains("WebDAV"));
        // Expired credentials say so; new ones are checked before they're kept.
        srv.state.expired.store(true, Ordering::Relaxed);
        let e = sync_once_at(&va).unwrap_err();
        assert!(e.contains("expired") || e.contains("user name or password"), "{e}");
        srv.state.expired.store(false, Ordering::Relaxed);
        let SyncTarget::Remote { secret, .. } = &target else { unreachable!() };
        let wrong = match secret {
            RemoteSecret::S3 { access_key, .. } => {
                RemoteSecret::S3 { access_key: access_key.clone(), secret_key: "nope".into(), session_token: None }
            }
            RemoteSecret::Webdav { user, .. } => RemoteSecret::Webdav { user: user.clone(), password: "nope".into() },
        };
        assert!(sync_set_remote_secret_at(&va, &wrong).is_err());
        assert!(sync_set_remote_secret_at(&va, secret).unwrap().starts_with("Connected"));
        sync_once_at(&va).unwrap();
        // The storage only ever saw ciphertext and ids.
        for (k, v) in srv.objects() {
            assert!(!k.contains("Plan") && !String::from_utf8_lossy(&v).contains("Plan"), "{k}");
        }
    }
}

/// `sync move`: from a synced folder to WebDAV; the other device joins
/// the new vault and its notes are adopted, not doubled.
#[test]
fn a_vault_moves_from_a_folder_to_webdav() {
    let (_srv, dav) = remote_targets().pop().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, shared) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("Dropbox"));
    for d in [&va, &vb, &shared] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
    let folder = SyncTarget::Folder(shared);
    sync_create_at(&va, &folder, "correct horse battery", "laptop", &Selection::default()).unwrap();
    sync_once_at(&va).unwrap();
    let space = sync_list(&folder).unwrap().remove(0).space;
    sync_join_at(&vb, &folder, &space, "correct horse battery", "phone", &Selection::default()).unwrap();
    sync_once_at(&vb).unwrap();
    std::fs::write(va.join("Late.md"), "written just before the move\n").unwrap();
    let (_rk, last) = sync_move_at(&va, &dav, "another passphrase", false).unwrap();
    assert!(last.is_none());
    assert!(SyncConfig::load(&va).unwrap().transport_label().starts_with("WebDAV"));
    sync_once_at(&va).unwrap();
    // B (here in the same test process, so A's stop took the shared
    // key from this secrets file too): stop, then join the new vault.
    sync_stop_at(&vb);
    let new_space = sync_list(&dav).unwrap().remove(0).space;
    sync_join_at(&vb, &dav, &new_space, "another passphrase", "phone", &Selection::default()).unwrap();
    sync_once_at(&vb).unwrap();
    sync_once_at(&va).unwrap();
    assert_eq!(files(&va), ["Late.md", "Plan.md"]);
    assert_eq!(files(&va), files(&vb), "adopted, no copies");
}

/// 0.12 review (security L4, data M1): a move that fails to make the
/// new vault leaves the old setup working; changes not yet uploaded to
/// the old storage stop the move unless forced.
#[test]
fn a_failed_move_keeps_the_old_setup_and_unsent_changes_stop_it() {
    let (srv, dav) = remote_targets().pop().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (va, shared, other) = (dir.path().join("A"), dir.path().join("Dropbox"), dir.path().join("Elsewhere"));
    for d in [&va, &shared, &other] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
    let folder = SyncTarget::Folder(shared);
    sync_create_at(&va, &folder, "correct horse battery", "laptop", &Selection::default()).unwrap();
    sync_once_at(&va).unwrap();
    let before = SyncConfig::load(&va).unwrap();
    // The test connection's writes succeed, then the storage drops the
    // connection: making the new vault fails.
    srv.state.puts_left.store(2, Ordering::Relaxed);
    assert!(sync_move_at(&va, &dav, "another passphrase", false).is_err());
    let after = SyncConfig::load(&va).unwrap();
    assert_eq!((&after.space, after.transport_label()), (&before.space, before.transport_label()));
    std::fs::write(va.join("Still.md"), "still syncing\n").unwrap();
    sync_once_at(&va).unwrap();
    // Now on WebDAV, with a change that can't be uploaded.
    srv.state.puts_left.store(u64::MAX, Ordering::Relaxed);
    sync_move_at(&va, &dav, "another passphrase", false).unwrap();
    sync_once_at(&va).unwrap();
    std::fs::write(va.join("Unsent.md"), "made offline\n").unwrap();
    // The pass's tip write goes through, then the connection drops: the
    // new revision stays in the outbox.
    srv.state.puts_left.store(1, Ordering::Relaxed);
    let to = SyncTarget::Folder(other);
    let e = sync_move_at(&va, &to, "third passphrase", false).unwrap_err();
    assert!(e.contains("change(s) made here haven't reached WebDAV"), "{e}");
    assert!(SyncConfig::load(&va).unwrap().transport_label().starts_with("WebDAV"));
    let (_rk, last) = sync_move_at(&va, &to, "third passphrase", true).unwrap();
    assert!(last.is_some());
    sync_once_at(&va).unwrap();
    assert!(SyncConfig::load(&va).unwrap().transport_label().starts_with("folder"));
}

/// A device that hears of a key change while the storage is failing tries
/// the key refresh again on its next pass, in the same session: it doesn't
/// keep the old key until another key change or a restart.
#[test]
fn a_key_refresh_that_fails_is_tried_again_next_pass() {
    let (srv, target) = remote_targets().remove(0);
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, vc) = (dir.path().join("A"), dir.path().join("B"), dir.path().join("C"));
    for d in [&va, &vb, &vc] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
    sync_create_at(&va, &target, "correct horse battery", "laptop", &Selection::default()).unwrap();
    sync_once_at(&va).unwrap();
    let space = sync_list(&target).unwrap().remove(0).space;
    sync_join_at(&vb, &target, &space, "correct horse battery", "phone", &Selection::default()).unwrap();
    sync_join_at(&vc, &target, &space, "correct horse battery", "tablet", &Selection::default()).unwrap();
    for v in [&vb, &vc] {
        sync_once_at(v).unwrap();
    }
    // B's long-lived session (as in the worker) has made its first pass.
    let (vault, cfg, token, key) = open_parts(&vb).unwrap();
    let mut b = Session::new(vb.clone(), vault, cfg, &token, key, lock_sync(&vb).unwrap()).unwrap();
    assert!(matches!(b.pass(APP_POLL).status, SyncStatus::Synced(_)));
    // A removes the tablet: a new vault key, and a note sealed with it.
    let tablet = sync_devices_at(&va).unwrap().devices.into_iter().find(|d| d.label.starts_with("tablet")).unwrap().id;
    sync_remove_device_at(&va, &tablet, "second pass phrase").unwrap();
    std::fs::write(va.join("After.md"), "after the removal\n").unwrap();
    sync_once_at(&va).unwrap();
    // B hears of it, and the storage fails on the pass that would take the key.
    b.pass(APP_POLL);
    assert!(!vb.join("After.md").exists());
    srv.state.expired.store(true, Ordering::Relaxed);
    assert!(!matches!(b.pass(APP_POLL).status, SyncStatus::Synced(_)));
    srv.state.expired.store(false, Ordering::Relaxed);
    let status = b.pass(APP_POLL).status;
    assert!(matches!(status, SyncStatus::Synced(_)), "the refresh wasn't tried again: {status:?}");
    assert_eq!(std::fs::read_to_string(vb.join("After.md")).unwrap(), "after the removal\n", "the refresh was tried again");
}

/// Review finding 5: one sync process per vault and device. A second
/// worker (the app next to `serve --sync`) waits and says so, then takes
/// over; one-off commands refuse.
#[test]
fn one_sync_process_per_vault_and_device() {
    let dir = tempfile::tempdir().unwrap();
    let (va, shared) = (dir.path().join("Vault"), dir.path().join("Syncthing"));
    for d in [&va, &shared] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Plan.md"), "# Plan\n").unwrap();
    sync_create_at(&va, &SyncTarget::Folder(shared), "correct horse battery", "server", &Selection::default()).unwrap();
    let first = SyncWorker::start_headless(&va, Duration::from_mins(1)).unwrap();
    let synced = |w: &SyncWorker, what: &str| {
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < Duration::from_secs(30), "{what}");
            if let Some(SyncEvent::Status(SyncStatus::Synced(_))) = w.next_event(Duration::from_millis(100)) {
                return;
            }
        }
    };
    synced(&first, "the first worker never synced");
    assert!(matches!(lock_sync(&va), Err(LockError::Busy { .. })));
    let once = sync_once_at(&va).unwrap_err();
    assert!(once.contains(ALREADY_SYNCING), "{once}");
    let scope = sync_set_selection_at(&va, &Selection::new(&["Work".into()], &[]).unwrap()).unwrap_err();
    assert!(scope.contains(ALREADY_SYNCING) && scope.contains("Stop it first"), "{scope}");
    assert!(SyncConfig::load(&va).unwrap().selection().is_everything(), "nothing changed");

    let second = SyncWorker::start_headless(&va, Duration::from_mins(1)).unwrap();
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(30), "the second worker never said why it waits");
        match second.next_event(Duration::from_millis(100)) {
            Some(SyncEvent::Status(SyncStatus::Elsewhere(e))) => {
                assert!(e.contains(ALREADY_SYNCING), "{e}");
                break;
            }
            Some(SyncEvent::Status(SyncStatus::Synced(_))) => panic!("two workers synced the same vault"),
            _ => {}
        }
    }
    first.stop_and_wait();
    synced(&second, "the second worker never took over");
    second.stop_and_wait();
    sync_once_at(&va).unwrap();
    // The lock goes with the process; the file stays, harmlessly.
    assert!(sync_dir(&va).join("lock").exists());
    drop(lock_sync(&va).unwrap());
}

/// Quire 0.9 needs `device` in config.json: with a selection set, it
/// doesn't find it, so it shows sync as off instead of syncing the
/// folder from an empty state. Quire 0.10 reads both.
#[test]
fn a_selective_config_is_off_limits_to_0_9() {
    #[derive(Deserialize)]
    #[expect(dead_code, reason = "only parsed")]
    struct Config09 {
        space: SpaceId,
        device: DeviceId,
        device_name: String,
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = SyncConfig {
        server: String::new(),
        folder: Some(dir.path().join("f")),
        space: SpaceId::random().unwrap(),
        device: DeviceId::random().unwrap(),
        device_name: "server".into(),
        shared_state: false,
        only: vec!["Work/".into()],
        exclude: Vec::new(),
        remote: None,
    };
    cfg.save(dir.path()).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(sync_dir(dir.path()).join("config.json")).unwrap()).unwrap();
    assert!(raw.get("device").is_none() && raw.get(PARTIAL_DEVICE).is_some());
    assert!(serde_json::from_value::<Config09>(raw).is_err(), "0.9 can't load it");
    assert_eq!(SyncConfig::load(dir.path()), Some(cfg.clone()));
    cfg.only.clear();
    cfg.save(dir.path()).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(sync_dir(dir.path()).join("config.json")).unwrap()).unwrap();
    assert!(serde_json::from_value::<Config09>(raw).is_ok(), "everything again: 0.9 syncs it as before");
    assert_eq!(SyncConfig::load(dir.path()), Some(cfg));
}

/// A note changed by the editor (or the web app, or an agent) after the
/// pass read it is never overwritten by sync, even between its check and
/// its write.
#[test]
fn sync_writes_check_the_file_as_one_step() {
    let vault = Vault::in_memory(&[("Plan.md", "one\n")]);
    let mut store = VaultStore::new(vault.clone());
    snapshot_reads(&mut store).unwrap();
    vault.write(&NotePath::new("Plan.md"), "one, saved meanwhile\n").unwrap();
    assert!(store.write("Plan.md", "from sync\n").is_err());
    assert!(store.remove("Plan.md").is_err());
    assert_eq!(vault.read(&NotePath::new("Plan.md")).unwrap(), "one, saved meanwhile\n");
    snapshot_reads(&mut store).unwrap();
    store.write("Plan.md", "from sync\n").unwrap();
    assert_eq!(vault.read(&NotePath::new("Plan.md")).unwrap(), "from sync\n");
    assert!(store.write("New.md", "new\n").is_ok(), "a new file, where none was read");
}

#[test]
fn a_selective_device_never_deletes_what_it_doesnt_hold() {
    let dir = tempfile::tempdir().unwrap();
    let (va, vb, shared) = (dir.path().join("Desktop"), dir.path().join("Server"), dir.path().join("Dropbox"));
    for d in [&va, &vb, &shared, &va.join("Work"), &va.join("Journal"), &vb.join("Journal")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(va.join("Work/Plan.md"), "plan\n").unwrap();
    std::fs::write(va.join("Journal/Today.md"), "private day\n").unwrap();
    std::fs::write(va.join("Inbox.md"), "inbox\n").unwrap();
    // Something already on the server, outside its selection.
    std::fs::write(vb.join("Journal/Stale.md"), "never synced\n").unwrap();
    let target = SyncTarget::Folder(shared);
    let mut a = Workspace::new(crate::Settings::default());
    a.open_vault(&va).unwrap();
    a.sync_create(&target, "correct horse battery", "laptop").unwrap();
    let space = sync_list(&target).unwrap().remove(0).space;
    let only = Selection::new(&["Work".to_string()], &[]).unwrap();
    sync_join_at(&vb, &target, &space, "correct horse battery", "server", &only).unwrap();
    let start = Instant::now();
    while !vb.join("Work/Plan.md").exists() {
        assert!(start.elapsed() < Duration::from_secs(30), "the selected note never arrived");
        a.tick(Instant::now());
        let _ = sync_once_at(&vb);
        std::thread::sleep(Duration::from_millis(100));
    }
    for _ in 0..5 {
        a.tick(Instant::now());
        sync_once_at(&vb).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(files(&vb), ["Journal/Stale.md", "Work/Plan.md"], "only the selection is written here");
    assert!(!dir.path().join("Desktop/Journal/Stale.md").exists(), "unselected files here are never sent");
    a.sync_nudge();
    std::fs::write(va.join("Journal/Today.md"), "private day, later\n").unwrap();
    for _ in 0..5 {
        a.tick(Instant::now());
        sync_once_at(&vb).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(files(&va), ["Inbox.md", "Journal/Today.md", "Work/Plan.md"], "nothing deleted on the app's side");
    assert_eq!(std::fs::read_to_string(va.join("Journal/Today.md")).unwrap(), "private day, later\n");
    let s = sync_summary_at(&vb).unwrap();
    assert_eq!((s.tracked, s.outside), (3, 2));
    assert!(sync_dir(&vb).join("state-partial.json").exists() && !sync_dir(&vb).join("state.json").exists());
    // Widening fetches the rest, still deleting nothing.
    sync_set_selection_at(&vb, &Selection::default()).unwrap();
    sync_once_at(&vb).unwrap();
    assert!(vb.join("Journal/Today.md").exists() && vb.join("Inbox.md").exists());
    assert!(sync_dir(&vb).join("state.json").exists());
    assert_eq!(std::fs::read_to_string(va.join("Work/Plan.md")).unwrap(), "plan\n");
}

/// Two vaults syncing through a folder, each pass as the app runs it.
struct Pair {
    _dir: tempfile::TempDir,
    va: PathBuf,
    vb: PathBuf,
    ea: Engine,
    eb: Engine,
    ta: FolderTransport,
    tb: FolderTransport,
}

impl Pair {
    fn new(notes: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = folder::sync_root(&dir.path().join("Dropbox"));
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
        let (va, vb) = (dir.path().join("A"), dir.path().join("B"));
        std::fs::create_dir_all(&vb).unwrap();
        for (p, t) in notes {
            std::fs::create_dir_all(va.join(p).parent().unwrap()).unwrap();
            std::fs::write(va.join(p), t).unwrap();
        }
        let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
        let ea = Engine::new(space.clone(), da.clone(), "a", key.keys(), SyncState::default());
        let eb = Engine::new(space.clone(), db.clone(), "b", key.keys(), SyncState::default());
        let (ta, tb) = (FolderTransport::new(&root, &space, da, None), FolderTransport::new(&root, &space, db, None));
        let mut p = Self { _dir: dir, va, vb, ea, eb, ta, tb };
        p.a().unwrap();
        p.b().unwrap();
        p
    }

    fn pass(e: &mut Engine, root: &Path, t: &FolderTransport) -> Result<Report, String> {
        let v = Vault::open_local(root).map_err(|e| e.to_string())?;
        let mut st = VaultStore::new(v);
        snapshot_reads(&mut st)?;
        e.sync(&mut st, t, "2026-11-16").map_err(|e| e.to_string())
    }

    fn a(&mut self) -> Result<Report, String> {
        Self::pass(&mut self.ea, &self.va, &self.ta)
    }

    fn b(&mut self) -> Result<Report, String> {
        Self::pass(&mut self.eb, &self.vb, &self.tb)
    }
}

/// Review D-M1: locking a folder that holds most of the vault replaces
/// each plain note by its encrypted file. That is no deletion to stop
/// for: the pass goes through, the other device gets the encrypted
/// files and loses the plain ones, and no plain text stays in the state.
#[test]
fn locking_most_of_a_vault_syncs_and_leaves_no_plain_text() {
    let mut notes: Vec<(String, String)> = (0..25).map(|i| (format!("Journal/{i:02}.md"), format!("secret day {i}\n"))).collect();
    notes.extend((0..15).map(|i| (format!("Notes/{i:02}.md"), format!("note {i}\n"))));
    let refs: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let mut p = Pair::new(&refs);
    let va = Vault::open_local(&p.va).unwrap();
    quire_vault::lock::lock_folder(&va, "Journal", Some("a passphrase for the test"), quire_vault::lock::Kdf::TEST, 1).unwrap();
    // Even before the deletes go (a pass that fails for another
    // reason), the plain text of the locked notes leaves the state.
    let mut waiting = p.ea.state.clone();
    assert!(serde_json::to_string(&waiting).unwrap().contains("secret day"));
    scrub_locked_text(&va, &mut waiting);
    assert!(!serde_json::to_string(&waiting).unwrap().contains("secret day"));
    p.a().unwrap();
    p.b().unwrap();
    let b = files(&p.vb);
    assert!(
        !b.iter().any(|f| f.starts_with("Journal/") && Path::new(f).extension().is_some_and(|e| e == "md")),
        "plain copies gone: {b:?}"
    );
    assert_eq!(b.iter().filter(|f| f.ends_with(".md.qenc")).count(), 25, "{b:?}");
    let state = std::fs::read_to_string(p.va.join(".quire/sync/state.json")).unwrap_or_default();
    let state = if state.is_empty() { serde_json::to_string(&p.ea.state).unwrap() } else { state };
    assert!(!state.contains("secret day"), "no plain text of a locked note in the sync state");
    // Unlocking for good the other way round: the encrypted files go.
}

/// Review D-M2: the agent's own pruning of review history (more than
/// half of the settings lane at once) never stops that lane.
#[test]
fn pruning_review_history_never_stops_the_settings_lane() {
    let dir = tempfile::tempdir().unwrap();
    let root = folder::sync_root(&dir.path().join("Dropbox"));
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
    folder::open_lane(&root, &space, folder::SHARED_LANE).unwrap();
    let (va, vb) = (dir.path().join("A"), dir.path().join("B"));
    std::fs::create_dir_all(va.join(".quire/shared/agent/proposals/me")).unwrap();
    std::fs::create_dir_all(&vb).unwrap();
    for i in 0..30 {
        std::fs::write(va.join(format!(".quire/shared/agent/proposals/me/p{i:02}.json")), format!("{{\"n\":{i}}}")).unwrap();
    }
    std::fs::write(va.join(".quire/shared/vault.toml"), "format = 1\n").unwrap();
    let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
    let lane = |d: &DeviceId| FolderTransport::lane(&root, &space, d.clone(), None, folder::SHARED_LANE);
    let (ta, tb) = (lane(&da), lane(&db));
    let mut ea = Engine::new(space.clone(), da, "a", key.keys(), SyncState::default());
    let mut eb = Engine::new(space.clone(), db, "b", key.keys(), SyncState::default());
    let pass = |e: &mut Engine, v: &Path, t: &FolderTransport| {
        let mut st = VaultStore::new(Vault::open_local(v).unwrap()).shared_only();
        snapshot_reads(&mut st).unwrap();
        e.sync(&mut st, t, "2026-11-16")
    };
    pass(&mut ea, &va, &ta).unwrap();
    pass(&mut eb, &vb, &tb).unwrap();
    assert!(vb.join(".quire/shared/agent/proposals/me/p00.json").exists());
    // Thirty days later the agent prunes what was resolved.
    std::fs::remove_dir_all(va.join(".quire/shared/agent/proposals/me")).unwrap();
    pass(&mut ea, &va, &ta).unwrap();
    pass(&mut eb, &vb, &tb).unwrap();
    assert!(!vb.join(".quire/shared/agent/proposals/me/p00.json").exists());
    assert!(vb.join(".quire/shared/vault.toml").exists());
}

/// Review D-M3: iCloud (macOS 13 and older) evicts a note to a hidden
/// `.Note.md.icloud` stub; the note is not deleted, so no delete is sent.
#[test]
fn a_note_evicted_to_an_icloud_stub_is_not_sent_as_deleted() {
    let mut p = Pair::new(&[("Plan.md", "plan\n"), ("Notes/Idea.md", "idea\n")]);
    std::fs::remove_file(p.va.join("Notes/Idea.md")).unwrap();
    std::fs::write(p.va.join("Notes/.Idea.md.icloud"), b"bplist00").unwrap();
    p.a().unwrap();
    p.b().unwrap();
    assert_eq!(files(&p.vb), ["Notes/Idea.md", "Plan.md"]);
}

/// Review D-L4: a note whose name is too long to go to the trash with
/// the trash's date in front still goes there when another device
/// deletes it, so that delete doesn't fail every pass.
#[test]
fn a_note_with_a_very_long_name_can_be_deleted_by_another_device() {
    let long = format!("{}.md", "n".repeat(243));
    let mut p = Pair::new(&[(long.as_str(), "long\n"), ("Plan.md", "plan\n")]);
    std::fs::remove_file(p.va.join(&long)).unwrap();
    p.a().unwrap();
    p.b().unwrap();
    assert_eq!(files(&p.vb), ["Plan.md"]);
    assert!(
        std::fs::read_dir(p.vb.join(".trash")).unwrap().flatten().any(|e| Path::new(&e.file_name()).extension().is_some_and(|x| x == "md"))
    );
}

/// Reliability review #1: a vault folder that can't be listed (a USB
/// drive pulled out, a network share that dropped) is never read as
/// "every note was deleted here".
#[test]
fn a_vault_that_cant_be_listed_sends_no_deletes() {
    let mut p = Pair::new(&[("Plan.md", "plan\n"), ("Work/Notes.md", "notes\n"), ("Work/Ideas.md", "ideas\n")]);
    assert_eq!(files(&p.vb), ["Plan.md", "Work/Ideas.md", "Work/Notes.md"]);
    // The vault's folder is gone: the store's listing fails.
    let v = Vault::open_local(&p.va).unwrap();
    let mut st = VaultStore::new(v);
    snapshot_reads(&mut st).unwrap();
    let away = p.va.with_file_name("A-unplugged");
    std::fs::rename(&p.va, &away).unwrap();
    let err = p.ea.sync(&mut st, &p.ta, "2026-11-16").unwrap_err();
    assert!(matches!(err, quire_sync::SyncError::List(_)), "{err}");
    p.b().unwrap();
    assert_eq!(files(&p.vb), ["Plan.md", "Work/Ideas.md", "Work/Notes.md"], "nothing deleted on the other device");
    std::fs::rename(&away, &p.va).unwrap();
    p.a().unwrap();
    p.b().unwrap();
    assert_eq!(files(&p.vb), ["Plan.md", "Work/Ideas.md", "Work/Notes.md"]);
}

/// Reliability review #1: a vault folder that is suddenly empty (a
/// drive's mount point with the drive gone) sends nothing until the
/// person says the deletes are meant, and the message says how.
#[test]
fn many_deletes_wait_until_the_person_allows_them() {
    let (_dir, va, _, target) = setup_dirs();
    for i in 0..20 {
        std::fs::write(va.join(format!("{i:02}.md")), format!("note {i}\n")).unwrap();
    }
    sync_create_at(&va, &target, "correct horse battery", "laptop", &Selection::default()).unwrap();
    sync_once_at(&va).unwrap();
    for i in 0..12 {
        std::fs::remove_file(va.join(format!("{i:02}.md"))).unwrap();
    }
    let err = sync_once_at(&va).unwrap_err();
    assert!(err.contains("12 of the 20 notes") && err.contains("Send these deletes in Settings → Sync"), "{err}");
    assert_eq!(LastPass::load(&va).unwrap().state, "error");
    let held = HeldDeletes::load(&va).unwrap();
    assert_eq!(held.notes.len(), 12);
    assert_eq!(held.notes[0].path, "00.md");
    let seen = held.id();
    // Review F-L3: before the person approves, the rest goes missing
    // too. Approving what they saw is refused; nothing more is sent.
    for i in 12..20 {
        std::fs::remove_file(va.join(format!("{i:02}.md"))).unwrap();
    }
    assert!(sync_once_at(&va).is_err());
    let err = sync_allow_deletes_at(&va, &seen).unwrap_err();
    assert!(err.contains("now 20 instead of 12"), "{err}");
    assert!(sync_once_at(&va).is_err(), "still held back");
    let seen = HeldDeletes::load(&va).unwrap().id();
    // An allowance that has run out sends nothing.
    assert_eq!(sync_allow_deletes_at(&va, &seen).unwrap(), 20);
    let mut h = HeldDeletes::load(&va).unwrap();
    h.allowed_until = Some(crate::history_ops::now_ms() - 1);
    h.save(&va).unwrap();
    assert!(sync_once_at(&va).is_err());
    // Allowed now: exactly those go, and the record of them with them.
    assert_eq!(sync_allow_deletes_at(&va, &seen).unwrap(), 20);
    let r = sync_once_at(&va).unwrap();
    assert_eq!(r.pushed, 20);
    assert!(HeldDeletes::load(&va).is_none(), "allowed once");
    assert!(sync_allow_deletes_at(&va, &seen).is_err(), "nothing held back now");
}

/// Review F-I3: deleting `.quire/shared/` (to "reset" settings) asks
/// before every vault setting is deleted on every device; one settings
/// file, or the review history the agent prunes, never asks.
#[test]
fn deleting_the_shared_settings_asks_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = folder::sync_root(&dir.path().join("Dropbox"));
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
    folder::open_lane(&root, &space, folder::SHARED_LANE).unwrap();
    let (va, vb) = (dir.path().join("A"), dir.path().join("B"));
    std::fs::create_dir_all(va.join(".quire/shared/srs")).unwrap();
    std::fs::create_dir_all(va.join(".quire/shared/agent/proposals/me")).unwrap();
    std::fs::create_dir_all(&vb).unwrap();
    for (p, t) in
        [("vault.toml", "format = 1\n"), ("agent-policy.toml", "x = 1\n"), ("srs/d1.jsonl", "{}\n"), ("agent/proposals/me/p.json", "{}")]
    {
        std::fs::write(va.join(".quire/shared").join(p), t).unwrap();
    }
    let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
    let lane = |d: &DeviceId| FolderTransport::lane(&root, &space, d.clone(), None, folder::SHARED_LANE);
    let (ta, tb) = (lane(&da), lane(&db));
    let mut ea = Engine::new(space.clone(), da, "a", key.keys(), SyncState::default());
    let mut eb = Engine::new(space.clone(), db, "b", key.keys(), SyncState::default());
    let pass = |e: &mut Engine, v: &Path, t: &FolderTransport| {
        let mut st = VaultStore::new(Vault::open_local(v).unwrap()).shared_only();
        snapshot_reads(&mut st).unwrap();
        e.sync(&mut st, t, "2026-11-16")
    };
    pass(&mut ea, &va, &ta).unwrap();
    pass(&mut eb, &vb, &tb).unwrap();
    // One file: sent.
    std::fs::remove_file(va.join(".quire/shared/agent-policy.toml")).unwrap();
    pass(&mut ea, &va, &ta).unwrap();
    // The whole folder: held back.
    std::fs::remove_dir_all(va.join(".quire/shared")).unwrap();
    let Err(quire_sync::SyncError::SettingsDeletes { notes }) = pass(&mut ea, &va, &ta) else { panic!("sent") };
    assert_eq!(notes.len(), 2, "vault.toml and the review log, not the agent's history: {notes:?}");
    pass(&mut eb, &vb, &tb).unwrap();
    assert!(vb.join(".quire/shared/vault.toml").exists() && !vb.join(".quire/shared/agent-policy.toml").exists());
    ea.allowed_deletes = notes.into_iter().map(|(id, _)| id).collect();
    pass(&mut ea, &va, &ta).unwrap();
    pass(&mut eb, &vb, &tb).unwrap();
    assert!(!vb.join(".quire/shared/vault.toml").exists());
}

/// Reliability review #14: a damaged state file is kept and reported,
/// not silently replaced; one that can't be read stops the session.
#[test]
fn a_damaged_sync_state_is_kept_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(sync_dir(dir.path())).unwrap();
    assert_eq!(open_state_named(dir.path(), "state.json").unwrap(), (SyncState::default(), None));
    let path = sync_dir(dir.path()).join("state.json");
    std::fs::write(&path, b"{\"cursor\": 12, \"fil").unwrap();
    let (state, notice) = open_state_named(dir.path(), "state.json").unwrap();
    assert_eq!(state, SyncState::default());
    assert!(notice.is_some_and(|n| n.contains("started afresh")));
    assert_eq!(std::fs::read(sync_dir(dir.path()).join("state.json.unreadable")).unwrap(), b"{\"cursor\": 12, \"fil");
    // Saved whole, with no temporary file left.
    let s = SyncState { cursor: 7, ..SyncState::default() };
    save_state_named(dir.path(), "state.json", &s).unwrap();
    assert_eq!(open_state_named(dir.path(), "state.json").unwrap().0.cursor, 7);
    assert!(!std::fs::read_dir(sync_dir(dir.path())).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".tmp")));
    // A folder where the file should be: can't be read, so no fresh start.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir_all(&path).unwrap();
    assert!(open_state_named(dir.path(), "state.json").is_err());
}

/// Reliability review #3, as the app's store sees it: on a disk that
/// ignores capitals, a note whose name differs only in capitals from a
/// file here can't be stored (its own old name doesn't count).
#[test]
fn names_that_differ_only_in_capitals_are_one_file_on_some_disks() {
    let vault = Vault::in_memory(&[("Plan.md", "big plan\n"), ("Notes/Idea.md", "idea\n")]);
    let mut store = VaultStore::new(vault);
    store.case_blind.set(Some(true));
    snapshot_reads(&mut store).unwrap();
    assert!(store.unstorable("plan.md", None).is_some_and(|why| why.contains("“Plan.md”")));
    assert!(store.unstorable("NOTES/idea.md", None).is_some());
    assert_eq!(store.unstorable("plan.md", Some("Plan.md")), None, "the note's own file");
    assert_eq!(store.unstorable("Plan.md", None), None, "the same name");
    assert_eq!(store.unstorable("Other.md", None), None);
    store.remove("Plan.md").unwrap();
    assert_eq!(store.unstorable("plan.md", None), None, "gone, so it can be stored");
    // On a disk that tells capitals apart, both are fine.
    let mut store = VaultStore::new(Vault::in_memory(&[("Plan.md", "big plan\n")]));
    snapshot_reads(&mut store).unwrap();
    assert_eq!(store.unstorable("plan.md", None), None);
}

/// Review D-L3, on a real macOS disk: two notes whose names are one
/// name to APFS (`é` written as one character and as two) end with one
/// skipped and reported, and the pass finishes (no copy every pass).
#[cfg(target_os = "macos")]
#[test]
fn names_the_disk_takes_as_one_are_skipped_not_copied() {
    let dir = tempfile::tempdir().unwrap();
    let root = folder::sync_root(&dir.path().join("Dropbox"));
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
    let (dl, dm) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
    let mut el = Engine::new(space.clone(), dl.clone(), "linux", key.keys(), SyncState::default());
    let mut em = Engine::new(space.clone(), dm.clone(), "mac", key.keys(), SyncState::default());
    let (tl, tm) = (FolderTransport::new(&root, &space, dl, None), FolderTransport::new(&root, &space, dm, None));
    let mut linux = quire_sync::MemoryStore::default();
    linux.write("Ol\u{e9}.md", "composed\n").unwrap();
    linux.write("Ole\u{301}.md", "decomposed\n").unwrap();
    el.sync(&mut linux, &tl, "2026-11-16").unwrap();
    let vm = dir.path().join("Mac");
    std::fs::create_dir_all(vm.join(".quire")).unwrap();
    for _ in 0..3 {
        let mut st = VaultStore::new(Vault::open_local(&vm).unwrap());
        snapshot_reads(&mut st).unwrap();
        em.sync(&mut st, &tm, "2026-11-16").unwrap();
    }
    assert_eq!(files(&vm).len(), 1, "{:?}", files(&vm));
    assert_eq!(em.state.skipped.len(), 1);
}

/// The same on a real Windows or macOS disk, and the names Windows refuses.
#[cfg(any(windows, target_os = "macos"))]
#[test]
fn this_disk_refuses_what_sync_skips() {
    let dir = tempfile::tempdir().unwrap();
    let root = folder::sync_root(&dir.path().join("Dropbox"));
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
    // Another device (Linux) with notes this one can't store.
    let (dl, dw) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
    let mut el = Engine::new(space.clone(), dl.clone(), "linux", key.keys(), SyncState::default());
    let mut ew = Engine::new(space.clone(), dw.clone(), "here", key.keys(), SyncState::default());
    let (tl, tw) = (FolderTransport::new(&root, &space, dl, None), FolderTransport::new(&root, &space, dw, None));
    let mut linux = quire_sync::MemoryStore::default();
    linux.write("Plan.md", "big plan\n").unwrap();
    linux.write("plan.md", "small plan\n").unwrap();
    linux.write("fine.md", "fine\n").unwrap();
    if cfg!(windows) {
        linux.write("Why?.md", "why\n").unwrap();
        linux.write("aux.md", "aux\n").unwrap();
    }
    el.sync(&mut linux, &tl, "2026-11-16").unwrap();
    let vw = dir.path().join("Here");
    std::fs::create_dir_all(vw.join(".quire")).unwrap();
    let mut st = VaultStore::new(Vault::open_local(&vw).unwrap());
    snapshot_reads(&mut st).unwrap();
    assert!(st.case_blind(), "this test is for a disk that ignores capitals");
    let r = ew.sync(&mut st, &tw, "2026-11-16").unwrap();
    assert_eq!(r.skipped.len(), if cfg!(windows) { 3 } else { 1 }, "{r:?}");
    let here = files(&vw);
    assert_eq!(here.len(), 2, "{here:?}");
    assert!(here.contains(&"fine.md".to_string()) && (here.contains(&"Plan.md".to_string()) || here.contains(&"plan.md".to_string())));
    std::fs::write(vw.join("fine.md"), "fine, from here\n").unwrap();
    snapshot_reads(&mut st).unwrap();
    ew.sync(&mut st, &tw, "2026-11-16").unwrap();
    el.sync(&mut linux, &tl, "2026-11-16").unwrap();
    assert_eq!(linux.read("fine.md").unwrap(), "fine, from here\n");
    assert_eq!(linux.read("plan.md").unwrap(), "small plan\n", "nothing deleted there");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

/// Data review #5: a synced drawing that grows past the sync limit is
/// held back with a notice, never sent as a deletion.
#[test]
fn a_synced_file_grown_too_big_is_held_back_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let root = folder::sync_root(&dir.path().join("Dropbox"));
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
    let (va, vb) = (dir.path().join("A"), dir.path().join("B"));
    std::fs::create_dir_all(&va).unwrap();
    std::fs::create_dir_all(&vb).unwrap();
    let (a, b) = (Vault::open_local(&va).unwrap(), Vault::open_local(&vb).unwrap());
    let (da, db) = (DeviceId::random().unwrap(), DeviceId::random().unwrap());
    let mut ea = Engine::new(space.clone(), da.clone(), "a", key.keys(), SyncState::default());
    let mut eb = Engine::new(space.clone(), db.clone(), "b", key.keys(), SyncState::default());
    let (ta, tb) = (FolderTransport::new(&root, &space, da, None), FolderTransport::new(&root, &space, db, None));
    std::fs::write(va.join("Board.excalidraw"), b"{\"type\":\"excalidraw\"}").unwrap();
    let pass = |e: &mut Engine, v: &Vault, t: &FolderTransport| {
        let mut st = VaultStore::new(v.clone());
        snapshot_reads(&mut st).unwrap();
        e.sync(&mut st, t, "2026-11-13").unwrap()
    };
    pass(&mut ea, &a, &ta);
    pass(&mut eb, &b, &tb);
    assert!(vb.join("Board.excalidraw").exists());
    std::fs::write(va.join("Board.excalidraw"), vec![b' '; usize::try_from(MAX_ATTACHMENT_BYTES).unwrap() + 1]).unwrap();
    let r = pass(&mut ea, &a, &ta);
    assert_eq!(r.held_back, ["Board.excalidraw"]);
    pass(&mut eb, &b, &tb);
    assert!(vb.join("Board.excalidraw").exists(), "the other device keeps its copy");
    assert!(va.join("Board.excalidraw").exists());
}

#[test]
fn attachments_sync_as_bytes_and_big_ones_stay_local() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_local(dir.path()).unwrap();
    std::fs::create_dir_all(dir.path().join("attachments")).unwrap();
    let png = [0x89u8, b'P', b'N', b'G', 0, 1, 2, 255];
    std::fs::write(dir.path().join("attachments/a.png"), png).unwrap();
    std::fs::write(dir.path().join("huge.mov"), vec![0u8; usize::try_from(MAX_ATTACHMENT_BYTES).unwrap() + 1]).unwrap();
    vault.write(&NotePath::new("N.md"), "![[a.png]]").unwrap();
    let mut store = VaultStore::new(vault);
    let mut listed = store.list().unwrap();
    listed.sort();
    assert_eq!(listed, ["N.md", "attachments/a.png"], "too-big files stay out");
    let text = store.read("attachments/a.png").unwrap();
    snapshot_reads(&mut store).unwrap();
    store.write("attachments/b.png", &text).unwrap();
    assert_eq!(std::fs::read(dir.path().join("attachments/b.png")).unwrap(), png);
    assert_eq!(store.digest("attachments/b.png"), Some(quire_sync::engine::fingerprint("attachments/b.png", &text)));
    // A change underneath is noticed for attachments too.
    snapshot_reads(&mut store).unwrap();
    std::fs::write(dir.path().join("attachments/a.png"), [1u8, 2, 3]).unwrap();
    assert!(store.write("attachments/a.png", &text).is_err());
}

#[test]
fn a_sync_folder_must_be_outside_the_vault_and_vice_versa() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("Notes");
    let dropbox = dir.path().join("Dropbox");
    std::fs::create_dir_all(vault.join("inner")).unwrap();
    std::fs::create_dir_all(&dropbox).unwrap();
    assert_eq!(sync_folder_root(&dropbox, &vault).unwrap(), dropbox.join("Quire Sync"));
    assert!(sync_folder_root(&vault.join("inner"), &vault).is_err());
    assert!(sync_folder_root(dir.path(), &dir.path().join("Quire Sync").join("v")).is_err());
    assert!(sync_folder_root(&dir.path().join("missing"), &vault).is_err());
    // The same folder reached through a symlink (macOS's /var → /private/var).
    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert!(sync_folder_root(dir.path(), &link.join("Quire Sync").join("v")).is_err());
        assert!(sync_folder_root(&link.join("Notes").join("inner"), &vault).is_err());
    }
}

#[test]
fn a_0_5_config_still_loads_and_folder_configs_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let old = r#"{"server":"https://s.example.com","space":"ab","device":"cd","device_name":"laptop"}"#;
    std::fs::create_dir_all(sync_dir(dir.path())).unwrap();
    std::fs::write(sync_dir(dir.path()).join("config.json"), old).unwrap();
    let cfg = SyncConfig::load(dir.path()).unwrap();
    assert_eq!((cfg.server.as_str(), cfg.folder.as_ref()), ("https://s.example.com", None));
    let cfg = SyncConfig { server: String::new(), folder: Some(dir.path().join("Dropbox/Quire Sync")), ..cfg };
    cfg.save(dir.path()).unwrap();
    assert_eq!(SyncConfig::load(dir.path()), Some(cfg));
}

#[test]
fn config_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SyncConfig {
        server: "https://sync.example.com".into(),
        folder: None,
        space: SpaceId::random().unwrap(),
        device: DeviceId::random().unwrap(),
        device_name: "laptop".into(),
        shared_state: false,
        only: Vec::new(),
        exclude: Vec::new(),
        remote: None,
    };
    cfg.save(dir.path()).unwrap();
    assert!(!std::fs::read_to_string(sync_dir(dir.path()).join("config.json")).unwrap().contains("shared_state"), "unchanged for 0.8");
    assert_eq!(SyncConfig::load(dir.path()).as_ref(), Some(&cfg));
    assert_eq!(load_state(dir.path(), &cfg), SyncState::default());
}

#[test]
fn vault_settings_sync_only_when_turned_on() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_local(dir.path()).unwrap();
    vault.write(&NotePath::new("A.md"), "a").unwrap();
    vault.storage().write_atomic(&NotePath::new(".quire/shared/vault.toml"), "format = 1\n").unwrap();
    vault.storage().write_atomic(&NotePath::new(".quire/agent/ledger/x.jsonl"), "{}\n").unwrap();
    let off = VaultStore::new(vault.clone());
    assert_eq!(off.list().unwrap(), vec!["A.md".to_string()], "never .quire/ by default");
    let mut on = VaultStore::new(vault.clone()).with_shared_state(true);
    assert_eq!(on.list().unwrap(), vec!["A.md".to_string(), ".quire/shared/vault.toml".to_string()], "only the shared folder");
    vault.storage().write_atomic(&NotePath::new(".quire/shared/srs/d1.jsonl"), "{}\n").unwrap();
    assert!(on.list().unwrap().contains(&".quire/shared/srs/d1.jsonl".to_string()), "review logs in subfolders too");
    snapshot_reads(&mut on).unwrap();
    on.write(".quire/shared/agent-policy.toml", &b64().encode("format = 1\n")).unwrap();
    assert_eq!(vault.storage().read(&NotePath::new(".quire/shared/agent-policy.toml")).unwrap(), "format = 1\n");
    assert!(on.write(".quire/shared/../x.toml", "").is_err());
    // A device that hasn't turned it on joins in as soon as another device's settings arrive.
    let mut joined = VaultStore::new(vault);
    snapshot_reads(&mut joined).unwrap();
    joined.incoming(&quire_sync::Content {
        path: ".quire/shared/srs/d1.jsonl".into(),
        deleted: false,
        text: String::new(),
        from: "x".into(),
        at: 0,
        actor: None,
    });
    assert!(joined.list().unwrap().contains(&".quire/shared/vault.toml".to_string()));
}

#[test]
fn agent_attribution_travels_with_versions() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open_local(dir.path()).unwrap();
    let store = quire_agent::AgentStore::open(
        &vault,
        quire_agent::DeviceInfo { id: "d1".into(), name: "laptop".into() },
        Arc::new(quire_agent::SystemClock),
    );
    let agent = quire_agent::Actor::agent("mcp:claude-code", "Claude Code", quire_agent::Via::McpStdio, "d1");
    store
        .submit(
            &agent,
            quire_agent::ChangeSet::one(quire_agent::Change::Create { path: NotePath::new("wiki/A.md"), text: "by agent\n".into() }),
        )
        .unwrap();
    vault.write(&NotePath::new("Mine.md"), "by me\n").unwrap();
    let mut vs = VaultStore::new(vault.clone());
    let a = vs.actor("wiki/A.md").unwrap();
    assert_eq!((a.id.as_str(), a.label.as_str(), a.kind.as_str()), ("mcp:claude-code", "Claude Code", "agent"));
    assert_eq!(vs.actor("Mine.md"), None, "the person's own edits carry nothing");
    vault.write(&NotePath::new("wiki/A.md"), "edited by me since\n").unwrap();
    assert_eq!(vs.actor("wiki/A.md"), None, "only while the file is what the agent wrote");
    // Receiving: the version is kept with the sender's actor.
    let content = quire_sync::Content {
        path: "wiki/B.md".into(),
        deleted: false,
        text: "from the other laptop\n".into(),
        from: "desktop".into(),
        at: 1,
        actor: Some(a),
    };
    snapshot_reads(&mut vs).unwrap();
    vs.incoming(&content);
    vs.write("wiki/B.md", &content.text).unwrap();
    let snap = vault.history(&NotePath::new("wiki/B.md")).remove(0);
    let prov = quire_agent::Provenance::from_json(&vault.read_provenance(&snap).unwrap()).unwrap();
    assert_eq!(
        (prov.actor.id.as_str(), prov.actor.via, prov.actor.device.as_str()),
        ("mcp:claude-code", quire_agent::Via::Sync, "desktop")
    );
}

#[test]
fn sharing_vault_settings_in_a_folder_opens_a_lane_and_keeps_0_8_in() {
    let vault_dir = tempfile::tempdir().unwrap();
    let sync_dir_root = tempfile::tempdir().unwrap();
    let mut ws = Workspace::new(crate::Settings::default());
    ws.open_vault(vault_dir.path()).unwrap();
    let root = folder::sync_root(sync_dir_root.path());
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    folder::create_space(&root, &space, &WrappedKey::wrap(&key, "passphrase", KDF).unwrap()).unwrap();
    let cfg = SyncConfig {
        server: String::new(),
        folder: Some(root.clone()),
        space: space.clone(),
        device: DeviceId::random().unwrap(),
        device_name: "x".into(),
        shared_state: false,
        only: Vec::new(),
        exclude: Vec::new(),
        remote: None,
    };
    cfg.save(vault_dir.path()).unwrap();
    ws.sync_share_vault_settings(true).unwrap();
    assert_eq!(folder::min_reader(&root, &space).unwrap(), 1, "0.8 devices keep syncing notes");
    assert!(folder::lane_exists(&root, &space, folder::SHARED_LANE));
    assert!(SyncConfig::load(vault_dir.path()).unwrap().shared_state);
    ws.sync_share_vault_settings(false).unwrap();
    assert!(!SyncConfig::load(vault_dir.path()).unwrap().shared_state);
}

/// Quire 0.8.0's `LocalStore`, as shipped (`git show
/// v0.8.0:crates/quire-app/src/vault_sync.rs`): notes and attachments
/// are listed, and any path is written. With `FolderTransport::new`
/// (its scan of `devices/` is unchanged since 0.8.0) and the engine
/// (unchanged but for two hooks whose defaults do what 0.8 did), this
/// syncs as a 0.8 device that joined the vault earlier does: it never
/// reads `space.json` again.
struct Store08 {
    vault: Vault,
}

impl LocalStore for Store08 {
    fn list(&self) -> Result<Vec<String>, String> {
        Ok(self
            .vault
            .storage()
            .list()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|p| p.is_markdown() || quire_sync::engine::is_attachment(p.as_str()))
            .map(|p| p.as_str().to_string())
            .collect())
    }

    fn read(&self, path: &str) -> Option<String> {
        use base64::Engine as _;
        if quire_sync::engine::is_attachment(path) {
            self.vault.storage().read_bytes(&NotePath::new(path)).ok().map(|b| b64().encode(b))
        } else {
            self.vault.read(&NotePath::new(path)).ok()
        }
    }

    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        use base64::Engine as _;
        if quire_sync::engine::is_attachment(path) {
            let bytes = b64().decode(text).map_err(|e| e.to_string())?;
            return self.vault.storage().write_bytes_atomic(&NotePath::new(path), &bytes).map_err(|e| e.to_string());
        }
        self.vault.write(&NotePath::new(path), text).map_err(|e| e.to_string())
    }

    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.vault.trash(&NotePath::new(path)).map_err(|e| e.to_string())
    }

    /// 0.8's engine sent a delete for every tracked file it didn't list
    /// (0.13.2 checks the file first, which alone would have stopped
    /// this one: the settings file was still there).
    fn missing(&self, _path: &str) -> bool {
        true
    }
}

/// Three devices in one sync folder: `a` and `c` run 0.9, `b` runs 0.8.
struct Fleet {
    _dir: tempfile::TempDir,
    root: PathBuf,
    space: SpaceId,
    keys: quire_sync::Keys,
    vaults: [Vault; 3],
    devices: [DeviceId; 3],
    engines: Vec<Engine>,
    lanes: Vec<Engine>,
}

impl Fleet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = folder::sync_root(&dir.path().join("Dropbox"));
        let space = SpaceId::random().unwrap();
        let key = VaultKey::generate().unwrap();
        folder::create_space(&root, &space, &WrappedKey::wrap(&key, "pw", KDF).unwrap()).unwrap();
        let vaults = ["A", "B", "C"].map(|n| {
            std::fs::create_dir_all(dir.path().join(n)).unwrap();
            Vault::open_local(dir.path().join(n)).unwrap()
        });
        let devices = [(); 3].map(|()| DeviceId::random().unwrap());
        let engine = |i: usize| Engine::new(space.clone(), devices[i].clone(), ["a", "b", "c"][i], key.keys(), SyncState::default());
        let engines = (0..3).map(engine).collect();
        let lanes = (0..3).map(engine).collect();
        Self { _dir: dir, root, space, keys: key.keys(), vaults, devices, engines, lanes }
    }

    /// One pass of device `i`: 0.9 (both lanes) or 0.8.
    fn pass(&mut self, i: usize) {
        let main = FolderTransport::new(&self.root, &self.space, self.devices[i].clone(), None);
        if i == 1 {
            let mut st = Store08 { vault: self.vaults[1].clone() };
            self.engines[1].sync(&mut st, &main, "2026-11-03").unwrap();
            return;
        }
        let mut notes = VaultStore::new(self.vaults[i].clone()).notes_only();
        snapshot_reads(&mut notes).unwrap();
        self.engines[i].sync(&mut notes, &main, "2026-11-03").unwrap();
        let lane = FolderTransport::lane(&self.root, &self.space, self.devices[i].clone(), None, folder::SHARED_LANE);
        let mut shared = VaultStore::new(self.vaults[i].clone()).shared_only();
        snapshot_reads(&mut shared).unwrap();
        self.lanes[i].sync(&mut shared, &lane, "2026-11-03").unwrap();
    }

    fn rounds(&mut self, n: usize) {
        for _ in 0..n {
            for i in 0..3 {
                self.pass(i);
            }
        }
    }

    fn shared(&self, i: usize, p: &str) -> Option<String> {
        self.vaults[i].storage().read(&NotePath::new(format!("{SHARED_PREFIX}{p}"))).ok()
    }

    /// One pass of device `i` as Quire 0.9 runs it: both lanes, no
    /// review history of its own.
    fn pass_09(&mut self, i: usize) {
        let main = FolderTransport::new(&self.root, &self.space, self.devices[i].clone(), None);
        let mut notes = VaultStore::new(self.vaults[i].clone()).notes_only();
        snapshot_reads(&mut notes).unwrap();
        self.engines[i].sync(&mut notes, &main, "2026-11-03").unwrap();
        let lane = FolderTransport::lane(&self.root, &self.space, self.devices[i].clone(), None, folder::SHARED_LANE);
        let mut shared = VaultStore::new(self.vaults[i].clone()).shared_only();
        snapshot_reads(&mut shared).unwrap();
        self.lanes[i].sync(&mut shared, &lane, "2026-11-03").unwrap();
    }

    /// One pass of device `i` as 0.10 runs it with shared state on:
    /// publish review history, both lanes, take in the others'.
    fn pass_10(&mut self, i: usize, agent: &quire_agent::AgentStore) {
        export_review(agent);
        self.pass_09(i);
        import_review(agent);
    }
}

/// 0.10: a server's agent suggests, the desktop reviews, through the
/// real engine and lanes, with a Quire 0.9 device in the same folder
/// passing the shared review history on without reading or deleting it.
#[test]
fn review_history_travels_between_devices_and_0_9_passes_it_on() {
    let mut f = Fleet::new();
    let note = "# Launch\nThe venue is booked.\nWe ship on Monday.\nBring snacks.\n";
    f.vaults[0].write(&NotePath::new("Launch.md"), note).unwrap();
    let store = |i: usize, id: &str, name: &str| {
        quire_agent::AgentStore::open(
            &f.vaults[i],
            quire_agent::DeviceInfo { id: id.into(), name: name.into() },
            Arc::new(quire_agent::SystemClock),
        )
    };
    let (server, desk) = (store(0, "d_server", "home server"), store(2, "d_desk", "laptop"));
    let rounds = |f: &mut Fleet, n: usize| {
        for _ in 0..n {
            f.pass_10(0, &server);
            f.pass_09(1);
            f.pass_10(2, &desk);
        }
    };
    rounds(&mut f, 2);
    assert_eq!(f.vaults[2].read(&NotePath::new("Launch.md")).unwrap(), note);
    // The agent on the server suggests two changes.
    let agent = quire_agent::Actor::agent("oauth:claude.ai", "Claude", quire_agent::Via::McpHttp, "d_server");
    let new_text = "# Launch\nThe venue is booked for Friday.\nWe ship on Monday.\nBring snacks and water.\n";
    let quire_agent::SubmitOutcome::Proposed { id, .. } = server
        .submit(
            &agent,
            quire_agent::ChangeSet::one(quire_agent::Change::Edit { path: NotePath::new("Launch.md"), new_text: new_text.into() }),
        )
        .unwrap()
    else {
        panic!("a suggestion")
    };
    rounds(&mut f, 2);
    let pending = desk.proposals(&quire_agent::ProposalFilter::pending());
    assert_eq!(pending.len(), 1, "on the desktop");
    assert_eq!(pending[0].from_device.as_ref().map(|d| d.1.as_str()), Some("home server"));
    // The 0.9 device holds the files as they are, and reads none of them.
    let file = format!("agent/proposals/d_server/{id}.json");
    assert_eq!(f.shared(1, &file), f.shared(0, &file));
    assert!(f.vaults[1].storage().list_under(".quire/agent/proposals").unwrap_or_default().is_empty());
    // The desktop accepts the first hunk and rejects the second.
    let path = NotePath::new("Launch.md");
    let hunks = desk.review(&id, &path, note).hunks;
    assert_eq!(hunks.len(), 2);
    let d = |h: &quire_agent::Hunk, a| quire_agent::HunkDecision {
        hunk: h.id.clone(),
        action: a,
        at: 0,
        text: None,
        other: serde_json::Map::new(),
    };
    desk.accept_in_file(&id, &path, &[d(&hunks[0], quire_agent::HunkAction::Accepted)], None).unwrap();
    let now = f.vaults[2].read(&path).unwrap();
    let left = desk.review(&id, &path, &now).hunks;
    desk.decide_checked(&id, &path, &[d(&left[0], quire_agent::HunkAction::Rejected)], &now, None).unwrap();
    rounds(&mut f, 3);
    // Resolved on the server, the text there once (it came by note sync).
    assert!(server.proposals(&quire_agent::ProposalFilter::pending()).is_empty());
    let resolved = server.archived_proposals().into_iter().find(|p| p.id == id).unwrap();
    assert_eq!(resolved.status, quire_agent::ProposalStatus::PartiallyApplied);
    for i in 0..3 {
        let t = f.vaults[i].read(&path).unwrap();
        assert_eq!(t.matches("for Friday").count(), 1, "device {i}: {t}");
        assert!(!t.contains("water"), "device {i}: {t}");
    }
    // Every device's runs, on the server too; nothing was deleted anywhere.
    assert!(server.log(&quire_agent::LedgerQuery::default()).iter().any(|e| e.op == quire_agent::Op::Accept && e.actor.device == "d_desk"));
    for i in 0..3 {
        assert!(f.shared(i, &file).is_some(), "device {i}");
        assert!(f.shared(i, &format!("agent/decisions/d_desk/{id}.json")).is_some(), "device {i}");
        assert!(f.shared(i, "agent/devices/d_server.json").is_some(), "device {i}");
    }
    assert!(f.vaults[1].storage().list_under(".quire/agent/proposals").unwrap_or_default().is_empty(), "0.9 never took it in");
    // No revision a 0.8 device could read names review history.
    let main = FolderTransport::new(&f.root, &f.space, DeviceId::random().unwrap(), None);
    for r in main.pull(0).unwrap() {
        let c = quire_sync::Content::open(&f.keys, &f.space, &r).unwrap();
        assert!(!c.path.starts_with(".quire"), "{}", c.path);
    }
}

#[test]
fn review_a_0_8_device_never_deletes_shared_vault_settings() {
    let mut f = Fleet::new();
    let a = &f.vaults[0];
    a.write(&NotePath::new("Plan.md"), "# Plan\n").unwrap();
    a.storage().write_atomic(&NotePath::new(".quire/shared/vault.toml"), "format = 1\n").unwrap();
    a.storage().write_atomic(&NotePath::new(".quire/shared/agent-policy.toml"), "format = 1\n").unwrap();
    a.storage().write_atomic(&NotePath::new(".quire/shared/srs/a.jsonl"), "{\"v\":1}\n").unwrap();
    f.rounds(3);
    // Notes reach every device, 0.8 included; settings reach only 0.9.
    for i in 0..3 {
        assert_eq!(f.vaults[i].read(&NotePath::new("Plan.md")).unwrap(), "# Plan\n", "device {i}");
    }
    assert_eq!(f.shared(2, "vault.toml").as_deref(), Some("format = 1\n"));
    assert_eq!(f.shared(2, "srs/a.jsonl").as_deref(), Some("{\"v\":1}\n"));
    assert!(f.vaults[1].storage().list_under(".quire/shared").unwrap_or_default().is_empty(), "0.8 never gets them");
    assert!(f.engines[1].state.files.values().all(|t| !t.path.starts_with(".quire")), "0.8 never tracks them");
    // More changes and passes: nothing is ever deleted.
    f.vaults[2].storage().write_atomic(&NotePath::new(".quire/shared/srs/c.jsonl"), "{\"v\":1}\n").unwrap();
    f.vaults[1].write(&NotePath::new("From 0.8.md"), "old device\n").unwrap();
    f.rounds(3);
    for i in [0, 2] {
        for p in ["vault.toml", "agent-policy.toml", "srs/a.jsonl", "srs/c.jsonl"] {
            assert!(f.shared(i, p).is_some(), "device {i} lost {p}");
        }
        assert_eq!(f.vaults[i].read(&NotePath::new("From 0.8.md")).unwrap(), "old device\n");
    }
    // No revision a 0.8 device can read names a settings file.
    let main = FolderTransport::new(&f.root, &f.space, DeviceId::random().unwrap(), None);
    for r in main.pull(0).unwrap() {
        let c = quire_sync::Content::open(&f.keys, &f.space, &r).unwrap();
        assert!(!c.path.starts_with(".quire"), "{}", c.path);
    }
    // A settings file that reached the notes lane anyway (a build from
    // before the lanes) is neither written nor deleted by 0.9 there.
    let mut notes = VaultStore::new(f.vaults[0].clone()).notes_only();
    snapshot_reads(&mut notes).unwrap();
    notes.remove(".quire/shared/vault.toml").unwrap();
    notes.write(".quire/shared/vault.toml", "x").unwrap();
    assert_eq!(f.shared(0, "vault.toml").as_deref(), Some("format = 1\n"));
}

/// The simulation above catches the review's finding: with settings in
/// the notes lane (the design before the lanes), the 0.8 device writes
/// them, misses them on its next pass, and deletes them everywhere.
#[test]
fn the_0_8_simulation_sees_the_delete_the_lanes_prevent() {
    let mut f = Fleet::new();
    f.vaults[0].storage().write_atomic(&NotePath::new(".quire/shared/vault.toml"), "format = 1\n").unwrap();
    let mixed = |f: &mut Fleet| {
        let main = FolderTransport::new(&f.root, &f.space, f.devices[0].clone(), None);
        let mut st = VaultStore::new(f.vaults[0].clone()).with_shared_state(true);
        snapshot_reads(&mut st).unwrap();
        f.engines[0].sync(&mut st, &main, "2026-11-03").unwrap();
    };
    for _ in 0..3 {
        mixed(&mut f);
        f.pass(1);
    }
    mixed(&mut f);
    assert!(f.shared(0, "vault.toml").is_none(), "deleted through the 0.8 device");
}

#[test]
fn a_stored_selection_with_an_unusable_folder_keeps_the_rest_and_says_so() {
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    // Entering one is refused.
    assert!(Selection::new(&s(&["A: Archive"]), &[]).is_err());
    // Stored: the bad entry is dropped, the good one still applies.
    let (sel, warnings) = Selection::lenient(&s(&["Work", "A: Archive"]), &s(&["Work/Old", "../x"]));
    assert_eq!((sel.only.as_slice(), sel.exclude.as_slice()), (&["Work/".to_string()][..], &["Work/Old/".to_string()][..]));
    assert!(sel.includes("Work/a.md") && !sel.includes("Work/Old/a.md") && !sel.includes("Plan.md"));
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("A: Archive"), "{warnings:?}");
    // Only unusable entries: nothing syncs (never everything), and it is said.
    let (sel, warnings) = Selection::lenient(&s(&["A: Archive"]), &[]);
    assert!(!sel.is_everything() && !sel.includes("Plan.md") && !sel.includes("Archive/a.md"));
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    // A good selection has no warnings.
    assert!(Selection::lenient(&s(&["Work"]), &[]).1.is_empty());
}
