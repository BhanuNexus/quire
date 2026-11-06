// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli lock | unlock | status` end to end (0.11), and a revoked MCP
//! client refused at start.

use std::path::Path;
use std::process::{Command, Output};

fn cli(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_quire-cli"));
    c.args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("APPDATA", home)
        .env("QUIRE_SECRETS_FILE", home.join("secrets.toml"))
        .env_remove("QUIRE_LOCK_PASSPHRASE")
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("quire-cli runs")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// Every file under `dir`, read as bytes, holding `marker`.
fn holding(dir: &Path, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if std::fs::read(&p).is_ok_and(|b| b.windows(marker.len()).any(|w| w == marker.as_bytes())) {
                out.push(p.display().to_string());
            }
        }
    }
    out
}

#[test]
fn lock_status_unlock_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let (home, vault) = (dir.path().join("home"), dir.path().join("vault"));
    std::fs::create_dir_all(vault.join("Daily")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(vault.join("Daily/Day.md"), "# Day\nCLI-MARKER-55 diary\n").unwrap();
    std::fs::write(vault.join("Plan.md"), "# Plan\n").unwrap();
    let v = vault.to_str().unwrap();
    let pass = [("QUIRE_LOCK_PASSPHRASE", "correct horse battery")];
    // A new folder needs somewhere to put its recovery key.
    let o = cli(&home, &["lock", v, "Daily"], &pass);
    assert!(!o.status.success() && text(&o).contains("--recovery-key-file"), "{}", text(&o));
    let rk = dir.path().join("daily-key.txt");
    let o = cli(&home, &["lock", v, "Daily", "--recovery-key-file", rk.to_str().unwrap()], &pass);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("Daily/ is locked"), "{}", text(&o));
    let key = std::fs::read_to_string(&rk).unwrap();
    assert_eq!(key.trim().len(), 69);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&rk).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(!text(&o).contains(key.trim()), "the recovery key is never printed");
    assert!(holding(&vault, "CLI-MARKER-55").is_empty(), "no plain text in the vault");
    let o = cli(&home, &["status", v], &[]);
    assert!(text(&o).contains("Daily/  1 note, 0 other files"), "{}", text(&o));
    // Unlock: wrong passphrase, right one, print one note.
    let o = cli(&home, &["unlock", v, "Daily", "--check"], &[("QUIRE_LOCK_PASSPHRASE", "wrong horse battery")]);
    assert!(!o.status.success() && text(&o).contains("Wrong passphrase"), "{}", text(&o));
    let o = cli(&home, &["unlock", v, "Daily", "--check"], &pass);
    assert!(o.status.success() && text(&o).contains("opens Daily/ (1 notes)"), "{}", text(&o));
    let o = cli(&home, &["unlock", v, "Daily", "--cat", "Day"], &pass);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "# Day\nCLI-MARKER-55 diary\n");
    // Quick capture can't write today's note into it (the CLI holds no key).
    let o = cli(&home, &["capture", v, "a thought"], &[]);
    assert!(!o.status.success() && text(&o).contains("Daily/ is locked"), "{}", text(&o));
    assert_eq!(std::fs::read_dir(vault.join("Daily")).unwrap().count(), 2, "Day.md.qenc and the key file only");
    // A new passphrase, with the recovery key.
    let o = cli(
        &home,
        &["lock", v, "Daily", "--change-passphrase", "--recovery-key", "--recovery-key-file", rk.to_str().unwrap()],
        &[("QUIRE_LOCK_NEW_PASSPHRASE", "a brand new passphrase")],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert!(!cli(&home, &["unlock", v, "Daily", "--check"], &pass).status.success());
    let new = [("QUIRE_LOCK_PASSPHRASE", "a brand new passphrase")];
    // Back to a plain folder.
    let o = cli(&home, &["unlock", v, "Daily", "--convert"], &new);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(std::fs::read_to_string(vault.join("Daily/Day.md")).unwrap(), "# Day\nCLI-MARKER-55 diary\n");
    assert!(text(&cli(&home, &["status", v], &[])).starts_with("No locked folders"));
}

#[test]
fn a_revoked_mcp_client_is_refused_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let (home, vault) = (dir.path().join("home"), dir.path().join("vault"));
    std::fs::create_dir_all(vault.join(".quire/shared")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(vault.join(".quire/shared/agent-policy.toml"), "format = 1\n[clients.\"mcp:claude-code\"]\nrevoked = true\n").unwrap();
    let v = vault.to_str().unwrap();
    let o = cli(&home, &["mcp", v, "--client", "claude-code"], &[]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("Access for mcp:claude-code was revoked in Quire"), "{}", text(&o));
    // Another client starts (and ends at once: stdin is closed).
    let o = cli(&home, &["mcp", v, "--client", "other"], &[]);
    assert!(o.status.success(), "{}", text(&o));
}
