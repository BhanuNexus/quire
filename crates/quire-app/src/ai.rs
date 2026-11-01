// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wiring settings + secrets to the assistant and the semantic embedder.

use std::sync::Arc;

use crossbeam_channel::{Receiver, bounded};
use quire_intel::llm::{LanguageModel, LocalModel};

use crate::secrets;
use crate::settings::Settings;
use crate::workspace::Workspace;

pub const PROVIDERS: &[(&str, &str)] =
    &[("local", "Offline (built-in)"), ("anthropic", "Anthropic (Claude)"), ("openai", "OpenAI-compatible (OpenAI, Ollama, LM Studio…)")];

pub fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "https://api.anthropic.com",
        "openai" => "http://localhost:11434",
        _ => "",
    }
}

pub fn default_model(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-sonnet-5-5",
        "openai" => "llama3.2",
        _ => "",
    }
}

fn or_default(v: &str, d: &str) -> String {
    if v.trim().is_empty() { d.to_string() } else { v.trim().to_string() }
}

/// The base URL and model actually used for the configured provider.
pub fn resolved(settings: &Settings) -> (String, String) {
    let p = settings.ai_provider.as_str();
    (or_default(&settings.ai_base_url, default_base_url(p)), or_default(&settings.ai_model, default_model(p)))
}

pub fn embed_base_url(settings: &Settings) -> String {
    if !settings.embed_base_url.trim().is_empty() {
        return settings.embed_base_url.trim().to_string();
    }
    if settings.ai_provider == "openai" { resolved(settings).0 } else { default_base_url("openai").to_string() }
}

pub(crate) fn make_assistant(settings: &Settings) -> Arc<dyn LanguageModel> {
    #[cfg(feature = "remote-llm")]
    {
        let store = secrets::store();
        let (base, model) = resolved(settings);
        match settings.ai_provider.as_str() {
            "anthropic" => {
                if let Some(key) = secrets::api_key(store.as_ref(), "anthropic") {
                    return Arc::new(quire_intel::llm::AnthropicModel::new(key, model).with_base_url(base));
                }
            }
            "openai" => {
                let key = secrets::api_key(store.as_ref(), "openai");
                return Arc::new(quire_intel::llm::OpenAiCompatible::new(base, model, key));
            }
            _ => {}
        }
    }
    let _ = settings;
    Arc::new(LocalModel)
}

/// Where API keys are stored on this machine.
pub fn secret_store_kind() -> String {
    secrets::store().kind()
}

pub fn has_api_key(provider: &str) -> bool {
    secrets::api_key(secrets::store().as_ref(), provider).is_some()
}

impl Workspace {
    /// Re-read AI settings and keys: swap the assistant and the semantic embedder.
    pub fn apply_ai_settings(&mut self) {
        self.assistant = make_assistant(&self.settings);
        self.apply_embedder();
    }

    pub(crate) fn apply_embedder(&self) {
        #[cfg(feature = "remote-llm")]
        if self.settings.embeddings == "endpoint" {
            use quire_index::semantic::{SemanticIndex, VectorCache};
            let base = embed_base_url(&self.settings);
            let model = self.settings.embed_model.clone();
            let key = if self.settings.ai_provider == "openai" { secrets::api_key(secrets::store().as_ref(), "openai") } else { None };
            let cache_path = self.vault_root().map(|r| r.join(".quire").join("cache").join("embeddings.bin"));
            let builder: quire_index::SemanticBuilder = Arc::new(move |chunks| {
                let mut cache = match &cache_path {
                    Some(p) => VectorCache::open(p, &model),
                    None => VectorCache::in_memory(&model),
                };
                let emb = quire_intel::embed::EndpointEmbedder::new(&base, &model, key.clone());
                SemanticIndex::try_with_cache(chunks, Box::new(emb), &mut cache)
            });
            self.index.set_semantic_builder(Some(builder));
            return;
        }
        self.index.set_semantic_builder(None);
    }

    pub fn save_api_key(&mut self, provider: &str, key: &str) -> Result<(), String> {
        secrets::set_api_key(secrets::store().as_ref(), provider, key.trim())?;
        self.apply_ai_settings();
        Ok(())
    }

    /// Check the configured provider in the background.
    pub fn test_ai_connection(&self) -> Receiver<Result<String, String>> {
        let (tx, rx) = bounded(1);
        let provider = self.settings.ai_provider.clone();
        let (base, _) = resolved(&self.settings);
        let key = secrets::api_key(secrets::store().as_ref(), &provider);
        std::thread::spawn(move || {
            #[cfg(feature = "remote-llm")]
            let r = {
                use quire_intel::llm::{ProviderKind, probe};
                match provider.as_str() {
                    "anthropic" if key.is_none() => Err("Add an API key first".to_string()),
                    "anthropic" => probe(ProviderKind::Anthropic, &base, key.as_deref()).map_err(|e| e.to_string()),
                    "openai" => probe(ProviderKind::OpenAiCompatible, &base, key.as_deref()).map_err(|e| e.to_string()),
                    _ => Ok("Offline assistant — nothing to connect".to_string()),
                }
            };
            #[cfg(not(feature = "remote-llm"))]
            let r = {
                let _ = (provider, base, key);
                Err("This build has no network providers".to_string())
            };
            let _ = tx.send(r);
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "remote-llm")]
    #[test]
    fn unreachable_embedding_endpoint_falls_back_to_lsa() {
        let mut w =
            Workspace::new(Settings { embeddings: "endpoint".into(), embed_base_url: "http://127.0.0.1:9".into(), ..Default::default() });
        w.attach_vault(quire_vault::Vault::in_memory(&[("A.md", "# A\nRopes make undo cheap because subtrees are shared.")]));
        w.apply_ai_settings();
        w.index().flush_semantic();
        assert!(w.index().semantic_error().is_some());
        let sem = w.index().semantic().expect("LSA fallback built");
        assert!(sem.embedder_name().contains("latent"));
    }

    #[test]
    fn defaults_resolve_per_provider() {
        let mut s = Settings::default();
        assert_eq!(resolved(&s), (String::new(), String::new()));
        s.ai_provider = "openai".into();
        assert_eq!(resolved(&s).0, "http://localhost:11434");
        assert_eq!(embed_base_url(&s), "http://localhost:11434");
        s.ai_base_url = "http://box:1234/v1".into();
        s.ai_model = "qwen".into();
        assert_eq!(resolved(&s), ("http://box:1234/v1".into(), "qwen".into()));
        assert_eq!(embed_base_url(&s), "http://box:1234/v1");
        s.ai_provider = "anthropic".into();
        assert_eq!(embed_base_url(&s), "http://localhost:11434");
    }
}
