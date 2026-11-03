// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Embeds the engine's pin for this target (design §D.3): the release
//! workflow builds `quire-ai-engine` first and writes
//! `engine.sha256.<target>` (`"<sha256> <size> <binary-sha256>"`: the `.gz`
//! asset's hash and size, and the unpacked binary's hash) next to
//! this file. Development builds have no pin; they use `QUIRE_AI_ENGINE` or
//! an engine next to the app instead of downloading one.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let file = std::path::Path::new(&dir).join(format!("engine.sha256.{target}"));
    println!("cargo:rerun-if-changed={}", file.display());
    let pin = std::fs::read_to_string(&file).unwrap_or_default();
    let pin = pin.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
    println!("cargo:rustc-env=QUIRE_AI_ENGINE_PIN={pin}");
    println!("cargo:rustc-env=QUIRE_AI_TARGET={target}");
}
