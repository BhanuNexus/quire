// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.13.2: the command line's entry points. A mistyped vault path is an
//! error and never makes a folder; `--help` never touches the disk; usage
//! mistakes are one line and exit with code 2.

#![allow(clippy::unwrap_used, reason = "test code")]

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
    std::fs::create_dir(&vault).unwrap();
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
    std::fs::create_dir(&vault).unwrap();
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
