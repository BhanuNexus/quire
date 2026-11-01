// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Neural embeddings from any OpenAI-compatible `/v1/embeddings` endpoint.
//! With Ollama and `nomic-embed-text` this is fully local.

use quire_index::semantic::Embedder;

use crate::llm::OpenAiCompatible;

pub struct EndpointEmbedder {
    client: OpenAiCompatible,
    model: String,
    label: String,
}

impl EndpointEmbedder {
    pub fn new(base_url: &str, model: &str, api_key: Option<String>) -> Self {
        Self { client: OpenAiCompatible::new(base_url, model, api_key), model: model.to_string(), label: format!("{model} (neural)") }
    }
}

impl Embedder for EndpointEmbedder {
    fn name(&self) -> &str {
        &self.label
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        self.client.embed(&self.model, &[text]).ok().and_then(|mut v| v.pop()).unwrap_or_default()
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        self.client.embed(&self.model, texts).map_err(|e| e.to_string())
    }
}
