// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The app's engine handle talks to the real binary.

#[test]
fn the_client_handle_says_hello_to_this_binary() {
    let engine = quire_ai::engine::Engine::at(env!("CARGO_BIN_EXE_quire-ai-engine"));
    let h = engine.hello(std::time::Duration::from_secs(20)).unwrap();
    assert_eq!(h.engine_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(h.protocol, quire_ai::protocol::PROTOCOL_VERSION);
}
