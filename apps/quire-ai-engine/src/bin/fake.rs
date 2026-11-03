// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-ai-engine-fake`: the engine protocol with tiny deterministic
//! models, for tests and UI checks (`quire_ai_engine::fake`). Never shipped.
//! `QUIRE_FAKE_IDLE_EXIT_MS` shortens the exit after idling with nothing loaded.

fn main() {
    if quire_ai_engine::handle_args("quire-ai-engine-fake") {
        return;
    }
    let idle = std::env::var("QUIRE_FAKE_IDLE_EXIT_MS").ok().and_then(|v| v.parse().ok()).map(std::time::Duration::from_millis);
    quire_ai_engine::serve(quire_ai_engine::fake::FakeBackend::from_env(), idle);
}
