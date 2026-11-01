// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The real server over HTTP, with real engines on both ends.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quire_sync::{
    DeviceId, Engine, HttpTransport, KdfParams, LocalStore, MemoryStore, SpaceId, SyncState, Transport, TransportError, VaultKey,
    WrappedKey,
};
use quire_sync_server::{AppState, Store, app};

const TOKEN: &str = "test-token-0123456789abcdef";

/// Start a server on a free port; returns its base URL. It runs until the test ends.
fn serve(store: Store) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app(Arc::new(AppState::new(store, TOKEN)))).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

fn device(space: &SpaceId, key: &VaultKey, name: &str) -> (Engine, MemoryStore) {
    (Engine::new(space.clone(), DeviceId::random().unwrap(), name, key.keys(), SyncState::default()), MemoryStore::default())
}

#[test]
fn two_devices_sync_through_the_server() {
    let base = serve(Store::in_memory().unwrap());
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    let http = HttpTransport::new(&base, space.clone(), TOKEN);
    http.create_space(&WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap()).unwrap();
    assert!(http.create_space(&WrappedKey::wrap(&key, "x", KdfParams::TEST).unwrap()).is_err(), "can't create twice");

    // A second device finds the space on the server and joins with the passphrase.
    let listed = HttpTransport::list_spaces(&base, TOKEN).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].space, space);
    let joined = HttpTransport::new(&base, space.clone(), TOKEN).wrapped_key().unwrap().unwrap("pass phrase").unwrap();

    let (mut a, mut sa) = device(&space, &key, "laptop");
    let (mut b, mut sb) = device(&space, &joined, "phone");
    sa.write("Plan.md", "# Plan\none\ntwo\n").unwrap();
    a.sync(&mut sa, &http, "2026-11-01").unwrap();
    b.sync(&mut sb, &http, "2026-11-01").unwrap();
    assert_eq!(sb.read("Plan.md").unwrap(), "# Plan\none\ntwo\n");

    // Concurrent edits to different lines merge across the wire.
    sa.write("Plan.md", "# Plan\nONE\ntwo\n").unwrap();
    sb.write("Plan.md", "# Plan\none\nTWO\n").unwrap();
    a.sync(&mut sa, &http, "2026-11-01").unwrap();
    let r = b.sync(&mut sb, &http, "2026-11-01").unwrap();
    assert_eq!(r.merged, vec!["Plan.md".to_string()]);
    a.sync(&mut sa, &http, "2026-11-01").unwrap();
    assert_eq!(sa, sb);
    assert_eq!(sa.read("Plan.md").unwrap(), "# Plan\nONE\nTWO\n");
}

#[test]
fn wrong_token_and_unknown_space_are_refused() {
    let base = serve(Store::in_memory().unwrap());
    let space = SpaceId::random().unwrap();
    let bad = HttpTransport::new(&base, space.clone(), "not-the-token-at-all-xxxxxxxx");
    let err = bad.pull(0).unwrap_err();
    assert_eq!(err, TransportError::Refused("the server token is wrong".into()));
    let good = HttpTransport::new(&base, space, TOKEN);
    assert_eq!(good.pull(0).unwrap_err(), TransportError::Refused("this vault isn't on the server".into()));
    let offline = HttpTransport::new("http://127.0.0.1:9", SpaceId::random().unwrap(), TOKEN);
    assert!(matches!(offline.pull(0), Err(TransportError::Offline(_))));
}

#[test]
fn wait_wakes_when_another_device_pushes() {
    let base = serve(Store::in_memory().unwrap());
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    let http = HttpTransport::new(&base, space.clone(), TOKEN);
    http.create_space(&WrappedKey::wrap(&key, "p", KdfParams::TEST).unwrap()).unwrap();

    let waiter = HttpTransport::new(&base, space.clone(), TOKEN);
    let started = Instant::now();
    let handle = std::thread::spawn(move || waiter.wait(0, 20).unwrap());
    std::thread::sleep(Duration::from_millis(300));
    let (mut a, mut sa) = device(&space, &key, "laptop");
    sa.write("A.md", "a\n").unwrap();
    a.sync(&mut sa, &http, "2026-11-01").unwrap();
    assert_eq!(handle.join().unwrap(), 1);
    assert!(started.elapsed() < Duration::from_secs(10), "woke on push, not on timeout");
    // Already past: returns at once.
    assert_eq!(http.wait(0, 20).unwrap(), 1);
}

#[test]
fn data_survives_a_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sync.sqlite");
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    {
        let base = serve(Store::open(&path).unwrap());
        let http = HttpTransport::new(&base, space.clone(), TOKEN);
        http.create_space(&WrappedKey::wrap(&key, "p", KdfParams::TEST).unwrap()).unwrap();
        let (mut a, mut sa) = device(&space, &key, "laptop");
        sa.write("A.md", "kept\n").unwrap();
        a.sync(&mut sa, &http, "2026-11-01").unwrap();
    }
    let base = serve(Store::open(&path).unwrap());
    let http = HttpTransport::new(&base, space.clone(), TOKEN);
    let (mut b, mut sb) = device(&space, &key, "phone");
    b.sync(&mut sb, &http, "2026-11-01").unwrap();
    assert_eq!(sb.read("A.md").unwrap(), "kept\n");
    // The database holds no readable names or text.
    let raw = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("kept") && !String::from_utf8_lossy(&raw).contains("A.md"));
}

#[test]
fn concurrent_pushes_keep_one_linear_history_per_note() {
    let base = serve(Store::in_memory().unwrap());
    let space = SpaceId::random().unwrap();
    let key = VaultKey::generate().unwrap();
    let http = HttpTransport::new(&base, space.clone(), TOKEN);
    http.create_space(&WrappedKey::wrap(&key, "p", KdfParams::TEST).unwrap()).unwrap();
    let (mut seed, mut s0) = device(&space, &key, "seed");
    s0.write("Shared.md", "start\n").unwrap();
    seed.sync(&mut s0, &http, "2026-11-01").unwrap();

    // Eight devices each add a line at the same time, many times over.
    #[expect(clippy::needless_collect, reason = "spawn every thread before joining any, so they run at once")]
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let (base, space, key) = (base.clone(), space.clone(), key.clone());
            std::thread::spawn(move || {
                let http = HttpTransport::new(&base, space.clone(), TOKEN);
                let (mut e, mut s) = device(&space, &key, &format!("d{i}"));
                e.sync(&mut s, &http, "2026-11-01").unwrap();
                for n in 0..5 {
                    let mut t = s.read("Shared.md").unwrap();
                    writeln!(t, "d{i}-{n}").unwrap();
                    s.write("Shared.md", &t).unwrap();
                    e.sync(&mut s, &http, "2026-11-01").unwrap();
                }
                (e, s)
            })
        })
        .collect();
    let mut devs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for (e, s) in &mut devs {
        e.sync(s, &http, "2026-11-01").unwrap();
    }
    seed.sync(&mut s0, &http, "2026-11-01").unwrap();
    let all: String = s0.0.values().cloned().collect();
    for i in 0..8 {
        for n in 0..5 {
            assert!(all.contains(&format!("d{i}-{n}")), "lost d{i}-{n}");
        }
    }
    for (_, s) in &devs {
        assert_eq!(*s, s0);
    }
}
