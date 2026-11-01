// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Talk to `quire-sync-server` over HTTP(S).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::crypto::WrappedKey;
use crate::protocol::{PushOutcome, Revision, SpaceId};
use crate::transport::{Transport, TransportError};

/// A space on a sync server.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    base: String,
    space: SpaceId,
    token: String,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct Changes {
    revisions: Vec<Revision>,
}

#[derive(Deserialize)]
struct Latest {
    latest: u64,
}

#[derive(Serialize)]
struct CreateSpace<'a> {
    space: &'a SpaceId,
    wrapped_key: &'a WrappedKey,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

const PAGE: u32 = 1000;

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .user_agent(format!("Quire/{}", env!("CARGO_PKG_VERSION")))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .build()
        .into()
}

impl HttpTransport {
    /// `base` is the server's address, e.g. `https://sync.example.com`.
    pub fn new(base: &str, space: SpaceId, token: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_string(), space, token: token.to_string(), agent: agent(Duration::from_mins(1)) }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/spaces/{}{path}", self.base, self.space)
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn fail(status: u16, body: &str) -> TransportError {
        let msg = serde_json::from_str::<ErrorBody>(body).map_or_else(|_| format!("HTTP {status}"), |e| e.error);
        match status {
            401 => TransportError::Refused("the server token is wrong".into()),
            404 => TransportError::Refused("this vault isn't on the server".into()),
            _ => TransportError::Refused(msg),
        }
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, agent: &ureq::Agent, url: &str) -> Result<T, TransportError> {
        let mut res = agent.get(url).header("Authorization", &self.auth()).call().map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        let body = res.body_mut().read_to_string().map_err(|e| TransportError::Offline(e.to_string()))?;
        if status != 200 {
            return Err(Self::fail(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| TransportError::Refused(format!("unexpected reply: {e}")))
    }

    /// Put a new space and its wrapped key on the server.
    pub fn create_space(&self, key: &WrappedKey) -> Result<(), TransportError> {
        let body = serde_json::to_string(&CreateSpace { space: &self.space, wrapped_key: key }).unwrap_or_default();
        let mut res = self
            .agent
            .post(format!("{}/v1/spaces", self.base))
            .header("Authorization", &self.auth())
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        if status == 201 {
            return Ok(());
        }
        let text = res.body_mut().read_to_string().unwrap_or_default();
        Err(Self::fail(status, &text))
    }

    /// The space's wrapped key, to join from a new device.
    pub fn wrapped_key(&self) -> Result<WrappedKey, TransportError> {
        self.get_json(&self.agent, &self.url("/key"))
    }

    /// Wait until the log passes `after` (or `secs` pass). Returns the latest position.
    pub fn wait(&self, after: u64, secs: u64) -> Result<u64, TransportError> {
        let long = agent(Duration::from_secs(secs + 15));
        let l: Latest = self.get_json(&long, &self.url(&format!("/wait?after={after}&timeout={secs}")))?;
        Ok(l.latest)
    }
}

impl Transport for HttpTransport {
    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        let mut out = Vec::new();
        let mut cursor = after;
        loop {
            let page: Changes = self.get_json(&self.agent, &self.url(&format!("/changes?after={cursor}&limit={PAGE}")))?;
            let n = page.revisions.len();
            if let Some(last) = page.revisions.last() {
                cursor = last.seq;
            }
            out.extend(page.revisions);
            if n < PAGE as usize {
                return Ok(out);
            }
        }
    }

    fn push(&self, rev: &Revision) -> Result<PushOutcome, TransportError> {
        let body = serde_json::to_string(rev).unwrap_or_default();
        let mut res = self
            .agent
            .post(self.url("/revisions"))
            .header("Authorization", &self.auth())
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().map_err(|e| TransportError::Offline(e.to_string()))?;
        match status {
            200 | 409 => serde_json::from_str(&text).map_err(|e| TransportError::Refused(format!("unexpected reply: {e}"))),
            _ => Err(Self::fail(status, &text)),
        }
    }
}
