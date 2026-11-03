// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-device AI is experimental in 0.9.0 and off by default: `quire-cli ai`
//! says so for the subcommands that need models, and runs them once the
//! experimental switch is on.

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

#[test]
fn ai_commands_that_need_models_say_on_device_ai_is_coming() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&["ai", "download", "bge-small-en-v1.5"][..], &["ai", "engine"], &["ai", "install", "x.gguf"]] {
        let out = cli(dir.path(), args, false);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} refused");
        assert!(err.contains("on-device AI is coming in an update"), "{args:?}: {err}");
        assert!(err.contains("QUIRE_EXPERIMENTAL_ON_DEVICE=1"), "{err}");
    }
    assert!(!dir.path().join("models").join("bge-small-en-v1.5").exists(), "nothing downloaded");
    // The list still works, with the note first.
    let out = cli(dir.path(), &["ai", "models"], false);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.starts_with("Note: on-device AI is coming in an update"), "{text}");
    assert!(text.contains("bge-small-en-v1.5"), "{text}");
}

#[test]
fn the_experimental_switch_turns_them_back_on() {
    let dir = tempfile::tempdir().unwrap();
    let out = cli(dir.path(), &["ai", "models"], true);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && !text.contains("coming in an update"), "{text}");
    // The subcommands run as before.
    let out = cli(dir.path(), &["ai", "install"], true);
    assert!(String::from_utf8_lossy(&out.stderr).contains("give the files to install"));
}
