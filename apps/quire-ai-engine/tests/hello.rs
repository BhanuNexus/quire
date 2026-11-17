// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The app's engine handle talks to the real binary.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

#![allow(clippy::disallowed_methods, reason = "tests build fixtures with std directly")]

#[test]
fn the_client_handle_says_hello_to_this_binary() {
    let engine = quire_ai::engine::Engine::at(env!("CARGO_BIN_EXE_quire-ai-engine"));
    let h = engine.hello(std::time::Duration::from_secs(20)).unwrap();
    assert_eq!(h.engine_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(h.protocol, quire_ai::protocol::PROTOCOL_VERSION);
}
