// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Protocol versions and eras (design §4.1).
//!
//! A request whose `params._meta` names `io.modelcontextprotocol/protocolVersion`
//! is *modern* (2026-07-28): results carry `resultType` and the server's
//! identity in `_meta`. A stdio process that received `initialize` is
//! *legacy* for the rest of its life, and answers exactly as Quire 0.8 did.

use serde_json::{Value, json};

/// The current (modern) version.
pub const MODERN: &str = "2026-07-28";
/// Versions negotiated through `initialize`, newest first.
pub const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Everything this server speaks, newest first.
pub const SUPPORTED: &[&str] = &[MODERN, "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// JSON-RPC error codes used here.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// The requested protocol version isn't supported.
    pub const UNSUPPORTED_VERSION: i64 = -32022;
}

/// How to answer one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Era {
    /// 2026-07-28 semantics.
    Modern,
    /// `initialize` was received; the negotiated version.
    Legacy(String),
}

impl Era {
    pub const fn is_modern(&self) -> bool {
        matches!(self, Self::Modern)
    }
}

/// The version a request names in `_meta`, if any.
pub fn requested_version(params: &Value) -> Option<&str> {
    params.get("_meta")?.get(META_VERSION)?.as_str()
}

/// `{"name": "quire", "version": …}`.
pub fn server_info() -> Value {
    json!({ "name": "quire", "version": env!("CARGO_PKG_VERSION") })
}

/// Add the modern result envelope.
pub fn modern_result(mut result: Value) -> Value {
    if let Some(obj) = result.as_object_mut() {
        obj.entry("resultType").or_insert_with(|| "complete".into());
        let meta = obj.entry("_meta").or_insert_with(|| json!({}));
        if let Some(m) = meta.as_object_mut() {
            m.insert(META_SERVER_INFO.into(), server_info());
        }
    }
    result
}

/// The error for a version this server doesn't speak.
pub fn unsupported_version(requested: &str) -> Value {
    json!({
        "code": code::UNSUPPORTED_VERSION,
        "message": format!("unsupported protocol version {requested}"),
        "data": { "supported": SUPPORTED, "requested": requested },
    })
}

/// The error for a request that names no version outside a legacy session.
pub fn missing_version() -> Value {
    json!({
        "code": code::INVALID_PARAMS,
        "message": format!("missing {META_VERSION}; this server supports {}", SUPPORTED.join(", ")),
        "data": { "supported": SUPPORTED },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes() {
        let r = modern_result(json!({"tools": []}));
        assert_eq!(r["resultType"], "complete");
        assert_eq!(r["_meta"][META_SERVER_INFO]["name"], "quire");
        assert_eq!(requested_version(&json!({"_meta": {META_VERSION: MODERN}})), Some(MODERN));
        assert_eq!(requested_version(&json!({})), None);
        assert_eq!(unsupported_version("1999-01-01")["data"]["supported"][0], MODERN);
    }
}
