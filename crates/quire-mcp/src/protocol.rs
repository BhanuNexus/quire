// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Protocol versions and eras (design §4.1).
//!
//! A request whose `params._meta` names `io.modelcontextprotocol/protocolVersion`
//! is *modern* (2026-07-28): results carry `resultType` and the server's
//! identity in `_meta`. A stdio process that received `initialize` is
//! *legacy* for the rest of its life. Legacy answers are 0.8's, plus the
//! fields each legacy version defines (see [`Era`]).

use serde_json::{Value, json};

/// The current (modern) version.
pub const MODERN: &str = "2026-07-28";
/// Versions negotiated through `initialize`, newest first.
pub const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Everything this server speaks, newest first.
pub const SUPPORTED: &[&str] = &[MODERN, "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_CLIENT_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";

/// JSON-RPC error codes used here.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// Resource not found before 2026-07-28 (now `INVALID_PARAMS`).
    pub const RESOURCE_NOT_FOUND_LEGACY: i64 = -32002;
    /// HTTP headers don't match the body, or required ones are missing.
    pub const HEADER_MISMATCH: i64 = -32020;
    /// The requested protocol version isn't supported.
    pub const UNSUPPORTED_VERSION: i64 = -32022;
}

/// How to answer one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Era {
    /// 2026-07-28 semantics.
    Modern,
    /// An older version: from `initialize` (stdio) or the
    /// `MCP-Protocol-Version` header (HTTP).
    Legacy(String),
}

impl Era {
    pub const fn is_modern(&self) -> bool {
        matches!(self, Self::Modern)
    }

    /// The version string this era answers in.
    pub fn version(&self) -> &str {
        match self {
            Self::Modern => MODERN,
            Self::Legacy(v) => v,
        }
    }

    /// At least `v` (versions are dates, so they compare as strings).
    fn at_least(&self, v: &str) -> bool {
        self.version() >= v
    }

    /// `title`, `outputSchema`, `structuredContent` and `resource_link`
    /// (2025-06-18 and later).
    pub fn structured(&self) -> bool {
        self.at_least("2025-06-18")
    }

    /// Tool `annotations` (2025-03-26 and later).
    pub fn annotations(&self) -> bool {
        self.at_least("2025-03-26")
    }
}

/// The version a request names in `_meta`, if any.
pub fn requested_version(params: &Value) -> Option<&str> {
    params.get("_meta")?.get(META_VERSION)?.as_str()
}

/// `{"name": "quire", "version": …}`.
pub fn server_info() -> Value {
    json!({ "name": "quire", "title": "Quire", "version": env!("CARGO_PKG_VERSION") })
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

/// A JSON-RPC error object.
pub fn error(code: i64, message: impl Into<String>) -> Value {
    json!({ "code": code, "message": message.into() })
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

/// A resource that doesn't exist (the code changed in 2026-07-28).
pub fn resource_not_found(era: &Era, uri: &str) -> Value {
    let c = if era.is_modern() { code::INVALID_PARAMS } else { code::RESOURCE_NOT_FOUND_LEGACY };
    json!({ "code": c, "message": format!("Resource not found: {uri}"), "data": { "uri": uri } })
}

/// The era of a request that names a version: modern, a known legacy
/// version, or an error.
pub fn era_for(version: &str) -> Result<Era, Value> {
    if version == MODERN {
        Ok(Era::Modern)
    } else if LEGACY.contains(&version) {
        Ok(Era::Legacy(version.to_string()))
    } else {
        Err(unsupported_version(version))
    }
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

    #[test]
    fn features_follow_the_version() {
        let e = |v: &str| era_for(v).unwrap();
        assert!(e(MODERN).structured() && e(MODERN).annotations());
        assert!(e("2025-06-18").structured());
        assert!(!e("2025-03-26").structured() && e("2025-03-26").annotations());
        assert!(!e("2024-11-05").annotations());
        assert!(era_for("2030-01-01").is_err());
        assert_eq!(resource_not_found(&Era::Modern, "x")["code"], code::INVALID_PARAMS);
        assert_eq!(resource_not_found(&e("2025-06-18"), "x")["code"], code::RESOURCE_NOT_FOUND_LEGACY);
    }
}
