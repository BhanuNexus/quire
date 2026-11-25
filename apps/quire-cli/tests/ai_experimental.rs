// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-device AI is off until the pack is turned on (0.14): `quire-cli ai`
//! says so, never downloads a model this build hasn't pinned, and keeps voice
//! models behind the experimental switch.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]
#![allow(clippy::disallowed_methods, reason = "tests build fixtures with std directly")]

use std::process::{Command, Output};

fn cli(home: &std::path::Path, args: &[&str], env_on: bool) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_quire-cli"));
    c.args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("APPDATA", home)
        .env("QUIRE_MODELS_DIR", home.join("models"))
        .env_remove("QUIRE_AI_ENGINE")
        .env_remove("QUIRE_EXPERIMENTAL_ON_DEVICE");
    if env_on {
        c.env("QUIRE_EXPERIMENTAL_ON_DEVICE", "1");
    }
    c.output().expect("quire-cli runs")
}

fn settings_text(home: &std::path::Path) -> String {
    ["quire/settings.toml", "Quire/settings.toml", "dev.Quire.Quire/settings.toml"]
        .iter()
        .flat_map(|p| [home.join(".config").join(p), home.join(p)])
        .find_map(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// 0.14: the pack. Nothing runs or downloads while it is off; a model this
/// build hasn't pinned can't be downloaded and turns nothing on; voice models
/// still need the experimental switch.
#[test]
fn ai_commands_respect_the_pack_and_pins() {
    let dir = tempfile::tempdir().unwrap();
    let out = cli(dir.path(), &["ai", "engine"], false);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("on-device AI is off"));
    // Qwen 3.5 isn't pinned in this build: refused, and the pack stays off.
    let out = cli(dir.path(), &["ai", "download", "qwen3.5-4b-q4km"], false);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("isn't available in this build"), "{err}");
    assert!(!settings_text(dir.path()).contains("pack = true"), "nothing turned on");
    assert!(!dir.path().join("models").join("qwen3.5-4b-q4km").exists(), "nothing downloaded");
    let out = cli(dir.path(), &["ai", "download", "whisper-base"], false);
    assert!(String::from_utf8_lossy(&out.stderr).contains("experimental"), "voice stays experimental");
    // The list works, with the note first and no "coming in an update".
    let out = cli(dir.path(), &["ai", "models"], false);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.starts_with("Note: on-device AI is off") && !text.contains("coming"), "{text}");
    assert!(text.contains("qwen3.5-4b-q4km") && text.contains("not available in this build"), "{text}");
}

#[test]
fn the_experimental_switch_turns_them_back_on() {
    let dir = tempfile::tempdir().unwrap();
    let out = cli(dir.path(), &["ai", "models"], true);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && !text.contains("Note:"), "{text}");
    // The subcommands run as before.
    let out = cli(dir.path(), &["ai", "install"], true);
    assert!(String::from_utf8_lossy(&out.stderr).contains("give the files to install"));
}
