// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Batch push (0.14, `docs/design/0.14-sync-speed.md` §3.4): the store's
//! one-transaction batch, the route's refusals, and real engines over HTTP
//! in every pairing of 0.13 and 0.14 clients and servers, behind proxies
//! that refuse or cut large bodies or lose a reply.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds fixtures with std directly"
)]

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axum::Router;
use quire_sync::protocol::{BatchRequest, BatchResponse, WireOutcome};
use quire_sync::{
    DeviceId, Engine, FileId, HttpTransport, KdfParams, LocalStore, MemoryStore, RevId, Revision, ServerInfo, SpaceId, SyncState, VaultKey,
    WrappedKey,
};
use quire_sync_server::{AppState, Store, app, app_0_13};

const TOKEN: &str = "test-token-0123456789abcdef";
const TODAY: &str = "2026-11-20";

/// Serve `router` on a free port; `pushes` counts the requests that push
/// revisions (one or a batch).
fn serve_router(router: Router, pushes: Arc<AtomicUsize>) -> String {
    let router = router.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
        if req.method() == axum::http::Method::POST && req.uri().path().contains("/revisions") {
            pushes.fetch_add(1, Ordering::Relaxed);
        }
        next.run(req)
    }));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, router).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

/// A 0.14 server (`batches`) or a 0.13 one, and its push counter.
fn serve(batches: bool) -> (String, Arc<AtomicUsize>, Arc<AppState>) {
    let st = Arc::new(AppState::new(Store::in_memory().unwrap(), TOKEN));
    let pushes = Arc::new(AtomicUsize::new(0));
    let router = if batches { app(Arc::clone(&st)) } else { app_0_13(Arc::clone(&st)) };
    (serve_router(router, Arc::clone(&pushes)), pushes, st)
}

fn space(base: &str) -> (SpaceId, VaultKey) {
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    HttpTransport::new(base, space.clone(), TOKEN).create_space(&WrappedKey::wrap(&key, "p", KdfParams::TEST).unwrap()).unwrap();
    (space, key)
}

struct Dev {
    engine: Engine,
    local: MemoryStore,
    t: HttpTransport,
}

impl Dev {
    /// A 0.14 device, or (`old`) one that never batches, as Quire 0.13.
    fn new(base: &str, space: &SpaceId, key: &VaultKey, name: &str, old: bool) -> Self {
        Self::chunked(base, space, key, name, old, quire_sync::engine::CHUNK)
    }

    /// One that sends at most `chunk` revisions (besides deletes) a pass.
    fn chunked(base: &str, space: &SpaceId, key: &VaultKey, name: &str, old: bool, chunk: usize) -> Self {
        let t = HttpTransport::new(base, space.clone(), TOKEN);
        Self {
            engine: Engine::new(space.clone(), DeviceId::random().unwrap(), name, key.keys(), SyncState::default()).with_chunk(chunk),
            local: MemoryStore::default(),
            t: if old { t.without_batches() } else { t },
        }
    }

    fn sync(&mut self) -> quire_sync::Report {
        self.engine.sync(&mut self.local, &self.t, TODAY).unwrap()
    }

    /// Passes until nothing is left to send.
    fn sync_all(&mut self) {
        while self.sync().more_to_send() {}
    }
}

fn notes(d: &mut Dev, n: usize, size: usize) {
    for i in 0..n {
        d.local.write(&format!("Note {i:04}.md"), &format!("{i}\n{}\n", "x".repeat(size))).unwrap();
    }
}

#[test]
fn a_first_sync_takes_a_request_per_thousand_notes() {
    let (base, pushes, st) = serve(true);
    let (space, key) = space(&base);
    let mut a = Dev::new(&base, &space, &key, "a", false);
    notes(&mut a, 2500, 20);
    a.sync_all();
    assert!(a.t.sends_batches());
    let n = pushes.load(Ordering::Relaxed);
    assert!(n <= 2500usize.div_ceil(1000) + 10, "{n} requests");
    // Its own revisions aren't fetched again: the cursor passed them.
    assert_eq!(a.engine.state.cursor, st.store().latest(&space).unwrap());
    let mut b = Dev::new(&base, &space, &key, "b", false);
    b.sync_all();
    assert_eq!(b.local, a.local);
}

#[test]
fn a_0_14_device_on_a_0_13_server_pushes_one_at_a_time() {
    let (base, pushes, _) = serve(false);
    let (space, key) = space(&base);
    let mut a = Dev::new(&base, &space, &key, "a", false);
    notes(&mut a, 30, 10);
    a.sync_all();
    assert!(!a.t.sends_batches());
    assert_eq!(pushes.load(Ordering::Relaxed), 30);
    let mut b = Dev::new(&base, &space, &key, "b", true);
    b.sync_all();
    assert_eq!(b.local, a.local);
    b.local.write("Note 0003.md", "3\nedited on 0.13\n").unwrap();
    a.local.write("Note 0004.md", "4\nedited on 0.14\n").unwrap();
    b.sync_all();
    a.sync_all();
    b.sync_all();
    assert_eq!(b.local, a.local);
}

#[test]
fn devices_of_0_13_and_0_14_share_a_0_14_server() {
    let (base, _, _) = serve(true);
    let (space, key) = space(&base);
    let mut new = Dev::new(&base, &space, &key, "new", false);
    let mut old = Dev::new(&base, &space, &key, "old", true);
    notes(&mut new, 40, 10);
    new.sync_all();
    old.sync_all();
    assert_eq!(old.local, new.local);
    old.local.write("From 0.13.md", "old\n").unwrap();
    old.local.write("Note 0001.md", "1\nedited on 0.13\n").unwrap();
    new.local.write("Note 0002.md", "2\nedited on 0.14\n").unwrap();
    new.local.write("Note 0001.md", "1\nxxxxxxxxxx\nand on 0.14\n").unwrap();
    old.sync_all();
    new.sync_all();
    old.sync_all();
    assert_eq!(old.local, new.local);
    assert_eq!(new.local.read("From 0.13.md").as_deref(), Some("old\n"));
    assert!(!old.t.sends_batches() && new.t.sends_batches());
}

// ------------------------------------------------------------------ proxies

#[derive(Clone, Copy, PartialEq, Eq)]
enum Proxy {
    /// Answers `413` to a body over this many bytes.
    Refuses(usize),
    /// Cuts the connection on a body over this many bytes.
    Cuts(usize),
    /// Cuts the connection on the first batch only (a flaky network).
    CutsTheFirstBatch,
    /// Passes the first batch on, then cuts the connection instead of
    /// passing its reply back.
    LosesAReply,
}

const MIB: usize = 1024 * 1024;

/// A proxy in front of `base`, one request a connection.
fn proxy(base: &str, kind: Proxy) -> String {
    let upstream = base.trim_start_matches("http://").to_string();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let lost = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let (upstream, lost) = (upstream.clone(), Arc::clone(&lost));
            std::thread::spawn(move || relay(client, &upstream, kind, &lost));
        }
    });
    format!("http://{addr}")
}

fn relay(client: TcpStream, upstream: &str, kind: Proxy, lost: &AtomicBool) {
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut head = Vec::new();
    let mut first = String::new();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let lower = line.to_ascii_lowercase();
        if first.is_empty() {
            first.clone_from(&line);
        }
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("connection:") {
            continue;
        }
        if line == "\r\n" {
            head.extend_from_slice(b"Connection: close\r\n\r\n");
            break;
        }
        head.extend_from_slice(line.as_bytes());
    }
    let mut client = client;
    let batch = first.contains("/revisions/batch");
    let cut = match kind {
        Proxy::Cuts(limit) => len > limit,
        Proxy::CutsTheFirstBatch => batch && !lost.swap(true, Ordering::Relaxed),
        _ => false,
    };
    if cut {
        // Closed with the body unread: the client sees a reset.
        drop(reader);
        #[expect(clippy::let_underscore_must_use, reason = "the client may have gone already")]
        let _ = client.shutdown(Shutdown::Both);
        return;
    }
    if let Proxy::Refuses(limit) = kind
        && len > limit
    {
        #[expect(clippy::let_underscore_must_use, reason = "the client may have gone already")]
        let _ = client.write_all(b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let mut body = vec![0u8; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let mut up = TcpStream::connect(upstream).unwrap();
    up.write_all(&head).unwrap();
    up.write_all(&body).unwrap();
    let mut answer = Vec::new();
    #[expect(clippy::let_underscore_must_use, reason = "the answer read so far is passed on")]
    let _ = up.read_to_end(&mut answer);
    if kind == Proxy::LosesAReply && batch && !lost.swap(true, Ordering::Relaxed) {
        #[expect(clippy::let_underscore_must_use, reason = "the client may have gone already")]
        let _ = client.shutdown(Shutdown::Both);
        return;
    }
    #[expect(clippy::let_underscore_must_use, reason = "the client may have gone already")]
    let _ = client.write_all(&answer);
}

fn behind_a_proxy(kind: Proxy, most: usize) -> Dev {
    let (base, pushes, _) = serve(true);
    let front = proxy(&base, kind);
    let (space, key) = space(&front);
    let mut a = Dev::new(&front, &space, &key, "a", false);
    // Some 5 MB of sealed notes: a 4 MiB batch is refused, then sent smaller.
    notes(&mut a, 300, 5000);
    a.sync_all();
    assert!(a.t.sends_batches(), "batches go on, smaller");
    let n = pushes.load(Ordering::Relaxed);
    assert!(n < most, "{n} requests: batches, not one a note");
    let mut b = Dev::new(&base, &space, &key, "b", false);
    b.sync_all();
    assert_eq!(b.local, a.local);
    a
}

#[test]
fn a_proxy_refusing_large_bodies_halves_the_batches_for_the_session() {
    let mut a = behind_a_proxy(Proxy::Refuses(MIB), 30);
    assert!(a.t.batch_cap() <= MIB, "{}", a.t.batch_cap());
    a.sync();
    assert!(a.t.batch_cap() <= MIB, "a 413 holds for the session");
}

#[test]
fn a_proxy_cutting_large_bodies_halves_the_batches_for_the_pass() {
    let mut a = behind_a_proxy(Proxy::Cuts(MIB), 30);
    // The next pass without a cut sends 4 MiB batches again.
    a.sync();
    a.sync();
    assert_eq!(a.t.batch_cap(), quire_sync::transport::BATCH_BYTES);
}

#[test]
fn a_proxy_cutting_small_bodies_still_syncs() {
    // Below a note's size times a few: batches shrink to a revision or two.
    behind_a_proxy(Proxy::Cuts(64 * 1024), 400);
}

#[test]
fn a_flaky_cut_on_one_revision_leaves_batches_on() {
    let (base, _, _) = serve(true);
    let front = proxy(&base, Proxy::CutsTheFirstBatch);
    let (space, key) = space(&base);
    let mut a = Dev::new(&front, &space, &key, "a", false);
    // About 2 MiB of request: a batch of one.
    a.local.write("Attachment.md", &"y".repeat(MIB)).unwrap();
    assert!(a.engine.sync(&mut a.local, &a.t, TODAY).is_err(), "an ordinary offline error");
    assert!(a.t.sends_batches(), "batches stay on");
    a.sync_all();
    assert!(a.t.sends_batches());
    let mut b = Dev::new(&base, &space, &key, "b", false);
    b.sync_all();
    assert_eq!(b.local, a.local);
}

#[test]
fn a_note_too_big_for_the_proxy_fails_as_it_did_in_0_13() {
    let (base, _, _) = serve(true);
    let front = proxy(&base, Proxy::Cuts(MIB));
    let (space, key) = space(&base);
    let mut a = Dev::new(&front, &space, &key, "a", false);
    // One note over the proxy's limit fails as it did in 0.13, an ordinary
    // offline error, and batches go on.
    a.local.write("Big.md", &"y".repeat(1536 * 1024)).unwrap();
    assert!(a.engine.sync(&mut a.local, &a.t, TODAY).is_err());
    assert!(a.engine.sync(&mut a.local, &a.t, TODAY).is_err(), "every pass");
    assert!(a.t.sends_batches());
    // Small notes still go.
    a.local.0.clear();
    a.local.write("Small.md", "small\n").unwrap();
    a.sync_all();
    let mut b = Dev::new(&base, &space, &key, "b", false);
    b.sync_all();
    assert_eq!(b.local.read("Small.md").as_deref(), Some("small\n"));
}

#[test]
fn a_batch_whose_reply_was_lost_is_sent_again_without_duplicates() {
    for n in [50, 1] {
        let (base, _, st) = serve(true);
        let front = proxy(&base, Proxy::LosesAReply);
        let (space, key) = space(&base);
        let mut a = Dev::new(&front, &space, &key, "a", false);
        notes(&mut a, n, 10);
        // The batch was stored, but its reply never came: sent again in
        // halves within the pass, or (one revision) the pass fails and the
        // next one sends it again.
        let first = a.engine.sync(&mut a.local, &a.t, TODAY);
        assert_eq!(first.is_err(), n == 1, "{first:?}");
        assert_eq!(st.store().latest(&space).unwrap(), n as u64, "stored whole");
        let r = a.sync();
        assert!(r.conflict_copies.is_empty(), "{r:?}");
        a.sync_all();
        assert_eq!(st.store().latest(&space).unwrap(), n as u64, "nothing stored twice");
        let mut b = Dev::new(&base, &space, &key, "b", false);
        b.sync_all();
        assert_eq!(b.local, a.local);
        assert_eq!(b.local.0.len(), n, "no note twice");
    }
}

// ------------------------------------------------------------------ the store

fn rev(file: &FileId, parents: &[&RevId], device: &DeviceId) -> Revision {
    // The server never opens a revision: any bytes stand in for the sealed text.
    Revision {
        file: file.clone(),
        rev: RevId::random().unwrap(),
        parents: parents.iter().map(|p| (*p).clone()).collect(),
        seq: 0,
        device: device.clone(),
        sealed: vec![0xAB; 64],
        chain: Vec::new(),
    }
}

#[test]
fn a_batch_is_one_transaction_with_an_outcome_per_revision() {
    let store = Store::in_memory().unwrap();
    let space = SpaceId::random().unwrap();
    store.create_space(&space, &WrappedKey::wrap(&VaultKey::generate().unwrap(), "p", KdfParams::TEST).unwrap()).unwrap();
    let d = DeviceId::random().unwrap();
    let (f, g) = (FileId::random().unwrap(), FileId::random().unwrap());
    let a = rev(&f, &[], &d);
    store.push(&space, &a).unwrap();
    let b = rev(&g, &[], &d);
    let b2 = rev(&g, &[&b.rev], &d);
    let stale = rev(&f, &[&RevId::random().unwrap()], &d);
    let lost = rev(&FileId::random().unwrap(), &[&RevId::random().unwrap()], &d);
    let batch = [b, stale, lost, b2];
    let (out, latest) = store.push_many(&space, &batch).unwrap();
    assert_eq!(
        out,
        [WireOutcome::Accepted { seq: 2 }, WireOutcome::Conflict {}, WireOutcome::LostParent {}, WireOutcome::Accepted { seq: 3 }]
    );
    assert_eq!(latest, 3);
    // Sent again (its reply was lost): the same positions, nothing stored twice.
    let (again, latest) = store.push_many(&space, &batch).unwrap();
    assert_eq!(again, out);
    assert_eq!(latest, 3);
    assert_eq!(store.changes(&space, 0, 100).unwrap().len(), 3);
    // An empty batch stores nothing and says where the log is.
    assert_eq!(store.push_many(&space, &[]).unwrap(), (Vec::new(), 3));
}

/// One request, as an older or a misbehaving client sends it: its status
/// and body.
fn raw(base: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(base.trim_start_matches("http://")).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        write!(req, "{k}: {v}\r\n").unwrap();
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    let status = answer.split(' ').nth(1).unwrap().parse().unwrap();
    let body = answer.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

fn post(base: &str, space: &SpaceId, token: &str, protocol: u32, revs: &[Revision]) -> (u16, String) {
    let body = serde_json::to_string(&BatchRequest { revisions: revs.to_vec() }).unwrap();
    let auth = format!("Bearer {token}");
    let proto = protocol.to_string();
    let headers = [("Authorization", auth.as_str()), ("Quire-Sync-Protocol", proto.as_str()), ("Content-Type", "application/json")];
    raw(base, "POST", &format!("/v1/spaces/{space}/revisions/batch"), &headers, &body)
}

#[test]
fn a_bad_batch_is_refused_whole_and_stores_nothing() {
    let (base, _, st) = serve(true);
    let (space, _) = space(&base);
    let me = DeviceId::random().unwrap();
    let good = rev(&FileId::random().unwrap(), &[], &me);
    // A malformed id: 400.
    let mut bad = rev(&FileId::random().unwrap(), &[], &me);
    bad.rev = RevId("not/an id".into());
    assert_eq!(post(&base, &space, TOKEN, 3, &[good.clone(), bad]).0, 400);
    // More than a thousand: 400.
    let many: Vec<Revision> = (0..1001).map(|_| rev(&FileId::random().unwrap(), &[], &me)).collect();
    assert_eq!(post(&base, &space, TOKEN, 3, &many).0, 400);
    assert_eq!(st.store().latest(&space).unwrap(), 0, "nothing stored");
    // A device token writing as another device: 403.
    let token = HttpTransport::enroll(&base, &space, TOKEN, &me).unwrap();
    let other = rev(&FileId::random().unwrap(), &[], &DeviceId::random().unwrap());
    assert_eq!(post(&base, &space, &token, 3, &[good.clone(), other]).0, 403);
    assert_eq!(st.store().latest(&space).unwrap(), 0);
    // A client older than the space's minimum: 426.
    st.store().raise_min_protocol(&space, 3).unwrap();
    assert_eq!(post(&base, &space, &token, 2, std::slice::from_ref(&good)).0, 426);
    assert_eq!(st.store().latest(&space).unwrap(), 0);
    // A wrong token: 401.
    assert_eq!(post(&base, &space, "not-the-token-at-all-xxxxxxxx", 3, std::slice::from_ref(&good)).0, 401);
    let (status, body) = post(&base, &space, &token, 3, std::slice::from_ref(&good));
    assert_eq!(status, 200, "{body}");
    let reply: BatchResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(reply.outcomes, [WireOutcome::Accepted { seq: 1 }]);
    assert_eq!(reply.latest, 1);
}

#[test]
fn info_names_the_batch_feature_and_keeps_the_protocol() {
    let (base, _, _) = serve(true);
    let (status, body) = raw(&base, "GET", "/v1/info", &[], "");
    assert_eq!(status, 200);
    let info: ServerInfo = serde_json::from_str(&body).unwrap();
    assert_eq!(info.features, ["batch-push"]);
    assert_eq!(info.protocol.max, quire_sync::PROTOCOL);
    // A 0.13 server: no features, and no route.
    let (old, _, _) = serve(false);
    let info: ServerInfo = serde_json::from_str(&raw(&old, "GET", "/v1/info", &[], "").1).unwrap();
    assert!(info.features.is_empty());
    assert!(!body.is_empty() && !raw(&old, "GET", "/v1/info", &[], "").1.contains("features"));
    let (space, _) = space(&old);
    assert_eq!(post(&old, &space, TOKEN, 3, &[]).0, 404);
}

// ------------------------------------------------------------------ races

#[test]
fn two_devices_batching_edits_to_the_same_notes_lose_none() {
    let (base, _, _) = serve(true);
    let (space, key) = space(&base);
    let mut seed = Dev::new(&base, &space, &key, "seed", false);
    notes(&mut seed, 50, 5);
    seed.sync_all();
    #[expect(clippy::needless_collect, reason = "both threads start before either is joined")]
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let (base, space, key) = (base.clone(), space.clone(), key.clone());
            std::thread::spawn(move || {
                let mut d = Dev::new(&base, &space, &key, &format!("d{i}"), false);
                d.sync_all();
                for round in 0..3 {
                    for n in 0..50 {
                        let p = format!("Note {n:04}.md");
                        let t = d.local.read(&p).unwrap();
                        d.local.write(&p, &format!("{t}d{i} r{round} n{n}\n")).unwrap();
                    }
                    d.sync_all();
                }
                d
            })
        })
        .collect();
    let mut devs: Vec<Dev> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for _ in 0..3 {
        for d in &mut devs {
            d.sync_all();
        }
        seed.sync_all();
    }
    let all: String = seed.local.0.values().cloned().collect();
    for i in 0..2 {
        for round in 0..3 {
            for n in 0..50 {
                assert!(all.contains(&format!("d{i} r{round} n{n}\n")), "lost d{i} r{round} n{n}");
            }
        }
    }
    assert_eq!(devs[0].local, seed.local);
    assert_eq!(devs[1].local, seed.local);
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }
}

/// Three devices (one of them 0.13) editing, creating and deleting, with
/// passes in between that send in small chunks and batches: every device
/// ends with the same notes and every revision is applied everywhere.
#[test]
fn three_devices_batching_through_the_server_converge() {
    let seeds = std::env::var("SYNC_SEEDS").ok().and_then(|s| s.parse::<u64>().ok()).map_or(25, |n| n.div_ceil(5));
    let (base, _, st) = serve(true);
    for seed in 1..=seeds {
        let (space, key) = space(&base);
        let mut devs: Vec<Dev> = (0..3).map(|i| Dev::chunked(&base, &space, &key, &format!("d{i}"), i == 2, 3)).collect();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for step in 0..60 {
            let i = rng.below(devs.len());
            match rng.below(4) {
                0 => {
                    let d = &mut devs[i];
                    // A pass may stop between batches or chunks: what it sent stands.
                    #[expect(clippy::let_underscore_must_use, reason = "a failed pass is retried by the next one")]
                    let _ = d.engine.sync(&mut d.local, &d.t, TODAY);
                }
                1 if !devs[i].local.0.is_empty() && rng.below(4) == 0 => {
                    let names: Vec<String> = devs[i].local.0.keys().cloned().collect();
                    let p = &names[rng.below(names.len())];
                    devs[i].local.0.remove(p);
                }
                _ => {
                    let p = format!("N{}.md", rng.below(12));
                    let mut lines: Vec<String> =
                        devs[i].local.read(&p).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
                    let at = rng.below(lines.len() + 1);
                    lines.insert(at, format!("w{step}"));
                    devs[i].local.write(&p, &(lines.join("\n") + "\n")).unwrap();
                }
            }
        }
        for _ in 0..10 {
            let mut changed = false;
            for d in &mut devs {
                let r = d.sync();
                changed |= r.changed_anything() || r.more_to_send();
            }
            if !changed {
                break;
            }
        }
        let latest = st.store().latest(&space).unwrap();
        for d in &devs {
            assert_eq!(d.local, devs[0].local, "seed {seed}: devices disagree");
            assert_eq!(d.engine.state.cursor, latest, "seed {seed}: every revision applied");
        }
    }
}
