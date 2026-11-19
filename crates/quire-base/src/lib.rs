// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bottom of the crate graph: small helpers every Quire crate may use.
//!
//! **What belongs here:** stateless, dependency-light helpers for a job
//! that several crates do and that must be done one way: writing a file
//! atomically ([`fs`]), the header every cache file carries ([`cache`]), random bytes that fail loudly ([`rand`]), whether
//! a relative path is safe ([`path`]), percent-encoding, HTML escaping and
//! hex ([`enc`]), the recovery-key codec ([`recovery`]), the clock and
//! dates ([`time`]), and the HTTP client builder (`net`, behind the
//! `net` feature).
//!
//! **What does not:** anything that knows about notes, vaults, sync,
//! settings or the UI; anything with state (caches, threads, globals);
//! and any dependency on another workspace crate. A helper used by one
//! crate stays in that crate. A new third-party dependency here is a
//! dependency of every crate, so it needs a reason in `Cargo.toml`.
//!
//! The raw calls these helpers wrap (`std::fs::rename`, `getrandom::fill`,
//! `ureq::Agent::config_builder`…) are refused elsewhere by `clippy.toml`
//! (`disallowed-methods`); the modules here are where they are allowed.

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "tests build fixtures with std directly"))]

pub mod cache;
pub mod enc;
pub mod fs;
#[cfg(feature = "net")]
pub mod net;
pub mod path;
pub mod rand;
pub mod recovery;
pub mod time;
