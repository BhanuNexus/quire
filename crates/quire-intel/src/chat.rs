// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chat with tools: the port every model sits behind in 0.9 (cloud
//! providers, a local server, the on-device engine, and [`ScriptedChat`] in
//! tests). Design §2.5 and §D.4.
//!
//! A model streams [`ChatEvent`]s into a sink. The older one-shot
//! [`LanguageModel`] port stays for the assist features; [`LanguageModelChat`]
//! adapts any of its providers to [`ChatModel`] (text only, no tools) until
//! WS-D gives the providers native tool use and streaming.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::{Assist, LanguageModel, LlmError, LlmRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { id: String, content: String, is_error: bool },
}

impl Part {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, parts: vec![Part::text(text)] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, parts: vec![Part::text(text)] }
    }

    /// The text parts joined (tool calls and results left out).
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                Part::ToolUse { .. } | Part::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A tool the model may call. `input_schema` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Ask for a JSON reply (providers that support a JSON mode use it).
    pub json: bool,
}

impl ChatRequest {
    /// A single-turn request with no tools.
    pub fn simple(system: impl Into<String>, user: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            system: system.into(),
            messages: vec![ChatMessage::user(user)],
            tools: Vec::new(),
            max_tokens,
            temperature: 0.2,
            json: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ChatEvent {
    /// A piece of the reply (streamed providers send many).
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Done {
        stop: StopReason,
        usage: Usage,
    },
}

/// Where a model runs, shown in the UI ("cloud", "this computer").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locality {
    Cloud,
    Device,
    LocalServer,
}

/// Shared stop button: set once, seen by every holder.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A model that chats, optionally with tools.
pub trait ChatModel: Send + Sync {
    fn name(&self) -> &str;
    fn locality(&self) -> Locality;
    fn supports_tools(&self) -> bool;
    /// Run one turn, sending events to `sink` as they arrive and ending
    /// with exactly one `Done`. Returns early with `LlmError::Cancelled`
    /// when `cancel` is set.
    fn chat(&self, req: &ChatRequest, cancel: &CancelFlag, sink: &mut dyn FnMut(ChatEvent)) -> Result<(), LlmError>;
}

/// Run a turn and collect its text and how it stopped.
pub fn chat_text(model: &dyn ChatModel, req: &ChatRequest, cancel: &CancelFlag) -> Result<(String, StopReason), LlmError> {
    let mut text = String::new();
    let mut stop = StopReason::EndTurn;
    model.chat(req, cancel, &mut |ev| match ev {
        ChatEvent::Text { text: t } => text.push_str(&t),
        ChatEvent::ToolUse { .. } => {}
        ChatEvent::Done { stop: s, .. } => stop = s,
    })?;
    Ok((text, stop))
}

// ------------------------------------------------------------------ test double

/// Replays a script: each `chat` call emits the next turn's events (a
/// `Done` is added if the turn has none) and records the request.
#[derive(Debug, Default)]
pub struct ScriptedChat {
    name: String,
    tools: bool,
    turns: Mutex<VecDeque<Vec<ChatEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedChat {
    pub fn new(turns: Vec<Vec<ChatEvent>>) -> Self {
        Self { name: "Scripted".into(), tools: true, turns: Mutex::new(turns.into()), requests: Mutex::default() }
    }

    /// Each turn replies with one text.
    pub fn texts<S: Into<String>>(replies: impl IntoIterator<Item = S>) -> Self {
        Self::new(replies.into_iter().map(|t| vec![ChatEvent::Text { text: t.into() }]).collect())
    }

    #[must_use]
    pub fn without_tools(mut self) -> Self {
        self.tools = false;
        self
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Turns not played yet.
    pub fn remaining(&self) -> usize {
        self.turns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }
}

impl ChatModel for ScriptedChat {
    fn name(&self) -> &str {
        &self.name
    }

    fn locality(&self) -> Locality {
        Locality::Device
    }

    fn supports_tools(&self) -> bool {
        self.tools
    }

    fn chat(&self, req: &ChatRequest, cancel: &CancelFlag, sink: &mut dyn FnMut(ChatEvent)) -> Result<(), LlmError> {
        self.requests.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(req.clone());
        let turn = self.turns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop_front();
        let Some(turn) = turn else { return Err(LlmError::Request("the script has no more turns".into())) };
        let mut done = false;
        for ev in turn {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            done |= matches!(ev, ChatEvent::Done { .. });
            sink(ev);
        }
        if !done {
            sink(ChatEvent::Done { stop: StopReason::EndTurn, usage: Usage::default() });
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ adapter

/// Any [`LanguageModel`] as a text-only [`ChatModel`]: the conversation is
/// flattened into one prompt and the reply arrives as one `Text` event.
pub struct LanguageModelChat {
    inner: Arc<dyn LanguageModel>,
    locality: Locality,
}

impl LanguageModelChat {
    pub fn new(inner: Arc<dyn LanguageModel>, locality: Locality) -> Self {
        Self { inner, locality }
    }
}

/// The conversation as one prompt. A single user message is passed as it is.
fn flatten(messages: &[ChatMessage]) -> String {
    if let [only] = messages
        && only.role == Role::User
    {
        return only.text();
    }
    let mut out = String::new();
    for m in messages {
        let who = match m.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        out.push_str(who);
        out.push_str(":\n");
        out.push_str(&m.text());
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

impl ChatModel for LanguageModelChat {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn locality(&self) -> Locality {
        self.locality
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn chat(&self, req: &ChatRequest, cancel: &CancelFlag, sink: &mut dyn FnMut(ChatEvent)) -> Result<(), LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let prompt = flatten(&req.messages);
        let question = req.messages.iter().rev().find(|m| m.role == Role::User).map(ChatMessage::text).unwrap_or_default();
        let text = self
            .inner
            .complete(&Assist::Ask { question }, &LlmRequest { system: req.system.clone(), prompt, max_tokens: req.max_tokens })?;
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        sink(ChatEvent::Text { text });
        sink(ChatEvent::Done { stop: StopReason::EndTurn, usage: Usage::default() });
        Ok(())
    }
}

/// Any [`ChatModel`] as a one-shot [`LanguageModel`], for the assist
/// features (summarize, rewrite…) when the model is the on-device one.
pub struct ChatLanguageModel {
    inner: Arc<dyn ChatModel>,
}

impl ChatLanguageModel {
    pub fn new(inner: Arc<dyn ChatModel>) -> Self {
        Self { inner }
    }
}

impl LanguageModel for ChatLanguageModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn complete(&self, _task: &Assist, req: &LlmRequest) -> Result<String, LlmError> {
        let chat = ChatRequest::simple(req.system.clone(), req.prompt.clone(), req.max_tokens);
        let (text, _) = chat_text(self.inner.as_ref(), &chat, &CancelFlag::new())?;
        if text.trim().is_empty() { Err(LlmError::Request("the model sent an empty reply".into())) } else { Ok(text) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_models_complete_one_shot() {
        let m = ChatLanguageModel::new(Arc::new(ScriptedChat::texts(["- one", ""])));
        let req = LlmRequest { system: "s".into(), prompt: "p".into(), max_tokens: 10 };
        assert_eq!(m.complete(&Assist::Summarize, &req).unwrap(), "- one");
        assert!(m.complete(&Assist::Summarize, &req).is_err(), "an empty reply is an error");
        assert_eq!(m.name(), "Scripted");
    }

    #[test]
    fn scripted_chat_replays_and_records() {
        let m = ScriptedChat::new(vec![
            vec![
                ChatEvent::ToolUse { id: "t1".into(), name: "search".into(), input: serde_json::json!({"query": "ropes"}) },
                ChatEvent::Done { stop: StopReason::ToolUse, usage: Usage::default() },
            ],
            vec![ChatEvent::Text { text: "Ropes ".into() }, ChatEvent::Text { text: "share subtrees.".into() }],
        ]);
        let cancel = CancelFlag::new();
        let mut events = Vec::new();
        m.chat(&ChatRequest::simple("sys", "why ropes?", 100), &cancel, &mut |e| events.push(e)).unwrap();
        assert!(matches!(events[0], ChatEvent::ToolUse { .. }));
        let (text, stop) = chat_text(&m, &ChatRequest::simple("sys", "go on", 100), &cancel).unwrap();
        assert_eq!((text.as_str(), stop), ("Ropes share subtrees.", StopReason::EndTurn));
        assert_eq!(m.requests().len(), 2);
        assert_eq!(m.remaining(), 0);
        assert!(chat_text(&m, &ChatRequest::simple("", "more", 10), &cancel).is_err(), "script exhausted");
        let cancelled = CancelFlag::new();
        cancelled.cancel();
        let m = ScriptedChat::texts(["never"]);
        assert!(matches!(chat_text(&m, &ChatRequest::simple("", "x", 10), &cancelled), Err(LlmError::Cancelled)));
    }

    #[test]
    fn language_models_chat_as_text() {
        let m = LanguageModelChat::new(Arc::new(crate::llm::LocalModel), Locality::Device);
        assert!(!m.supports_tools());
        let req = ChatRequest::simple("sys", "x", 10);
        // The offline model can't answer questions; the error passes through.
        assert!(matches!(chat_text(&m, &req, &CancelFlag::new()), Err(LlmError::Unsupported(_))));
        assert_eq!(flatten(&[ChatMessage::user("hi")]), "hi");
        assert_eq!(flatten(&[ChatMessage::user("hi"), ChatMessage::assistant("yo")]), "User:\nhi\n\nAssistant:\nyo");
    }

    #[test]
    fn messages_serialize_with_tagged_parts() {
        let m =
            ChatMessage { role: Role::Tool, parts: vec![Part::ToolResult { id: "t1".into(), content: "3 hits".into(), is_error: false }] };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"role": "tool", "parts": [{"type": "tool_result", "id": "t1", "content": "3 hits", "is_error": false}]})
        );
        assert_eq!(serde_json::from_value::<ChatMessage>(v).unwrap(), m);
    }
}
