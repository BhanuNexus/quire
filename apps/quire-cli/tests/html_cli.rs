// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli html <vault> <note>` prints the note as "Copy as HTML" copies
//! it (issue #17).

use std::process::Command;

#[test]
fn html_prints_the_note_for_pasting() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("pics")).unwrap();
    std::fs::write(vault.join("pics/dot.png"), "PNG").unwrap();
    std::fs::write(vault.join("Other.md"), "# Other\n").unwrap();
    std::fs::write(
        vault.join("Plan.md"),
        "---\ngoal: 100\n---\n# Plan\n\n> [!tip] Remember\n> Pack light\n\nSee [[Other]].\n\n![[dot.png]]\n\n$x^2$\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_quire-cli"))
        .args(["html", vault.to_str().unwrap(), "Plan"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("APPDATA", dir.path())
        .output()
        .expect("quire-cli runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let html = String::from_utf8(out.stdout).unwrap();
    assert!(html.contains("<h1>Plan</h1>") || html.contains("<h1 id=\"plan\">Plan</h1>"), "{html}");
    assert!(html.contains("data-callout=\"tip\"") && html.contains("rgb(0,168,150)"), "{html}");
    assert!(html.contains("Other</span>"), "a link to a note is its text: {html}");
    assert!(html.contains("src=\"file://") && html.contains("/pics/dot.png\""), "{html}");
    assert!(!html.contains("goal: 100") && !html.contains("<html"), "a fragment, without the properties: {html}");
    if cfg!(feature = "typeset") {
        assert!(html.contains("<img class=\"math\""), "{html}");
    } else {
        assert!(html.contains("x^2"), "Quire Lite keeps the source: {html}");
    }
    // A note that isn't there.
    let out = Command::new(env!("CARGO_BIN_EXE_quire-cli")).args(["html", vault.to_str().unwrap(), "Nope"]).output().unwrap();
    assert!(!out.status.success());
}
