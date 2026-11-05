// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Embeds the engine's pins (design §D.3).
//!
//! * `engine.sha256.<target>`: the release workflow builds `quire-ai-engine`
//!   first and writes `"<sha256> <size> <binary-sha256>"` (the `.gz` asset's
//!   hash and size, and the unpacked binary's hash) next to this file. The
//!   app downloads that asset on demand.
//! * `engine.bundled.sha256`: written by the packaging scripts when the
//!   engine ships inside the package (the full edition). It is the SHA-256
//!   of the exact file next to the app, after `lipo` and signing, which
//!   change its bytes on macOS and Windows.
//!
//! Development builds have neither; they use `QUIRE_AI_ENGINE` or an engine
//! next to the app unchecked instead of downloading one.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let read = |name: &str, fields: usize| {
        let file = dir.join(name);
        println!("cargo:rerun-if-changed={}", file.display());
        let pin = std::fs::read_to_string(&file).unwrap_or_default();
        pin.split_whitespace().take(fields).collect::<Vec<_>>().join(" ")
    };
    println!("cargo:rustc-env=QUIRE_AI_ENGINE_PIN={}", read(&format!("engine.sha256.{target}"), 3));
    println!("cargo:rustc-env=QUIRE_AI_BUNDLED_ENGINE_SHA256={}", read("engine.bundled.sha256", 1));
    println!("cargo:rustc-env=QUIRE_AI_TARGET={target}");
}
