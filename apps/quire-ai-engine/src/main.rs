// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-ai-engine`: runs on-device models for Quire in a separate process
//! (design 0.9 §D.3). It reads JSON-lines requests on stdin and answers on
//! stdout ([`quire_ai::protocol`]): candle for the language model and
//! embeddings, ocrs for OCR. A model that fails or runs out of memory takes
//! this process down, never the app.

fn main() {
    if quire_ai_engine::handle_args("quire-ai-engine") {
        return;
    }
    #[cfg(feature = "models")]
    quire_ai_engine::serve(quire_ai_engine::models::RealBackend::new(), None);
    #[cfg(not(feature = "models"))]
    quire_ai_engine::serve(quire_ai_engine::fake::NoModels, None);
}
