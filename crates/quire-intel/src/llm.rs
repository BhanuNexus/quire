// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Optional generative assistance behind ports.
//!
//! The default build has no network code: [`LocalModel`] answers what it can
//! (summaries) deterministically. Enable the `remote-llm` feature to get
//! [`AnthropicModel`] (the Messages API) and [`OpenAiCompatible`] (any
//! `/v1/chat/completions` server). Both are [`ChatModel`](crate::chat::ChatModel)s
//! with tools and streaming (design §D.4), and [`LanguageModel`]s for the
//! one-shot assist features, built on the same streaming call.

use std::sync::Arc;

use crossbeam_channel::{Receiver, bounded};

pub mod sse;

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
    #[error("stopped")]
    Cancelled,
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
    use std::collections::BTreeMap;
    use std::io::BufReader;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::sse::{SseEvent, SseReader};
    use super::{Assist, LanguageModel, LlmError, LlmRequest};
    use crate::chat::{CancelFlag, ChatEvent, ChatMessage, ChatModel, ChatRequest, Locality, Part, Role, StopReason, Usage, chat_text};

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

    /// For streamed replies: no overall limit (a long answer is fine), but a
    /// server that goes quiet for a minute is given up on.
    fn stream_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_mins(3)))
            .timeout_per_read(Some(Duration::from_mins(1)))
            .http_status_as_error(false)
            .tls_config(system_tls())
            .build()
            .into()
    }

    fn err_from(status: u16, body: &Value) -> LlmError {
        let msg = body["error"]["message"].as_str().or_else(|| body["error"].as_str()).unwrap_or("request failed");
        LlmError::Request(format!("HTTP {status}: {msg}"))
    }

    fn trim(base: &str) -> &str {
        base.trim_end_matches('/').trim_end_matches("/v1")
    }

    /// Send a request and stream its reply: SSE events to `on_event`, or
    /// (for servers that ignore `stream: true`) the whole JSON body to `on_json`.
    /// One piece of a reply: an SSE event, or a whole JSON body.
    enum Body<'a> {
        Event(&'a SseEvent),
        Json(&'a Value),
    }

    fn stream(
        req: ureq::RequestBuilder<ureq::typestate::WithBody>,
        body: &Value,
        cancel: &CancelFlag,
        on: &mut dyn FnMut(Body<'_>) -> Result<bool, LlmError>,
    ) -> Result<(), LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let resp = req.send_json(body).map_err(|e| LlmError::Request(e.to_string()))?;
        let status = resp.status().as_u16();
        let sse = resp.body().mime_type().is_some_and(|m| m.eq_ignore_ascii_case("text/event-stream"));
        let reader = resp.into_body().into_with_config().limit(64 * 1024 * 1024).reader();
        if status >= 400 || !sse {
            let text = std::io::read_to_string(reader).map_err(|e| LlmError::Request(e.to_string()))?;
            let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "error": { "message": text.trim() } }));
            if status >= 400 {
                return Err(err_from(status, &v));
            }
            return on(Body::Json(&v)).map(|_| ());
        }
        let mut events = SseReader::new(BufReader::new(reader));
        loop {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            match events.next_event() {
                Ok(Some(ev)) => {
                    if !on(Body::Event(&ev))? {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(LlmError::Request(format!("the reply stopped: {e}"))),
            }
        }
    }

    /// Partial tool calls, assembled from streamed pieces.
    #[derive(Default)]
    struct ToolAcc {
        id: String,
        name: String,
        json: String,
    }

    impl ToolAcc {
        fn finish(self) -> ChatEvent {
            let input = if self.json.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&self.json).unwrap_or_else(|_| json!({ "_unparsed": self.json }))
            };
            ChatEvent::ToolUse { id: self.id, name: self.name, input }
        }
    }

    // ------------------------------------------------------------------ Anthropic

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

        /// The Messages API body for `req` (streamed).
        pub fn body(&self, req: &ChatRequest) -> Value {
            let mut system = req.system.clone();
            if req.json {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str("Reply with one JSON value only: no prose, no code fence.");
            }
            let mut body = json!({
                "model": self.model,
                "max_tokens": req.max_tokens.max(1),
                "messages": anthropic_messages(&req.messages),
                "temperature": f64::from(req.temperature),
                "stream": true,
            });
            if !system.is_empty() {
                body["system"] = system.into();
            }
            if !req.tools.is_empty() {
                body["tools"] = req
                    .tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
                    .collect::<Vec<_>>()
                    .into();
            }
            body
        }
    }

    /// Messages in the Messages API shape: system messages go to `system`
    /// (the caller's), tool results are user turns, and turns of the same
    /// role are merged (the API wants them alternating).
    fn anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
        let mut out: Vec<(String, Vec<Value>)> = Vec::new();
        for m in messages {
            let role = match m.role {
                Role::System => continue,
                Role::User | Role::Tool => "user",
                Role::Assistant => "assistant",
            };
            let blocks: Vec<Value> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text } if text.is_empty() => None,
                    Part::Text { text } => Some(json!({ "type": "text", "text": text })),
                    Part::ToolUse { id, name, input } => Some(json!({ "type": "tool_use", "id": id, "name": name, "input": input })),
                    Part::ToolResult { id, content, is_error } => {
                        Some(json!({ "type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error }))
                    }
                })
                .collect();
            if blocks.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some((r, b)) if r == role => b.extend(blocks),
                _ => out.push((role.to_string(), blocks)),
            }
        }
        out.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect()
    }

    fn anthropic_stop(s: &str) -> StopReason {
        match s {
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        }
    }

    fn u32_of(v: &Value) -> u32 {
        v.as_u64().map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
    }

    impl ChatModel for AnthropicModel {
        fn name(&self) -> &str {
            &self.model
        }

        fn locality(&self) -> Locality {
            Locality::Cloud
        }

        fn supports_tools(&self) -> bool {
            true
        }

        fn chat(&self, req: &ChatRequest, cancel: &CancelFlag, sink: &mut dyn FnMut(ChatEvent)) -> Result<(), LlmError> {
            let http = stream_agent()
                .post(format!("{}/v1/messages", trim(&self.base_url)))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .header("accept", "text/event-stream");
            let mut usage = Usage::default();
            let mut stop = StopReason::EndTurn;
            let mut tools: BTreeMap<u64, ToolAcc> = BTreeMap::new();
            let mut done = false;
            stream(http, &self.body(req), cancel, &mut |piece| match piece {
                Body::Event(ev) => {
                    let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return Ok(true) };
                    let kind = v["type"].as_str().unwrap_or(ev.event.as_str());
                    match kind {
                        "message_start" => {
                            usage.input_tokens = u32_of(&v["message"]["usage"]["input_tokens"]);
                            usage.output_tokens = u32_of(&v["message"]["usage"]["output_tokens"]);
                        }
                        "content_block_start" => {
                            let b = &v["content_block"];
                            if b["type"] == "tool_use" {
                                let idx = v["index"].as_u64().unwrap_or(0);
                                tools.insert(
                                    idx,
                                    ToolAcc {
                                        id: b["id"].as_str().unwrap_or_default().into(),
                                        name: b["name"].as_str().unwrap_or_default().into(),
                                        json: String::new(),
                                    },
                                );
                            } else if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                                sink(ChatEvent::Text { text: t.into() });
                            }
                        }
                        "content_block_delta" => {
                            let d = &v["delta"];
                            match d["type"].as_str() {
                                Some("text_delta") => {
                                    if let Some(t) = d["text"].as_str() {
                                        sink(ChatEvent::Text { text: t.into() });
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(acc) = tools.get_mut(&v["index"].as_u64().unwrap_or(0)) {
                                        acc.json.push_str(d["partial_json"].as_str().unwrap_or_default());
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            if let Some(acc) = tools.remove(&v["index"].as_u64().unwrap_or(0)) {
                                sink(acc.finish());
                            }
                        }
                        "message_delta" => {
                            if let Some(s) = v["delta"]["stop_reason"].as_str() {
                                stop = anthropic_stop(s);
                            }
                            if v["usage"]["output_tokens"].is_u64() {
                                usage.output_tokens = u32_of(&v["usage"]["output_tokens"]);
                            }
                            if v["usage"]["input_tokens"].is_u64() {
                                usage.input_tokens = u32_of(&v["usage"]["input_tokens"]);
                            }
                        }
                        "message_stop" => {
                            done = true;
                            return Ok(false);
                        }
                        "error" => return Err(err_from(200, &v)),
                        _ => {}
                    }
                    Ok(true)
                }
                Body::Json(v) => {
                    // A server that answered without streaming.
                    for b in v["content"].as_array().into_iter().flatten() {
                        match b["type"].as_str() {
                            Some("text") => sink(ChatEvent::Text { text: b["text"].as_str().unwrap_or_default().into() }),
                            Some("tool_use") => sink(ChatEvent::ToolUse {
                                id: b["id"].as_str().unwrap_or_default().into(),
                                name: b["name"].as_str().unwrap_or_default().into(),
                                input: b["input"].clone(),
                            }),
                            _ => {}
                        }
                    }
                    usage.input_tokens = u32_of(&v["usage"]["input_tokens"]);
                    usage.output_tokens = u32_of(&v["usage"]["output_tokens"]);
                    stop = anthropic_stop(v["stop_reason"].as_str().unwrap_or_default());
                    done = true;
                    Ok(false)
                }
            })?;
            if !done {
                return Err(LlmError::Request("the reply ended early".into()));
            }
            for (_, acc) in std::mem::take(&mut tools) {
                sink(acc.finish());
            }
            sink(ChatEvent::Done { stop, usage });
            Ok(())
        }
    }

    impl LanguageModel for AnthropicModel {
        fn name(&self) -> &str {
            &self.model
        }

        fn complete(&self, _task: &Assist, req: &LlmRequest) -> Result<String, LlmError> {
            complete_via_chat(self, req)
        }
    }

    /// One-shot completion over the streaming chat call.
    fn complete_via_chat(model: &dyn ChatModel, req: &LlmRequest) -> Result<String, LlmError> {
        let (text, _) = chat_text(model, &ChatRequest::simple(req.system.clone(), req.prompt.clone(), req.max_tokens), &CancelFlag::new())?;
        if text.trim().is_empty() { Err(LlmError::Request("the model sent an empty reply".into())) } else { Ok(text) }
    }

    // ------------------------------------------------------------------ OpenAI-compatible

    /// OpenAI-compatible chat completions provider (local or cloud).
    #[derive(Debug, Clone)]
    pub struct OpenAiCompatible {
        api_key: Option<String>,
        pub model: String,
        pub base_url: String,
        /// Where it runs, for the UI ("this computer" for localhost servers).
        locality: Locality,
    }

    impl OpenAiCompatible {
        /// `base_url` like `http://localhost:11434` (Ollama) or `https://api.openai.com`.
        pub fn new(base_url: impl Into<String>, model: impl Into<String>, api_key: Option<String>) -> Self {
            let base_url: String = base_url.into();
            let locality = if is_loopback_url(&base_url) { Locality::LocalServer } else { Locality::Cloud };
            Self { api_key: api_key.filter(|k| !k.is_empty()), model: model.into(), base_url, locality }
        }

        fn request(&self, path: &str, agent: &ureq::Agent) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
            let mut req = agent.post(format!("{}/v1/{path}", trim(&self.base_url))).header("content-type", "application/json");
            if let Some(k) = &self.api_key {
                req = req.header("authorization", &format!("Bearer {k}"));
            }
            req
        }

        fn post(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value, LlmError> {
            let mut resp = self.request(path, &agent(timeout)).send_json(body).map_err(|e| LlmError::Request(e.to_string()))?;
            let status = resp.status().as_u16();
            let v: Value = resp.body_mut().read_json().map_err(|e| LlmError::Request(e.to_string()))?;
            if status >= 400 { Err(err_from(status, &v)) } else { Ok(v) }
        }

        /// Embeddings for a batch of texts via `/v1/embeddings`.
        pub fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
            let v = self.post("embeddings", &json!({ "model": model, "input": texts }), Duration::from_mins(2))?;
            let data = v["data"].as_array().ok_or_else(|| LlmError::Request(format!("unexpected response: {v}")))?;
            let mut out = vec![Vec::new(); texts.len()];
            for (i, d) in data.iter().enumerate() {
                let idx = d["index"].as_u64().map_or(i, |x| x as usize);
                let vec: Vec<f32> =
                    d["embedding"].as_array().map(|a| a.iter().filter_map(Value::as_f64).map(|x| x as f32).collect()).unwrap_or_default();
                if let Some(slot) = out.get_mut(idx) {
                    *slot = vec;
                }
            }
            if out.iter().any(Vec::is_empty) {
                return Err(LlmError::Request("embedding response was missing vectors".into()));
            }
            Ok(out)
        }

        /// The chat completions body for `req` (streamed).
        pub fn body(&self, req: &ChatRequest) -> Value {
            let mut messages = Vec::new();
            if !req.system.is_empty() {
                messages.push(json!({ "role": "system", "content": req.system }));
            }
            messages.extend(openai_messages(&req.messages));
            let mut body = json!({
                "model": self.model,
                "max_tokens": req.max_tokens.max(1),
                "temperature": f64::from(req.temperature),
                "messages": messages,
                "stream": true,
                "stream_options": { "include_usage": true },
            });
            if req.json {
                body["response_format"] = json!({ "type": "json_object" });
            }
            if !req.tools.is_empty() {
                body["tools"] = req
                    .tools
                    .iter()
                    .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } }))
                    .collect::<Vec<_>>()
                    .into();
            }
            body
        }
    }

    fn is_loopback_url(url: &str) -> bool {
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let host = rest.split(['/', '?']).next().unwrap_or_default();
        let host = if host.starts_with('[') {
            host.split(']').next().unwrap_or_default().trim_start_matches('[')
        } else {
            host.split(':').next().unwrap_or_default()
        };
        host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
    }

    /// Messages in the chat completions shape: tool calls on assistant
    /// messages, one `tool` message per result.
    fn openai_messages(messages: &[ChatMessage]) -> Vec<Value> {
        let mut out = Vec::new();
        for m in messages {
            match m.role {
                Role::System => out.push(json!({ "role": "system", "content": m.text() })),
                Role::User => out.push(json!({ "role": "user", "content": m.text() })),
                Role::Assistant => {
                    let calls: Vec<Value> = m
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            Part::ToolUse { id, name, input } => {
                                Some(json!({ "id": id, "type": "function", "function": { "name": name, "arguments": input.to_string() } }))
                            }
                            _ => None,
                        })
                        .collect();
                    let text = m.text();
                    let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { text.into() } });
                    if !calls.is_empty() {
                        msg["tool_calls"] = calls.into();
                    }
                    out.push(msg);
                }
                Role::Tool => {
                    for p in &m.parts {
                        match p {
                            Part::ToolResult { id, content, is_error } => {
                                let content = if *is_error { format!("Error: {content}") } else { content.clone() };
                                out.push(json!({ "role": "tool", "tool_call_id": id, "content": content }));
                            }
                            Part::Text { text } if !text.is_empty() => out.push(json!({ "role": "user", "content": text })),
                            _ => {}
                        }
                    }
                }
            }
        }
        out
    }

    fn openai_stop(s: &str) -> StopReason {
        match s {
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        }
    }

    impl ChatModel for OpenAiCompatible {
        fn name(&self) -> &str {
            &self.model
        }

        fn locality(&self) -> Locality {
            self.locality
        }

        fn supports_tools(&self) -> bool {
            true
        }

        fn chat(&self, req: &ChatRequest, cancel: &CancelFlag, sink: &mut dyn FnMut(ChatEvent)) -> Result<(), LlmError> {
            let http = self.request("chat/completions", &stream_agent()).header("accept", "text/event-stream");
            let mut usage = Usage::default();
            let mut stop = StopReason::EndTurn;
            let mut tools: BTreeMap<u64, ToolAcc> = BTreeMap::new();
            let mut saw_any = false;
            stream(http, &self.body(req), cancel, &mut |piece| match piece {
                Body::Event(ev) => {
                    let data = ev.data.trim();
                    if data == "[DONE]" {
                        return Ok(false);
                    }
                    let Ok(v) = serde_json::from_str::<Value>(data) else { return Ok(true) };
                    if v.get("error").is_some_and(|e| !e.is_null()) {
                        return Err(err_from(200, &v));
                    }
                    saw_any = true;
                    if v["usage"].is_object() {
                        usage.input_tokens = u32_of(&v["usage"]["prompt_tokens"]);
                        usage.output_tokens = u32_of(&v["usage"]["completion_tokens"]);
                    }
                    let choice = &v["choices"][0];
                    let delta = &choice["delta"];
                    if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                        sink(ChatEvent::Text { text: t.into() });
                    }
                    for (i, call) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
                        let idx = call["index"].as_u64().unwrap_or(i as u64);
                        let acc = tools.entry(idx).or_default();
                        if let Some(id) = call["id"].as_str().filter(|s| !s.is_empty()) {
                            acc.id = id.into();
                        }
                        if let Some(n) = call["function"]["name"].as_str().filter(|s| !s.is_empty()) {
                            acc.name = n.into();
                        }
                        acc.json.push_str(call["function"]["arguments"].as_str().unwrap_or_default());
                    }
                    if let Some(f) = choice["finish_reason"].as_str() {
                        stop = openai_stop(f);
                    }
                    Ok(true)
                }
                Body::Json(v) => {
                    saw_any = true;
                    let msg = &v["choices"][0]["message"];
                    if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
                        sink(ChatEvent::Text { text: t.into() });
                    }
                    for (i, call) in msg["tool_calls"].as_array().into_iter().flatten().enumerate() {
                        tools.insert(
                            i as u64,
                            ToolAcc {
                                id: call["id"].as_str().unwrap_or_default().into(),
                                name: call["function"]["name"].as_str().unwrap_or_default().into(),
                                json: call["function"]["arguments"]
                                    .as_str()
                                    .map_or_else(|| call["function"]["arguments"].to_string(), str::to_string),
                            },
                        );
                    }
                    stop = openai_stop(v["choices"][0]["finish_reason"].as_str().unwrap_or_default());
                    usage.input_tokens = u32_of(&v["usage"]["prompt_tokens"]);
                    usage.output_tokens = u32_of(&v["usage"]["completion_tokens"]);
                    Ok(false)
                }
            })?;
            if !saw_any {
                return Err(LlmError::Request("the server sent no reply".into()));
            }
            let called = !tools.is_empty();
            for (n, mut acc) in std::mem::take(&mut tools) {
                if acc.id.is_empty() {
                    acc.id = format!("call_{n}");
                }
                sink(acc.finish());
            }
            // Some servers say "stop" even after calling tools.
            if called && stop == StopReason::EndTurn {
                stop = StopReason::ToolUse;
            }
            sink(ChatEvent::Done { stop, usage });
            Ok(())
        }
    }

    impl LanguageModel for OpenAiCompatible {
        fn name(&self) -> &str {
            &self.model
        }

        fn complete(&self, _task: &Assist, req: &LlmRequest) -> Result<String, LlmError> {
            complete_via_chat(self, req)
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
        let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn loopback_servers_are_local() {
            assert!(is_loopback_url("http://localhost:11434"));
            assert!(is_loopback_url("http://127.0.0.1:1234/v1"));
            assert!(is_loopback_url("http://[::1]:8080"));
            assert!(!is_loopback_url("https://api.openai.com"));
            assert_eq!(OpenAiCompatible::new("http://localhost:11434", "m", None).locality(), Locality::LocalServer);
        }

        #[test]
        fn anthropic_turns_alternate_and_carry_tool_blocks() {
            let msgs = vec![
                ChatMessage::user("find ropes"),
                ChatMessage {
                    role: Role::Assistant,
                    parts: vec![
                        Part::text("Searching."),
                        Part::ToolUse { id: "t1".into(), name: "search".into(), input: json!({"query": "ropes"}) },
                    ],
                },
                ChatMessage {
                    role: Role::Tool,
                    parts: vec![Part::ToolResult { id: "t1".into(), content: "2 hits".into(), is_error: false }],
                },
                ChatMessage::user("thanks"),
            ];
            let v = anthropic_messages(&msgs);
            assert_eq!(v.len(), 3, "the tool result and the next user text share one user turn: {v:#?}");
            assert_eq!(v[1]["content"][1]["type"], "tool_use");
            assert_eq!(v[2]["content"][0]["tool_use_id"], "t1");
            assert_eq!(v[2]["content"][1]["text"], "thanks");
            let o = openai_messages(&msgs);
            assert_eq!(o[1]["tool_calls"][0]["function"]["arguments"], r#"{"query":"ropes"}"#);
            assert_eq!(o[2]["role"], "tool");
            assert_eq!(o[2]["tool_call_id"], "t1");
        }
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
