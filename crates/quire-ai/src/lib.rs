// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-device AI, client side (design 0.9 §D). Contains no inference code:
//! models run in the separate `quire-ai-engine` process, which speaks
//! [`protocol`] over stdio.
//!
//! * [`protocol`] — JSON-lines messages (shared with the engine)
//! * `catalog` — the compiled-in list of downloadable models (`client`)
//! * `download` — downloads, "Install from file…", the manifest (`client`)
//! * `engine` — starting and talking to the engine process (`client`)
//! * `local` — `LocalEmbedder`, `LocalChat`, `LocalOcr` (`client`)

#[cfg(feature = "client")]
pub mod catalog;
#[cfg(feature = "client")]
pub mod download;
#[cfg(feature = "client")]
pub mod engine;
#[cfg(feature = "client")]
pub mod local;
pub mod protocol;
