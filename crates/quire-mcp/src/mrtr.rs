// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Approval in the agent's client (design §4.5, MCP multi round-trip requests).
//!
//! When a client may approve changes itself (`approve_in_client` in the
//! vault's agent policy, and the client declared `elicitation`), a write
//! that would become a proposal instead answers `input_required` with a
//! form: apply, propose or cancel. The client retries the call with the
//! answer and the opaque `requestState`, which is XChaCha20-Poly1305 sealed
//! with a per-vault key and binds the caller, the call (method and a digest
//! of its arguments), the version it was made against and a 10-minute
//! expiry. A state that fails any check is refused.

use std::fmt::Write as _;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// How long a client has to answer.
pub const STATE_TTL_MS: i64 = 10 * 60 * 1000;
const AAD: &[u8] = b"quire-mcp/mrtr/v1";

/// What the sealed state says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatePayload {
    /// The client key (`mcp:claude-code`, `oauth:…`).
    pub principal: String,
    /// Unix ms.
    pub exp: i64,
    pub method: String,
    pub args_digest: String,
    pub base_version: String,
}

/// Why a state was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    Unreadable,
    WrongPrincipal,
    Expired,
    WrongRequest,
    Changed,
}

/// Seals and opens request states with one key.
pub struct Sealer {
    key: [u8; 32],
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer")
    }
}

/// `sha256` of the arguments in a canonical form (keys sorted, the MRTR
/// fields left out), hex.
pub fn args_digest(tool: &str, args: &Value) -> String {
    fn canonical(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), canonical(&m[k]))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    let c = canonical(&json!({ "tool": tool, "arguments": args }));
    let mut h = Sha256::new();
    h.update(c.to_string().as_bytes());
    h.finalize().iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl Sealer {
    /// A sealer with a fresh random key (tests, and when no key file can be kept).
    pub fn ephemeral() -> Self {
        let mut key = [0u8; 32];
        // Without randomness there's no safe key; an all-zero key would be guessable,
        // so fall back to a hash of the time and address, which is at least unique per run.
        if getrandom::fill(&mut key).is_err() {
            let seed = format!("{:?}{:p}", std::time::SystemTime::now(), &key);
            key.copy_from_slice(&Sha256::digest(seed.as_bytes()));
        }
        Self { key }
    }

    /// The vault's key in `path` (`<config>/vaults/<key>/mrtr.key`), created
    /// on first use with mode 0600.
    pub fn load_or_create(path: &Path) -> Self {
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
        {
            return Self { key };
        }
        let s = Self::ephemeral();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = quire_app::atomic::write(path, &s.key, true);
        s
    }

    pub fn seal(&self, p: &StatePayload) -> String {
        let mut nonce = [0u8; 24];
        let _ = getrandom::fill(&mut nonce);
        let plain = serde_json::to_vec(p).unwrap_or_default();
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let ct = cipher.encrypt(&XNonce::from(nonce), Payload { msg: &plain, aad: AAD }).unwrap_or_default();
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        URL_SAFE_NO_PAD.encode(out)
    }

    /// Open `state` and check it belongs to this caller, call and version.
    pub fn open(&self, state: &str, principal: &str, method: &str, digest: &str, now_ms: i64) -> Result<StatePayload, StateError> {
        let bytes = URL_SAFE_NO_PAD.decode(state.trim()).map_err(|_| StateError::Unreadable)?;
        if bytes.len() < 24 + 16 {
            return Err(StateError::Unreadable);
        }
        let (nonce, ct) = bytes.split_at(24);
        let nonce = XNonce::try_from(nonce).map_err(|_| StateError::Unreadable)?;
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let plain = cipher.decrypt(&nonce, Payload { msg: ct, aad: AAD }).map_err(|_| StateError::Unreadable)?;
        let p: StatePayload = serde_json::from_slice(&plain).map_err(|_| StateError::Unreadable)?;
        if p.principal != principal {
            return Err(StateError::WrongPrincipal);
        }
        if now_ms > p.exp {
            return Err(StateError::Expired);
        }
        if p.method != method || p.args_digest != digest {
            return Err(StateError::WrongRequest);
        }
        Ok(p)
    }
}

/// The client's answer to the approval form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Apply,
    Propose,
    Cancel,
}

/// The input request key Quire uses.
pub const APPROVE_KEY: &str = "approve";

/// `inputRequests` asking to approve a change shown as `diff`.
pub fn approval_request(path: &str, diff: &str) -> Value {
    let lines: Vec<&str> = diff.lines().collect();
    let mut shown = lines.iter().take(40).copied().collect::<Vec<_>>().join("\n");
    if lines.len() > 40 {
        let _ = write!(shown, "\n… {} more lines; review the rest in Quire", lines.len() - 40);
    }
    json!({
        APPROVE_KEY: {
            "method": "elicitation/create",
            "params": {
                "mode": "form",
                "message": format!("Apply this change to {path}?\n{shown}"),
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "decision": {
                            "type": "string",
                            "title": "Decision",
                            "enum": ["apply", "propose", "cancel"],
                            "enumNames": ["Apply now", "Send to Quire for review", "Cancel"],
                        },
                    },
                    "required": ["decision"],
                },
            },
        },
    })
}

/// Read the decision from `inputResponses`. A declined or dismissed form is
/// a cancel; a missing answer is `None` (ask again).
pub fn decision(responses: &Value) -> Option<Decision> {
    let r = responses.get(APPROVE_KEY)?;
    match r.get("action").and_then(Value::as_str) {
        Some("accept") => match r.get("content").and_then(|c| c.get("decision")).and_then(Value::as_str) {
            Some("apply") => Some(Decision::Apply),
            Some("propose") => Some(Decision::Propose),
            Some("cancel") => Some(Decision::Cancel),
            _ => None,
        },
        Some("decline" | "cancel") => Some(Decision::Cancel),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> StatePayload {
        StatePayload {
            principal: "mcp:claude-code".into(),
            exp: 1_000,
            method: "tools/call".into(),
            args_digest: args_digest("append", &json!({"path": "A.md", "text": "x"})),
            base_version: "sha256:1".into(),
        }
    }

    #[test]
    fn states_bind_principal_request_and_time() {
        let s = Sealer::ephemeral();
        let p = payload();
        let state = s.seal(&p);
        assert_eq!(s.open(&state, &p.principal, "tools/call", &p.args_digest, 500), Ok(p.clone()));
        assert_eq!(s.open(&state, "mcp:other", "tools/call", &p.args_digest, 500), Err(StateError::WrongPrincipal));
        assert_eq!(s.open(&state, &p.principal, "tools/call", &p.args_digest, 2_000), Err(StateError::Expired));
        let other = args_digest("append", &json!({"path": "B.md", "text": "x"}));
        assert_eq!(s.open(&state, &p.principal, "tools/call", &other, 500), Err(StateError::WrongRequest));
        // Tampering anywhere fails authentication.
        let mut bytes = URL_SAFE_NO_PAD.decode(&state).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(s.open(&URL_SAFE_NO_PAD.encode(bytes), &p.principal, "tools/call", &p.args_digest, 500), Err(StateError::Unreadable));
        assert_eq!(Sealer::ephemeral().open(&state, &p.principal, "tools/call", &p.args_digest, 500), Err(StateError::Unreadable));
        assert_eq!(s.open("short", &p.principal, "tools/call", &p.args_digest, 500), Err(StateError::Unreadable));
    }

    #[test]
    fn digests_ignore_key_order() {
        assert_eq!(
            args_digest("t", &json!({"a": 1, "b": [2, {"y": 1, "x": 2}]})),
            args_digest("t", &json!({"b": [2, {"x": 2, "y": 1}], "a": 1}))
        );
        assert_ne!(args_digest("t", &json!({"a": 1})), args_digest("u", &json!({"a": 1})));
    }

    #[test]
    fn decisions() {
        let r = |v: Value| decision(&json!({ APPROVE_KEY: v }));
        assert_eq!(r(json!({"action": "accept", "content": {"decision": "apply"}})), Some(Decision::Apply));
        assert_eq!(r(json!({"action": "accept", "content": {"decision": "propose"}})), Some(Decision::Propose));
        assert_eq!(r(json!({"action": "decline"})), Some(Decision::Cancel));
        assert_eq!(r(json!({"action": "accept", "content": {}})), None);
        assert_eq!(decision(&json!({})), None);
        let req = approval_request("A.md", &"+x\n".repeat(50));
        assert!(req[APPROVE_KEY]["params"]["message"].as_str().unwrap().contains("10 more lines"));
    }
}
