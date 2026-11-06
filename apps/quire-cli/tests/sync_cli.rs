// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli sync` as a person runs it: join with a passphrase from the
//! environment, sync, stop `watch` with SIGTERM mid-pass, and check that no
//! file is left half-written and no secret was printed.
#![cfg(unix)]
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PASS: &str = "a long sync passphrase";

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["home", "config", "Dropbox", "A", "B"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        Self { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quire-cli"));
        c.args(args)
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("QUIRE_SECRETS_FILE", self.path("config/secrets.toml"))
            .env("QUIRE_SYNC_PASSPHRASE", PASS)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = self.cmd(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        out
    }
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                if !rel.starts_with(".quire") {
                    todo.push(p);
                }
            } else if let Ok(bytes) = std::fs::read(&p) {
                // (A file being written can vanish between listing and reading.)
                out.push((rel, bytes));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn join_watch_and_stop_with_sigterm_leaves_whole_files() {
    let env = Env::new();
    let (a, b, dropbox, key) = (env.path("A"), env.path("B"), env.path("Dropbox"), env.path("recovery.txt"));
    let a_s = a.to_str().unwrap();
    // A note set big enough that a pass takes a while.
    let body = "Lorem ipsum dolor sit amet, with words enough to make the note big.\n".repeat(400);
    for i in 0..120 {
        let folder = if i % 3 == 0 { "Work" } else { "Home" };
        std::fs::create_dir_all(a.join(folder)).unwrap();
        std::fs::write(a.join(format!("{folder}/Note {i}.md")), format!("# Note {i}\n{body}")).unwrap();
    }
    let out = env.run(&[
        "sync",
        "join",
        a_s,
        "--folder",
        dropbox.to_str().unwrap(),
        "--create",
        "--recovery-key-file",
        key.to_str().unwrap(),
        "--device",
        "laptop",
    ]);
    let recovery = std::fs::read_to_string(&key).unwrap();
    assert!(recovery.trim().len() > 20);
    assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
    let mut printed = text(&out);
    printed.push_str(&text(&env.run(&["sync", "once", a_s])));

    let b_s = b.to_str().unwrap();
    printed.push_str(&text(&env.run(&["sync", "join", b_s, "--folder", dropbox.to_str().unwrap(), "--device", "server"])));
    let status = text(&env.run(&["sync", "status", b_s]));
    assert!(
        status.contains("Syncs through folder") && status.contains("device “server”") && status.contains("Folders: everything"),
        "{status}"
    );
    printed.push_str(&status);

    let mut child = env.cmd(&["sync", "watch", b_s, "--poll", "5s"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // Stop it as soon as the first notes land: mid-pass.
    let start = Instant::now();
    while files(&b).iter().filter(|(p, _)| Path::new(p).extension().is_some_and(|x| x.eq_ignore_ascii_case("md"))).count() < 3 {
        assert!(start.elapsed() < Duration::from_mins(2), "nothing arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    let killed = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(killed.success());
    let start = Instant::now();
    let exit = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(start.elapsed() < Duration::from_mins(2), "watch didn't stop");
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().unwrap();
    let said = text(&out);
    assert!(exit.success(), "{said}");
    assert!(said.contains("SIGTERM") && said.contains("Stopped."), "{said}");
    printed.push_str(&said);

    // Every file there is whole: a full copy of A's, and no temporary files.
    let theirs: std::collections::HashMap<String, Vec<u8>> = files(&a).into_iter().collect();
    let here = files(&b);
    assert!(!here.is_empty());
    for (p, bytes) in &here {
        assert!(!p.rsplit('/').next().unwrap().starts_with(".tmp"), "a temporary file was left: {p}");
        assert_eq!(theirs.get(p), Some(bytes), "{p} is partial or wrong");
    }
    // A later pass finishes the job.
    printed.push_str(&text(&env.run(&["sync", "once", b_s])));
    assert_eq!(files(&b).len(), files(&a).len());

    // Selective: a third device holding only Work/ never deletes the rest.
    let c = env.path("C");
    std::fs::create_dir_all(&c).unwrap();
    let c_s = c.to_str().unwrap();
    printed.push_str(&text(&env.run(&["sync", "join", c_s, "--folder", dropbox.to_str().unwrap(), "--only", "Work"])));
    printed.push_str(&text(&env.run(&["sync", "once", c_s])));
    printed.push_str(&text(&env.run(&["sync", "once", c_s])));
    assert!(files(&c).iter().all(|(p, _)| p.starts_with("Work/")));
    assert_eq!(files(&c).len(), 40);
    printed.push_str(&text(&env.run(&["sync", "once", a_s])));
    assert_eq!(files(&a).len(), 120, "nothing deleted on the full device");
    let status = text(&env.run(&["sync", "status", c_s]));
    assert!(status.contains("Folders: only Work/") && status.contains("80 outside the selection"), "{status}");

    // No secret ever reached the screen.
    assert!(!printed.contains(PASS), "the passphrase was printed");
    assert!(!printed.contains(recovery.trim()), "the recovery key was printed");
    let config = std::fs::read_to_string(a.join(".quire/sync/config.json")).unwrap();
    assert!(!config.contains(recovery.trim()) && !config.contains(PASS));
}

/// Review findings 1 and 5: one process syncs a vault on a machine. With
/// `watch` running, `once` and `scope` refuse and say why; a second `watch`
/// waits and takes over when the first stops.
#[test]
fn one_sync_per_vault_and_scope_waits_for_it() {
    let env = Env::new();
    let (a, dropbox, key) = (env.path("A"), env.path("Dropbox"), env.path("recovery.txt"));
    let a_s = a.to_str().unwrap();
    std::fs::create_dir_all(a.join("Work")).unwrap();
    std::fs::write(a.join("Work/Plan.md"), "# Plan\n").unwrap();
    std::fs::write(a.join("Home.md"), "# Home\n").unwrap();
    env.run(&["sync", "join", a_s, "--folder", dropbox.to_str().unwrap(), "--create", "--recovery-key-file", key.to_str().unwrap()]);
    let busy = "Quire is already syncing this vault (the app or `quire-cli serve`)";
    let stop = |child: &mut std::process::Child| {
        assert!(Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap().success());
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < Duration::from_mins(1), "watch didn't stop");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let mut first = env.cmd(&["sync", "watch", a_s, "--poll", "5s"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let start = Instant::now();
    while !a.join(".quire/sync/last.json").exists() {
        assert!(start.elapsed() < Duration::from_mins(1), "the first watch never synced");
        std::thread::sleep(Duration::from_millis(50));
    }
    let once = env.cmd(&["sync", "once", a_s]).output().unwrap();
    assert!(!once.status.success() && text(&once).contains(busy), "{}", text(&once));
    let scope = env.cmd(&["sync", "scope", a_s, "--only", "Work"]).output().unwrap();
    assert!(!scope.status.success() && text(&scope).contains(busy) && text(&scope).contains("Stop it first"), "{}", text(&scope));
    assert!(text(&env.run(&["sync", "status", a_s])).contains("Folders: everything"), "nothing changed");

    let mut second = env.cmd(&["sync", "watch", a_s, "--poll", "5s"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    stop(&mut first);
    // It takes the lock within a few seconds and syncs.
    std::thread::sleep(Duration::from_secs(8));
    std::fs::write(a.join("Later.md"), "# Later\n").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    stop(&mut second);
    let said = text(&second.wait_with_output().unwrap());
    assert!(said.contains(&format!("Waiting: {busy}")), "{said}");
    assert!(said.contains("Synced"), "the second watch never took over: {said}");

    // Nothing syncs now: the folders can change, and it says what that means.
    let out = text(&env.run(&["sync", "scope", a_s, "--only", "Work"]));
    assert!(out.contains("This device now syncs only Work/") && out.contains("stay on this disk"), "{out}");
    let out = text(&env.run(&["sync", "scope", a_s, "--all"]));
    assert!(out.contains("now syncs everything") && out.contains("Notes now included are fetched"), "{out}");
    env.run(&["sync", "once", a_s]);
}

use std::os::unix::fs::PermissionsExt as _;
