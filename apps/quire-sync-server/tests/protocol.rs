// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sync protocol versions across a mixed fleet (design §B.5): a 0.9 client
//! with a 0.8 server and the reverse, per-space minimums, and agent
//! attribution travelling with revisions.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::BTreeMap;
use std::sync::Arc;

use quire_sync::{
    ActorRef, Content, DeviceId, Engine, HttpTransport, KdfParams, LocalStore, MemoryStore, Negotiated, SpaceId, SyncState, TransportError,
    VaultKey, WrappedKey,
};
use quire_sync_server::{AppState, Store, app, app_0_8};

const TOKEN: &str = "test-token-0123456789abcdef";

fn serve(old: bool) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let st = Arc::new(AppState::new(Store::in_memory().unwrap(), TOKEN));
            let router = if old { app_0_8(st) } else { app(st) };
            axum::serve(listener, router).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

fn space(base: &str) -> (SpaceId, VaultKey) {
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    HttpTransport::new(base, space.clone(), TOKEN).create_space(&WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap()).unwrap();
    (space, key)
}

/// A raw GET (as Quire 0.8 sends it: no protocol header). Returns (status, body).
fn raw_get(base: &str, path: &str, token: Option<&str>) -> (u16, String) {
    use std::io::{Read, Write};
    let addr = base.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

fn engine(space: &SpaceId, key: &VaultKey, name: &str) -> Engine {
    Engine::new(space.clone(), DeviceId::random().unwrap(), name, key.keys(), SyncState::default())
}

#[test]
fn info_needs_no_token_and_names_the_range() {
    let base = serve(false);
    let (status, body) = raw_get(&base, "/v1/info", None);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["protocol"], serde_json::json!({"min": 1, "max": 2}));
    assert_eq!(v["server"], env!("CARGO_PKG_VERSION"));
    assert_eq!(HttpTransport::negotiate(&base).unwrap(), Negotiated::Version(2));
    assert_eq!(Negotiated::Version(2).notice(), None);
}

#[test]
fn a_0_9_client_syncs_with_a_0_8_server() {
    let base = serve(true);
    assert_eq!(HttpTransport::negotiate(&base).unwrap(), Negotiated::Version(1), "no /v1/info: protocol 1");
    assert!(Negotiated::Version(1).notice().unwrap().contains("older"));
    let (space, key) = space(&base);
    let http = HttpTransport::new(&base, space.clone(), TOKEN);
    let (mut a, mut b) = (engine(&space, &key, "laptop"), engine(&space, &key, "phone"));
    let (mut sa, mut sb) = (MemoryStore::default(), MemoryStore::default());
    sa.write("Plan.md", "# Plan\n").unwrap();
    a.sync(&mut sa, &http, "2026-11-02").unwrap();
    b.sync(&mut sb, &http, "2026-11-02").unwrap();
    assert_eq!(sb.read("Plan.md").unwrap(), "# Plan\n");
    let err = http.set_min_protocol(2).unwrap_err().to_string();
    assert!(err.contains("Update your sync server first"), "{err}");
}

#[test]
fn a_0_8_client_syncs_until_the_space_needs_0_9() {
    let base = serve(false);
    let (space, key) = space(&base);
    let new = HttpTransport::new(&base, space.clone(), TOKEN);
    let old = HttpTransport::new(&base, space.clone(), TOKEN).with_protocol(1);
    let (mut a, mut b) = (engine(&space, &key, "new"), engine(&space, &key, "old"));
    let (mut sa, mut sb) = (MemoryStore::default(), MemoryStore::default());
    sa.write("A.md", "from 0.9\n").unwrap();
    a.sync(&mut sa, &new, "2026-11-02").unwrap();
    b.sync(&mut sb, &old, "2026-11-02").unwrap();
    assert_eq!(sb.read("A.md").unwrap(), "from 0.9\n", "mixed fleets keep syncing");
    // Turning on vault-settings sync raises the minimum; 0.8 is then refused.
    new.set_min_protocol(2).unwrap();
    new.set_min_protocol(1).unwrap();
    sb.write("B.md", "from 0.8\n").unwrap();
    let err = b.sync(&mut sb, &old, "2026-11-02").unwrap_err().to_string();
    assert!(err.contains("needs Quire 0.9"), "{err}");
    // A real 0.8 client sends no header at all.
    let (status, body) = raw_get(&base, &format!("/v1/spaces/{space}/changes?after=0"), Some(TOKEN));
    assert_eq!(status, 426);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["min"], 2);
    a.sync(&mut sa, &new, "2026-11-02").unwrap();
    assert!(matches!(old.wrapped_key(), Err(TransportError::Refused(_))), "0.8 can't even join");
}

/// A store whose files say who wrote them.
#[derive(Default)]
struct Attributed {
    files: MemoryStore,
    by: BTreeMap<String, ActorRef>,
    received: Vec<(String, Option<ActorRef>)>,
}

impl LocalStore for Attributed {
    fn list(&self) -> Vec<String> {
        self.files.list()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.files.read(path)
    }
    fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
        self.files.write(path, text)
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.files.remove(path)
    }
    fn actor(&self, path: &str) -> Option<ActorRef> {
        self.by.get(path).cloned()
    }
    fn incoming(&mut self, content: &Content) {
        self.received.push((content.path.clone(), content.actor.clone()));
    }
}

#[test]
fn agent_attribution_converges_across_devices() {
    let base = serve(false);
    let (space, key) = space(&base);
    let http = HttpTransport::new(&base, space.clone(), TOKEN);
    let (mut a, mut b) = (engine(&space, &key, "laptop"), engine(&space, &key, "desktop"));
    let mut sa = Attributed::default();
    let mut sb = Attributed::default();
    let claude = ActorRef { id: "mcp:claude-code".into(), label: "Claude Code".into(), kind: "agent".into() };
    sa.files.write("wiki/Ropes.md", "# Ropes\n").unwrap();
    sa.by.insert("wiki/Ropes.md".into(), claude.clone());
    sa.files.write("Mine.md", "# Mine\n").unwrap();
    a.sync(&mut sa, &http, "2026-11-02").unwrap();
    b.sync(&mut sb, &http, "2026-11-02").unwrap();
    assert_eq!(sb.read("wiki/Ropes.md").unwrap(), "# Ropes\n");
    let got: BTreeMap<String, Option<ActorRef>> = sb.received.iter().cloned().collect();
    assert_eq!(got["wiki/Ropes.md"], Some(claude));
    assert_eq!(got["Mine.md"], None);
    // Concurrent edits still merge with actors present.
    sa.files.write("wiki/Ropes.md", "# Ropes\nfrom the agent\n").unwrap();
    sb.files.write("wiki/Ropes.md", "# Ropes v2\n").unwrap();
    a.sync(&mut sa, &http, "2026-11-02").unwrap();
    b.sync(&mut sb, &http, "2026-11-02").unwrap();
    a.sync(&mut sa, &http, "2026-11-02").unwrap();
    assert_eq!(sa.read("wiki/Ropes.md"), sb.read("wiki/Ropes.md"));
    assert_eq!(sa.read("wiki/Ropes.md").unwrap(), "# Ropes v2\nfrom the agent\n");
}
