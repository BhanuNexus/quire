// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13.2: the command line's entry points. A mistyped vault path is an
//! error and never makes a folder; `--help` never touches the disk; usage
//! mistakes are one line and exit with code 2.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::path::Path;
use std::process::{Command, Output};

fn cli(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quire-cli"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", cwd.join(".config"))
        .env("APPDATA", cwd)
        .output()
        .expect("quire-cli runs")
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn a_mistyped_vault_path_is_an_error_and_makes_no_folder() {
    let dir = tempfile::tempdir().unwrap();
    let typo = dir.path().join("Nots");
    let t = typo.to_str().unwrap();
    for args in [
        vec!["search", t, "word"],
        vec!["index", t],
        vec!["tasks", t],
        vec!["lint", t],
        vec!["mcp", t],
        vec!["serve", t],
        vec!["sync", "status", t],
        vec!["capture", t, "a line"],
        vec!["import", "--undo", "last", t],
    ] {
        let out = cli(dir.path(), &args);
        assert!(!out.status.success(), "{args:?} must fail");
        let e = err(&out);
        assert!(e.contains(&format!("No folder at {t}")), "{args:?}: {e}");
        assert!(e.contains("Check the path"), "{args:?}: says what to do: {e}");
    }
    assert_eq!(entries(dir.path()), Vec::<String>::new(), "nothing was created");
}

#[test]
fn a_file_is_not_a_vault_either() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "x").unwrap();
    let out = cli(dir.path(), &["search", dir.path().join("a.md").to_str().unwrap(), "x"]);
    assert!(!out.status.success());
    assert!(err(&out).contains("is a file, not a folder"), "{}", err(&out));
}

#[test]
fn an_existing_folder_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("A.md"), "# A\n\nhello\n").unwrap();
    let out = cli(dir.path(), &["search", vault.to_str().unwrap(), "hello"]);
    assert!(out.status.success(), "{}", err(&out));
}

#[test]
fn help_works_for_every_command_and_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["search", "sync", "capture", "mcp", "tasks", "lint", "serve", "import"] {
        for flag in ["--help", "-h"] {
            let out = cli(dir.path(), &[name, flag]);
            assert!(out.status.success(), "{name} {flag}: {}", err(&out));
            assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("quire-cli {name}")), "{name} {flag}");
        }
    }
    let out = cli(dir.path(), &["search", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(help.contains("\"phrase\"") && !help.contains('\\'), "no stray backslashes: {help}");
    assert!(help.lines().count() < 6, "only this command's help: {help}");
    let out = cli(dir.path(), &["help", "capture"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("(\"-\" reads stdin)"));
    assert_eq!(entries(dir.path()), Vec::<String>::new(), "no folder named --help, nothing else");
}

#[test]
fn usage_mistakes_are_one_line_with_exit_code_2() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let out = cli(dir.path(), &["searc", "x"]);
    assert_eq!(out.status.code(), Some(2));
    let e = err(&out);
    assert_eq!(e.lines().count(), 1, "{e}");
    assert!(e.contains("unknown command “searc”") && e.contains("Did you mean “search”?") && e.contains("--help"), "{e}");

    let out = cli(dir.path(), &["search", vault.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let e = err(&out);
    assert_eq!(e.lines().count(), 1, "{e}");
    assert!(e.contains("missing <query>") && e.contains("quire-cli search --help"), "{e}");

    // A failure that is not a usage mistake is code 1.
    let out = cli(dir.path(), &["search", dir.path().join("nope").to_str().unwrap(), "x"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn help_anywhere_before_a_double_dash_prints_help_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let v = vault.to_str().unwrap();
    let before = entries(&vault);
    for args in [
        vec!["capture", v, "note text", "--help"],
        vec!["serve", v, "--help"],
        vec!["serve", v, "-h"],
        vec!["sync", "once", v, "--help"],
        vec!["remind", v, "--install", "-h"],
        vec!["lock", v, "Journal", "--help"],
        vec!["search", v, "q", "-h"],
    ] {
        let out = cli(dir.path(), &args);
        assert!(out.status.success(), "{args:?}: {}", err(&out));
        assert!(String::from_utf8_lossy(&out.stdout).contains("quire-cli"), "{args:?}: prints help");
    }
    assert_eq!(entries(&vault), before, "nothing was written or made");
    // After `--` the words are text.
    let out = cli(dir.path(), &["capture", v, "--", "-h"]);
    assert!(out.status.success(), "{}", err(&out));
    let daily: Vec<String> = entries(&vault);
    assert!(daily.iter().any(|n| n != ".quire"), "a note was written: {daily:?}");
    // A folder named --help, -h or help is not a vault for serve.
    for name in ["--help", "-h", "help"] {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
        let out = cli(dir.path(), &["serve", name]);
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("quire-cli serve"), "serve {name}");
    }
}

#[test]
fn every_usage_error_is_one_line_with_exit_code_2() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let v = vault.to_str().unwrap();
    for args in [vec!["sync", "bogus", v], vec!["agent", "bogus", v], vec!["format"], vec!["import"], vec!["tags", v, "bogus"]] {
        let out = cli(dir.path(), &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", err(&out));
        assert_eq!(err(&out).lines().count(), 1, "{args:?}: {}", err(&out));
    }
}

#[test]
fn import_makes_its_target_folder_but_only_when_writing() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("A.md"), "# A\n").unwrap();
    let target = dir.path().join("new-vault");
    let (s, t) = (source.to_str().unwrap(), target.to_str().unwrap());
    let out = cli(dir.path(), &["import", s, t]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(!target.exists(), "a dry run makes nothing");
    let out = cli(dir.path(), &["import", s, t, "--write"]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(target.join("A.md").exists(), "imported into a folder it made");
    // Not two levels: a missing parent is an unplugged drive or a typo.
    let deep = dir.path().join("media").join("Notes");
    let out = cli(dir.path(), &["import", s, deep.to_str().unwrap(), "--write"]);
    assert!(!out.status.success());
    assert!(err(&out).contains("No folder at"), "{}", err(&out));
    assert!(!dir.path().join("media").exists());
}

fn vault_with(note: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), note).unwrap();
    dir
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn any_file_has(dir: &Path, text: &str) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .any(|p| if p.is_dir() { any_file_has(&p, text) } else { std::fs::read_to_string(&p).is_ok_and(|s| s.contains(text)) })
}

/// 0.13.2 (F-L4): after `--` every word is text, for every command.
#[test]
fn after_a_double_dash_replace_takes_flag_words_as_text() {
    let v = vault_with("keep --write and more\n");
    let t = v.path().to_str().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let o = cli(cwd.path(), &["replace", t, "--", "--write", "done"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(stdout(&o).contains("1 matches (dry run"), "{}", stdout(&o));
    assert_eq!(std::fs::read_to_string(v.path().join("a.md")).unwrap(), "keep --write and more\n", "a dry run writes nothing");
    // The real flag, before the `--`, still writes.
    let o = cli(cwd.path(), &["replace", t, "--write", "--", "--write", "done"]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(std::fs::read_to_string(v.path().join("a.md")).unwrap(), "keep done and more\n");
}

#[test]
fn after_a_double_dash_capture_takes_flag_words_as_text() {
    let v = vault_with("x\n");
    let t = v.path().to_str().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let o = cli(cwd.path(), &["capture", t, "--", "--url", "x"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(any_file_has(v.path(), "--url x"), "the words were captured as text");
    assert!(!stdout(&o).contains("saved") && !stdout(&o).contains("suggested"), "no web capture ran: {}", stdout(&o));
}

#[test]
fn after_a_double_dash_search_and_subcommands_take_flag_words_as_text() {
    let v = vault_with("a note about --help\n");
    let t = v.path().to_str().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let o = cli(cwd.path(), &["search", t, "--", "--help"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!stdout(&o).contains("quire-cli search"), "searched; didn't print help: {}", stdout(&o));
    // A command with subcommands: `--write` is the tag's name, not the flag.
    let o = cli(cwd.path(), &["tags", t, "delete", "--", "--write"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(!stdout(&o).contains("Usage") && !err(&o).contains("usage"), "{}{}", stdout(&o), err(&o));
    assert_eq!(std::fs::read_to_string(v.path().join("a.md")).unwrap(), "a note about --help\n");
}
