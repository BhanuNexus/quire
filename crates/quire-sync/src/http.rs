// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Talk to `quire-sync-server` over HTTP(S).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::crypto::WrappedKey;
use crate::keyring::{DeviceRecord, EpochRecord};
use crate::protocol::{
    DeviceId, HARDENED_PROTOCOL, LanePos, MIN_PROTOCOL, PROTOCOL, PROTOCOL_HEADER, PushOutcome, Revision, Sealing, ServerInfo, SpaceId,
};
use crate::transport::{Transport, TransportError};

/// A space on a sync server.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    base: String,
    space: SpaceId,
    token: String,
    agent: ureq::Agent,
    /// The protocol sent in `Quire-Sync-Protocol` (tests act as older clients).
    protocol: u32,
    /// The space's minimum protocol, once read (0: not yet).
    min: Arc<AtomicU32>,
    /// Devices removed from the vault, and the last log position still
    /// taken from each (0.13): their later revisions are ignored.
    removed: Arc<Mutex<BTreeMap<DeviceId, LanePos>>>,
}

/// A device of the space, as the server lists it (0.13).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceRow {
    pub device: String,
    #[serde(default)]
    pub enrolled: bool,
    #[serde(default)]
    pub removed: bool,
    #[serde(default)]
    pub last_seq: u64,
    #[serde(default)]
    pub record: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Enrolled {
    token: String,
}

/// What a server speaks, from `GET /v1/info` (design §B.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiated {
    /// Both speak this version.
    Version(u32),
    /// The server needs a newer Quire than this one.
    TooOld { min: u32 },
}

impl Negotiated {
    /// What to tell the person, if anything.
    pub fn notice(self) -> Option<String> {
        match self {
            // Sharing vault settings needs protocol 2; a protocol 2 server
            // lacks only removing devices (0.13), which says so when tried.
            Self::Version(v) if v < 2 => {
                Some("This sync server is older; some features (sharing vault settings across devices) are off until it's updated.".into())
            }
            Self::Version(_) => None,
            Self::TooOld { min } => Some(format!("This sync server needs a newer Quire (protocol {min}); update Quire on this device.")),
        }
    }
}

#[derive(Deserialize)]
struct Changes {
    revisions: Vec<Revision>,
    /// The newest position in the log; paging stops there.
    #[serde(default)]
    latest: u64,
}

/// Replies can carry attachments: up to a page of about 8 MB plus one
/// revision of a 10 MB file (base64, then hex).
const MAX_REPLY: u64 = 96 * 1024 * 1024;

#[derive(Deserialize)]
struct Latest {
    latest: u64,
}

#[derive(Serialize)]
struct CreateSpace<'a> {
    space: &'a SpaceId,
    wrapped_key: &'a WrappedKey,
}

/// A space as the server lists it (it knows nothing more about it).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SpaceInfo {
    pub space: SpaceId,
    /// Unix seconds.
    pub created: i64,
    pub revisions: u64,
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
        Self {
            base: base.trim_end_matches('/').to_string(),
            space,
            token: token.to_string(),
            agent: agent(Duration::from_mins(1)),
            protocol: PROTOCOL,
            min: Arc::new(AtomicU32::new(0)),
            removed: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// A device token for `device` in `space` (0.13), made with the
    /// server's token. The device keeps this one instead from then on.
    pub fn enroll(base: &str, space: &SpaceId, server_token: &str, device: &DeviceId) -> Result<String, TransportError> {
        let t = Self::new(base, space.clone(), server_token);
        let body = serde_json::json!({ "device": device }).to_string();
        let (status, text) = t.send("POST", &t.url("/devices"), Some(&body))?;
        match status {
            200 => serde_json::from_str::<Enrolled>(&text)
                .map(|e| e.token)
                .map_err(|e| TransportError::Refused(format!("unexpected reply: {e}"))),
            404 | 405 if !text.contains("vault") => Err(TransportError::Refused("Update your sync server first.".into())),
            _ => Err(Self::fail(status, &text)),
        }
    }

    /// A request with this transport's token; the status and body.
    fn send(&self, method: &str, url: &str, body: Option<&str>) -> Result<(u16, String), TransportError> {
        let auth = self.auth();
        let proto = self.protocol.to_string();
        let res = match (method, body) {
            ("PUT", Some(b)) => self
                .agent
                .put(url)
                .header("Authorization", &auth)
                .header(PROTOCOL_HEADER, &proto)
                .header("Content-Type", "application/json")
                .send(b),
            ("POST", Some(b)) => self
                .agent
                .post(url)
                .header("Authorization", &auth)
                .header(PROTOCOL_HEADER, &proto)
                .header("Content-Type", "application/json")
                .send(b),
            ("DELETE", _) => self.agent.delete(url).header("Authorization", &auth).header(PROTOCOL_HEADER, &proto).call(),
            _ => self.agent.get(url).header("Authorization", &auth).header(PROTOCOL_HEADER, &proto).call(),
        };
        let mut res = res.map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res.body_mut().with_config().limit(MAX_REPLY).read_to_string().map_err(|e| TransportError::Offline(e.to_string()))?;
        Ok((status, text))
    }

    /// The space's minimum protocol (1 on a server before 0.13).
    pub fn min_protocol(&self) -> Result<u32, TransportError> {
        let (status, text) = self.send("GET", &self.url("/min_protocol"), None)?;
        let min = match status {
            200 => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("min")?.as_u64())
                .map_or(1, |m| u32::try_from(m).unwrap_or(1)),
            404 | 405 if !text.contains("vault") => 1,
            _ => return Err(Self::fail(status, &text)),
        };
        // Lower than it said before: a server (or someone in front of it)
        // lowering the vault; nothing is sealed the old way.
        if min < self.min.load(Ordering::Relaxed) {
            return Err(TransportError::Refused(crate::folder::LOWERED.into()));
        }
        self.min.store(min, Ordering::Relaxed);
        Ok(min)
    }

    /// The vault's key records (0.13).
    pub fn key_records(&self) -> Result<Vec<EpochRecord>, TransportError> {
        let v: Vec<serde_json::Value> = self.get_json(&self.agent, &self.url("/keys"))?;
        Ok(v.into_iter().filter_map(|r| serde_json::from_value::<EpochRecord>(r).ok()).filter(|r| r.space == self.space).collect())
    }

    /// Add the next key record; refused if another device's came first.
    pub fn put_key_record(&self, r: &EpochRecord) -> Result<(), TransportError> {
        let body = serde_json::to_string(r).unwrap_or_default();
        let (status, text) = self.send("PUT", &self.url(&format!("/keys/{}", r.epoch)), Some(&body))?;
        match status {
            200 | 201 | 204 => Ok(()),
            409 => Err(TransportError::Refused("Another device changed the vault's key at the same moment; try again.".into())),
            _ => Err(Self::fail(status, &text)),
        }
    }

    /// The space's devices (0.13).
    pub fn device_rows(&self) -> Result<Vec<DeviceRow>, TransportError> {
        self.get_json(&self.agent, &self.url("/devices"))
    }

    /// Publish this device's record (only its own device token may).
    pub fn put_device_record(&self, r: &DeviceRecord) -> Result<(), TransportError> {
        let body = serde_json::to_string(r).unwrap_or_default();
        let (status, text) = self.send("PUT", &self.url(&format!("/devices/{}/record", r.device)), Some(&body))?;
        if matches!(status, 200 | 204) { Ok(()) } else { Err(Self::fail(status, &text)) }
    }

    /// Stop the server taking `device`'s token.
    pub fn remove_device(&self, device: &DeviceId) -> Result<(), TransportError> {
        let (status, text) = self.send("DELETE", &self.url(&format!("/devices/{device}")), None)?;
        if matches!(status, 200 | 204) { Ok(()) } else { Err(Self::fail(status, &text)) }
    }

    /// Ignore what removed devices wrote after their cut.
    pub fn set_removed(&self, removed: BTreeMap<DeviceId, LanePos>) {
        *self.removed.lock().unwrap_or_else(PoisonError::into_inner) = removed;
    }

    /// Act as a client of protocol `v` (tests of mixed fleets).
    #[must_use]
    pub const fn with_protocol(mut self, v: u32) -> Self {
        self.protocol = v;
        self
    }

    /// `GET /v1/info`: what the server speaks. A 404 is a 0.8 server (protocol 1).
    pub fn negotiate(base: &str) -> Result<Negotiated, TransportError> {
        let a = agent(Duration::from_secs(20));
        let mut res =
            a.get(format!("{}/v1/info", base.trim_end_matches('/'))).call().map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        let body = res.body_mut().with_config().limit(64 * 1024).read_to_string().map_err(|e| TransportError::Offline(e.to_string()))?;
        if status == 404 {
            return Ok(Negotiated::Version(1));
        }
        if status != 200 {
            return Err(Self::fail(status, &body));
        }
        let info: ServerInfo = serde_json::from_str(&body).map_err(|e| TransportError::Refused(format!("unexpected reply: {e}")))?;
        if info.protocol.min > PROTOCOL {
            return Ok(Negotiated::TooOld { min: info.protocol.min });
        }
        Ok(Negotiated::Version(info.protocol.max.clamp(MIN_PROTOCOL, PROTOCOL)))
    }

    /// Keep clients older than protocol `min` out of this space (they get
    /// `426`). Needs a 0.9 server.
    pub fn set_min_protocol(&self, min: u32) -> Result<(), TransportError> {
        let body = serde_json::json!({ "min": min }).to_string();
        let mut res = self
            .agent
            .put(self.url("/min_protocol"))
            .header("Authorization", &self.auth())
            .header(PROTOCOL_HEADER, &self.protocol.to_string())
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        match status {
            200 | 204 => Ok(()),
            404 if !text.contains("vault") => Err(TransportError::Refused("Update your sync server first.".into())),
            _ => Err(Self::fail(status, &text)),
        }
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
            426 => {
                let min = serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| v.get("min")?.as_u64()).unwrap_or(0);
                TransportError::Refused(if min >= 3 {
                    "This vault now needs Quire 0.13 or later on every device; update Quire.".into()
                } else if min >= 2 {
                    "This vault now needs Quire 0.9 or later on every device; update Quire.".into()
                } else {
                    format!("This server needs a newer Quire: {msg}")
                })
            }
            _ => TransportError::Refused(msg),
        }
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, agent: &ureq::Agent, url: &str) -> Result<T, TransportError> {
        let mut res = agent
            .get(url)
            .header("Authorization", &self.auth())
            .header(PROTOCOL_HEADER, &self.protocol.to_string())
            .call()
            .map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        let body = res.body_mut().with_config().limit(MAX_REPLY).read_to_string().map_err(|e| TransportError::Offline(e.to_string()))?;
        if status != 200 {
            return Err(Self::fail(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| TransportError::Refused(format!("unexpected reply: {e}")))
    }

    /// The spaces on a server, oldest first.
    pub fn list_spaces(base: &str, token: &str) -> Result<Vec<SpaceInfo>, TransportError> {
        let t = Self::new(base, SpaceId(String::new()), token);
        t.get_json(&t.agent, &format!("{}/v1/spaces", t.base))
    }

    /// Put a new space and its wrapped key on the server.
    pub fn create_space(&self, key: &WrappedKey) -> Result<(), TransportError> {
        let body = serde_json::to_string(&CreateSpace { space: &self.space, wrapped_key: key }).unwrap_or_default();
        let mut res = self
            .agent
            .post(format!("{}/v1/spaces", self.base))
            .header("Authorization", &self.auth())
            .header(PROTOCOL_HEADER, &self.protocol.to_string())
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
    fn sealing(&self) -> Sealing {
        let mut min = self.min.load(Ordering::Relaxed);
        if min == 0 {
            min = self.min_protocol().unwrap_or(1);
        }
        if min >= HARDENED_PROTOCOL { Sealing::Epoch } else { Sealing::Plain }
    }

    fn history(&self, file: &crate::protocol::FileId) -> Result<Vec<Revision>, TransportError> {
        match self.get_json::<Changes>(&self.agent, &self.url(&format!("/files/{file}"))) {
            Ok(c) => Ok(c.revisions),
            // Servers before 0.7 don't have this endpoint.
            Err(TransportError::Refused(m)) if m.contains("isn't on the server") || m.contains("HTTP 404") => {
                Err(TransportError::Refused("this sync server is too old to show history; update it".into()))
            }
            Err(e) => Err(e),
        }
    }

    fn pull(&self, after: u64) -> Result<Vec<Revision>, TransportError> {
        let mut out = Vec::new();
        let mut cursor = after;
        loop {
            let page: Changes = self.get_json(&self.agent, &self.url(&format!("/changes?after={cursor}&limit={PAGE}")))?;
            let n = page.revisions.len();
            if let Some(last) = page.revisions.last() {
                cursor = last.seq;
            }
            let removed = self.removed.lock().unwrap_or_else(PoisonError::into_inner).clone();
            out.extend(page.revisions.into_iter().filter(|r| removed.get(&r.device).is_none_or(|p| r.seq <= p.n)));
            // Pages can end early (big revisions), so keep going until the log's end.
            if n == 0 || cursor >= page.latest {
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
            .header(PROTOCOL_HEADER, &self.protocol.to_string())
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|e| TransportError::Offline(e.to_string()))?;
        let status = res.status().as_u16();
        // A conflict reply carries the head revision, which can be an attachment.
        let text = res.body_mut().with_config().limit(MAX_REPLY).read_to_string().map_err(|e| TransportError::Offline(e.to_string()))?;
        match status {
            200 | 409 => serde_json::from_str(&text).map_err(|e| TransportError::Refused(format!("unexpected reply: {e}"))),
            _ => Err(Self::fail(status, &text)),
        }
    }
}
