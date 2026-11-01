// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Optional generative assistance behind a port.
//!
//! The default build has no network code: [`LocalModel`] answers what it can
//! (summaries) deterministically. Enable the `remote-llm` feature to get
//! [`AnthropicModel`], which talks to the Anthropic Messages API.

use std::sync::Arc;

use crossbeam_channel::{Receiver, bounded};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assist {
    Summarize,
    Rewrite {
        tone: String,
    },
    FixGrammar,
    Continue,
    Outline,
    /// Answer a question using the provided notes as context.
    Ask {
        question: String,
    },
}

impl Assist {
    pub fn label(&self) -> &str {
        match self {
            Self::Summarize => "Summarize",
            Self::Rewrite { .. } => "Rewrite",
            Self::FixGrammar => "Fix grammar",
            Self::Continue => "Continue writing",
            Self::Outline => "Outline",
            Self::Ask { .. } => "Ask",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    pub system: String,
    pub prompt: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum LlmError {
    #[error("{0} is not available offline")]
    Unsupported(String),
    #[error("not configured: {0}")]
    NotConfigured(String),
    #[error("request failed: {0}")]
    Request(String),
}

/// Strategy port for text generation.
pub trait LanguageModel: Send + Sync {
    fn name(&self) -> &str;
    fn complete(&self, task: &Assist, req: &LlmRequest) -> Result<String, LlmError>;
    /// Deterministic offline provider (no generation).
    fn is_offline(&self) -> bool {
        false
    }
}

const SYSTEM: &str = "You are a writing assistant inside a Markdown notes app. \
Reply with Markdown only, no preamble. Preserve the author's voice, [[wikilinks]] and #tags.";

/// Build the prompt for a task over `text` (the selection or note).
pub fn build_request(task: &Assist, text: &str) -> LlmRequest {
    let instruction = match task {
        Assist::Summarize => "Summarize this note in 3–5 bullet points.".to_string(),
        Assist::Rewrite { tone } => format!("Rewrite this to be {tone}. Keep the meaning."),
        Assist::FixGrammar => "Fix spelling, grammar and punctuation. Change nothing else.".to_string(),
        Assist::Continue => "Continue this text naturally for one or two paragraphs.".to_string(),
        Assist::Outline => "Turn this into a hierarchical Markdown outline.".to_string(),
        Assist::Ask { question } => format!("Using only the notes below, answer: {question}\nCite note titles as [[wikilinks]]."),
    };
    LlmRequest { system: SYSTEM.into(), prompt: format!("{instruction}\n\n<text>\n{text}\n</text>"), max_tokens: 1024 }
}

/// Offline provider: deterministic, instant, limited.
#[derive(Debug, Default, Clone)]
pub struct LocalModel;

impl LanguageModel for LocalModel {
    fn name(&self) -> &'static str {
        "Local (offline)"
    }

    fn is_offline(&self) -> bool {
        true
    }

    fn complete(&self, task: &Assist, req: &LlmRequest) -> Result<String, LlmError> {
        match task {
            Assist::Summarize => {
                let text = req.prompt.split("<text>\n").nth(1).unwrap_or("").trim_end_matches("\n</text>");
                let plain = quire_markdown::extract(text).plain;
                let s = crate::summary::summarize(&plain, 4);
                if s.is_empty() {
                    return Err(LlmError::Unsupported("Summarizing very short text".into()));
                }
                Ok(s.into_iter().map(|x| format!("- {x}")).collect::<Vec<_>>().join("\n"))
            }
            other => Err(LlmError::Unsupported(other.label().into())),
        }
    }
}

/// Run a request on a background thread; poll the receiver from the UI.
pub fn spawn(model: Arc<dyn LanguageModel>, task: Assist, req: LlmRequest) -> Receiver<Result<String, LlmError>> {
    let (tx, rx) = bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(model.complete(&task, &req));
    });
    rx
}

#[cfg(feature = "remote-llm")]
pub use remote::{AnthropicModel, OpenAiCompatible, ProviderKind, probe};

#[cfg(feature = "remote-llm")]
mod remote {
    use std::time::Duration;

    use super::{Assist, LanguageModel, LlmError, LlmRequest};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ProviderKind {
        Anthropic,
        /// Any `/v1/chat/completions` server: `OpenAI`, Ollama, LM Studio, llama.cpp, vLLM…
        OpenAiCompatible,
    }

    /// Trust the operating system's certificate store, so company proxies and
    /// self-signed local servers the user has trusted work as in a browser.
    fn system_tls() -> ureq::tls::TlsConfig {
        ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build()
    }

    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).tls_config(system_tls()).build().into()
    }

    fn err_from(status: u16, body: &serde_json::Value) -> LlmError {
        let msg = body["error"]["message"].as_str().or_else(|| body["error"].as_str()).unwrap_or("request failed");
        LlmError::Request(format!("HTTP {status}: {msg}"))
    }

    fn trim(base: &str) -> &str {
        base.trim_end_matches('/').trim_end_matches("/v1")
    }

    /// Anthropic Messages API provider.
    #[derive(Debug, Clone)]
    pub struct AnthropicModel {
        api_key: String,
        pub model: String,
        pub base_url: String,
    }

    impl AnthropicModel {
        pub fn from_env(model: impl Into<String>) -> Result<Self, LlmError> {
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| LlmError::NotConfigured("ANTHROPIC_API_KEY is not set".into()))?;
            Ok(Self::new(api_key, model))
        }

        pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
            Self { api_key: api_key.into(), model: model.into(), base_url: "https://api.anthropic.com".into() }
        }

        #[must_use]
        pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
            let u: String = url.into();
            if !u.trim().is_empty() {
                self.base_url = u;
            }
            self
        }
    }

    impl LanguageModel for AnthropicModel {
        fn name(&self) -> &str {
            &self.model
        }

        fn complete(&self, _task: &Assist, req: &LlmRequest) -> Result<String, LlmError> {
            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": req.max_tokens,
                "system": req.system,
                "messages": [{ "role": "user", "content": req.prompt }],
            });
            let mut resp = agent(Duration::from_mins(2))
                .post(format!("{}/v1/messages", trim(&self.base_url)))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .send_json(&body)
                .map_err(|e| LlmError::Request(e.to_string()))?;
            let status = resp.status().as_u16();
            let v: serde_json::Value = resp.body_mut().read_json().map_err(|e| LlmError::Request(e.to_string()))?;
            if status >= 400 {
                return Err(err_from(status, &v));
            }
            let text: String = v["content"]
                .as_array()
                .map(|blocks| blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(""))
                .unwrap_or_default();
            if text.is_empty() { Err(LlmError::Request(format!("unexpected response: {v}"))) } else { Ok(text) }
        }
    }

    /// OpenAI-compatible chat completions provider (local or cloud).
    #[derive(Debug, Clone)]
    pub struct OpenAiCompatible {
        api_key: Option<String>,
        pub model: String,
        pub base_url: String,
    }

    impl OpenAiCompatible {
        /// `base_url` like `http://localhost:11434` (Ollama) or `https://api.openai.com`.
        pub fn new(base_url: impl Into<String>, model: impl Into<String>, api_key: Option<String>) -> Self {
            Self { api_key: api_key.filter(|k| !k.is_empty()), model: model.into(), base_url: base_url.into() }
        }

        fn post(&self, path: &str, body: &serde_json::Value, timeout: Duration) -> Result<serde_json::Value, LlmError> {
            let mut req = agent(timeout).post(format!("{}/v1/{path}", trim(&self.base_url))).header("content-type", "application/json");
            if let Some(k) = &self.api_key {
                req = req.header("authorization", &format!("Bearer {k}"));
            }
            let mut resp = req.send_json(body).map_err(|e| LlmError::Request(e.to_string()))?;
            let status = resp.status().as_u16();
            let v: serde_json::Value = resp.body_mut().read_json().map_err(|e| LlmError::Request(e.to_string()))?;
            if status >= 400 { Err(err_from(status, &v)) } else { Ok(v) }
        }

        /// Embeddings for a batch of texts via `/v1/embeddings`.
        pub fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
            let v = self.post("embeddings", &serde_json::json!({ "model": model, "input": texts }), Duration::from_mins(2))?;
            let data = v["data"].as_array().ok_or_else(|| LlmError::Request(format!("unexpected response: {v}")))?;
            let mut out = vec![Vec::new(); texts.len()];
            for (i, d) in data.iter().enumerate() {
                let idx = d["index"].as_u64().map_or(i, |x| x as usize);
                let vec: Vec<f32> = d["embedding"]
                    .as_array()
                    .map(|a| a.iter().filter_map(serde_json::Value::as_f64).map(|x| x as f32).collect())
                    .unwrap_or_default();
                if let Some(slot) = out.get_mut(idx) {
                    *slot = vec;
                }
            }
            if out.iter().any(Vec::is_empty) {
                return Err(LlmError::Request("embedding response was missing vectors".into()));
            }
            Ok(out)
        }
    }

    impl LanguageModel for OpenAiCompatible {
        fn name(&self) -> &str {
            &self.model
        }

        fn complete(&self, _task: &Assist, req: &LlmRequest) -> Result<String, LlmError> {
            let body = serde_json::json!({
                "model": self.model,
                "max_tokens": req.max_tokens,
                "messages": [
                    { "role": "system", "content": req.system },
                    { "role": "user", "content": req.prompt },
                ],
            });
            let v = self.post("chat/completions", &body, Duration::from_mins(3))?;
            let text = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
            if text.is_empty() { Err(LlmError::Request(format!("unexpected response: {v}"))) } else { Ok(text) }
        }
    }

    /// Check a provider is reachable and the key works. Returns a short description.
    pub fn probe(kind: ProviderKind, base_url: &str, api_key: Option<&str>) -> Result<String, LlmError> {
        let base = trim(base_url);
        let mut req = agent(Duration::from_secs(8)).get(format!("{base}/v1/models"));
        match kind {
            ProviderKind::Anthropic => {
                req = req.header("x-api-key", api_key.unwrap_or_default()).header("anthropic-version", "2023-06-01");
            }
            ProviderKind::OpenAiCompatible => {
                if let Some(k) = api_key.filter(|k| !k.is_empty()) {
                    req = req.header("authorization", &format!("Bearer {k}"));
                }
            }
        }
        let mut resp = req.call().map_err(|e| LlmError::Request(e.to_string()))?;
        let status = resp.status().as_u16();
        let v: serde_json::Value = resp.body_mut().read_json().unwrap_or(serde_json::Value::Null);
        if status >= 400 {
            return Err(err_from(status, &v));
        }
        let ids: Vec<String> =
            v["data"].as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect()).unwrap_or_default();
        Ok(match ids.len() {
            0 => "Connected".to_string(),
            n => format!(
                "Connected · {n} model{}: {}",
                if n == 1 { "" } else { "s" },
                ids.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_model_summarizes_and_declines_the_rest() {
        let note = "# Ropes\nRopes are trees of text chunks used by editors. Editors like Zed and Helix rely on ropes for fast edits. \
                    Ropes make undo cheap because nodes are shared. Bread is unrelated to ropes entirely here. \
                    A rope keeps line indexes in each node so editors can jump to lines fast.";
        let m = LocalModel;
        let out = m.complete(&Assist::Summarize, &build_request(&Assist::Summarize, note)).unwrap();
        assert!(out.starts_with("- "));
        assert!(m.complete(&Assist::Continue, &build_request(&Assist::Continue, note)).is_err());
        let rx = spawn(Arc::new(LocalModel), Assist::Summarize, build_request(&Assist::Summarize, note));
        assert!(rx.recv().unwrap().is_ok());
    }
}

#[cfg(all(test, feature = "remote-llm"))]
#[allow(clippy::unwrap_used, reason = "test-only mock server")]
mod remote_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// A tiny OpenAI-compatible server: answers chat, embeddings and models.
    fn mock_server() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let mut s = stream.unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut len = 0;
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = h.trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let body = String::from_utf8_lossy(&body).to_string();
                tx.send(format!("{} {auth} {body}", line.trim())).unwrap();
                let reply = if line.contains("/v1/chat/completions") {
                    r#"{"choices":[{"message":{"role":"assistant","content":"- local answer"}}]}"#.to_string()
                } else if line.contains("/v1/embeddings") {
                    r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#.to_string()
                } else {
                    r#"{"data":[{"id":"llama3.2"},{"id":"nomic-embed-text"}]}"#.to_string()
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (addr, rx)
    }

    #[test]
    fn openai_compatible_chat_embeddings_and_probe() {
        let (addr, seen) = mock_server();
        let m = OpenAiCompatible::new(format!("{addr}/v1/"), "llama3.2", Some("sk-test".into()));
        let out = m.complete(&Assist::Summarize, &build_request(&Assist::Summarize, "text")).unwrap();
        assert_eq!(out, "- local answer");
        let req = seen.recv().unwrap();
        assert!(req.starts_with("POST /v1/chat/completions"), "{req}");
        assert!(req.contains("Bearer sk-test") && req.contains("\"system\""), "{req}");

        let v = m.embed("nomic-embed-text", &["a", "b"]).unwrap();
        assert_eq!(v, vec![vec![1.0, 0.0], vec![0.0, 1.0]], "re-ordered by index");
        let _ = seen.recv().unwrap();

        let p = probe(ProviderKind::OpenAiCompatible, &addr, None).unwrap();
        assert!(p.contains("2 models") && p.contains("llama3.2"), "{p}");
    }

    #[test]
    fn unreachable_server_is_an_error_not_a_hang() {
        let m = OpenAiCompatible::new("http://127.0.0.1:9", "x", None);
        assert!(m.complete(&Assist::Summarize, &build_request(&Assist::Summarize, "t")).is_err());
    }
}
