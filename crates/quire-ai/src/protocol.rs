// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The engine protocol: one JSON object per line over the engine's stdio
//! (design §D.3, docs/formats/engine-protocol.md).
//!
//! Requests are `{id, method, params}`. Replies are `{id, result}` or
//! `{id, error}`, and streams send `{id, event}` lines before the final
//! reply. `hello` must come first, and the app and engine versions must match.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when a message changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod method {
    pub const HELLO: &str = "hello";
    pub const LOAD: &str = "load";
    pub const EMBED: &str = "embed";
    pub const GENERATE: &str = "generate";
    pub const OCR: &str = "ocr";
    pub const CANCEL: &str = "cancel";
    pub const UNLOAD: &str = "unload";
    pub const SHUTDOWN: &str = "shutdown";
}

/// Error codes in replies.
pub mod code {
    /// The method doesn't exist in this engine build.
    pub const UNKNOWN_METHOD: i32 = -32601;
    pub const BAD_PARAMS: i32 = -32602;
    /// App and engine versions differ.
    pub const VERSION_MISMATCH: i32 = 1;
    pub const MODEL_NOT_LOADED: i32 = 2;
    pub const CANCELLED: i32 = 3;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObj {
    pub code: i32,
    pub message: String,
}

/// One line from the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObj>,
    /// A streamed event (`generate`); more lines follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
}

impl Reply {
    pub fn ok(id: u64, result: Value) -> Self {
        Self { id, result: Some(result), error: None, event: None }
    }

    pub fn err(id: u64, code: i32, message: impl Into<String>) -> Self {
        Self { id, result: None, error: Some(ErrorObj { code, message: message.into() }), event: None }
    }

    pub fn event(id: u64, event: Value) -> Self {
        Self { id, result: None, error: None, event: Some(event) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    pub app_version: String,
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub engine_version: String,
    pub protocol: u32,
    /// `cpu`, `metal`.
    pub devices: Vec<String>,
    pub threads: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Embed,
    Llm,
    Ocr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadParams {
    pub model: String,
    pub path: String,
    pub sha256: String,
    pub kind: ModelKind,
    #[serde(default)]
    pub ctx: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadResult {
    pub loaded: bool,
    pub mem_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedParams {
    pub model: String,
    /// At most 64 per request.
    pub texts: Vec<String>,
    #[serde(default)]
    pub normalize: bool,
    /// `query: `, `passage: ` or empty (multilingual-e5 wants them).
    #[serde(default)]
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedResult {
    pub dims: u32,
    /// Row-major little-endian f16 vectors, base64.
    pub vectors_f16_b64: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateParams {
    pub model: String,
    /// Chat messages in `quire_intel::chat::ChatMessage` JSON form.
    pub messages: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    pub max_tokens: u32,
    pub temperature: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(default)]
    pub json: bool,
}

/// Streamed `generate` events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GenerateEvent {
    Delta { delta: String },
    ToolCall { tool_call: Value },
    Done { done: bool, usage: Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfPage {
    pub path: String,
    /// 0-based.
    pub page: u32,
    pub dpi: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrParams {
    pub model_set: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfPage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrLine {
    pub text: String,
    /// Four corners, clockwise from top-left, in pixels.
    pub quad: [[f32; 2]; 4],
    pub conf: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrResult {
    pub lines: Vec<OcrLine>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_the_documented_shapes() {
        let req: Request = serde_json::from_str(r#"{"id":1,"method":"hello","params":{"app_version":"0.9.0","protocol":1}}"#).unwrap();
        let hello: HelloParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        let r = Reply::err(1, code::VERSION_MISMATCH, "engine 0.9.1, app 0.9.0");
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"id":1,"error":{"code":1,"message":"engine 0.9.1, app 0.9.0"}}"#);
        let ev: GenerateEvent = serde_json::from_str(r#"{"delta":"Ro"}"#).unwrap();
        assert_eq!(ev, GenerateEvent::Delta { delta: "Ro".into() });
        let load: LoadParams = serde_json::from_str(r#"{"model":"bge","path":"/m","sha256":"ab","kind":"embed"}"#).unwrap();
        assert_eq!((load.kind, load.ctx), (ModelKind::Embed, 0));
    }
}
