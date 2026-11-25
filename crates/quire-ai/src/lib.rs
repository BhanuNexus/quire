// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-device AI, client side (`docs/design/0.9.md` §D). Contains no inference code:
//! models run in the separate `quire-ai-engine` process, which speaks
//! [`protocol`] over stdio.
//!
//! * [`protocol`] — JSON-lines messages (shared with the engine)
//! * [`toolcall`] — reading tool calls the way small models write them
//! * [`wav`] — WAV files for voice notes (written by the app, read by the engine)
//! * `catalog` — the compiled-in list of downloadable models (`client`)
//! * `download` — downloads, "Install from file…", the manifest (`client`)
//! * `engine` — starting and talking to the engine process (`client`)
//! * `local` — `LocalEmbedder`, `LocalChat`, `LocalOcr` (`client`)
//! * `memory` — free memory before a language model loads (`client`)

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "tests build fixtures with std directly"))]

#[cfg(feature = "client")]
pub mod catalog;
#[cfg(feature = "client")]
pub mod download;
#[cfg(feature = "client")]
pub mod engine;
#[cfg(feature = "client")]
pub mod local;
#[cfg(feature = "client")]
pub mod memory;
pub mod protocol;
pub mod toolcall;
pub mod wav;
