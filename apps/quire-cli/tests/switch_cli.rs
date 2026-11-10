// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli format` and `quire-cli import --undo` (0.12 switching tools).

use std::path::Path;
use std::process::{Command, Output};

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quire-cli"))
        .args(args)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join(".config"))
        .env("APPDATA", dir)
        .output()
        .expect("quire-cli runs")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn format_shows_a_diff_first_and_writes_only_with_write() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let messy = "Title\n=====\n\n* one   \n* two\n\n\n\n```\n*   keep   \n```\n";
    std::fs::write(vault.join("Messy.md"), messy).unwrap();
    std::fs::write(vault.join("Tidy.md"), "# Tidy\n\n- a\n").unwrap();
    let v = vault.to_str().unwrap();
    let out = cli(dir.path(), &["format", v]);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("-Title") && t.contains("+# Title"), "{t}");
    assert!(t.contains("1 would change") && t.contains("Dry run"), "{t}");
    assert_eq!(std::fs::read_to_string(vault.join("Messy.md")).unwrap(), messy, "a dry run writes nothing");
    let out = cli(dir.path(), &["format", v, "--write"]);
    assert!(out.status.success(), "{}", text(&out));
    let after = std::fs::read_to_string(vault.join("Messy.md")).unwrap();
    assert!(after.starts_with("# Title\n\n- one\n- two\n"), "{after}");
    assert!(after.contains("```\n*   keep   \n```"), "code is untouched: {after}");
    // Tidy already: a second run has nothing to do.
    let again = text(&cli(dir.path(), &["format", v]));
    assert!(again.contains("0 would change"), "{again}");
}

#[test]
fn an_import_can_be_listed_and_undone() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    std::fs::write(vault.join("Mine.md"), "# Mine\n").unwrap();
    let src = dir.path().join("export");
    std::fs::create_dir_all(src.join("pages")).unwrap();
    std::fs::write(src.join("pages/Idea.md"), "# Idea\n\nSome text #tag\n").unwrap();
    let (v, s) = (vault.to_str().unwrap(), src.to_str().unwrap());
    let dry = cli(dir.path(), &["import", s, v, "--from", "markdown"]);
    assert!(dry.status.success(), "{}", text(&dry));
    assert!(!vault.join("Imported").exists(), "a dry run writes nothing");
    let done = cli(dir.path(), &["import", s, v, "--from", "markdown", "--into", "Imported", "--write"]);
    assert!(done.status.success(), "{}", text(&done));
    let imported: Vec<_> = walk(&vault.join("Imported"));
    assert!(imported.iter().any(|p| p.ends_with("Idea.md")), "{imported:?}");
    let list = text(&cli(dir.path(), &["import", "--list", v]));
    assert!(list.contains("Imported"), "{list}");
    let undo = cli(dir.path(), &["import", "--undo", "last", v]);
    assert!(undo.status.success(), "{}", text(&undo));
    assert!(walk(&vault.join("Imported")).iter().all(|p| !p.ends_with("Idea.md")), "undone");
    assert!(vault.join("Mine.md").exists(), "notes that were already there are never touched");
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
