// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sync through S3-compatible storage and WebDAV, over real HTTP to the
//! in-process servers of `quire_sync::remote::mock`: two devices syncing at
//! the same moment, conflicts, cut uploads, wrong secrets, and hostile
//! servers. No network needed.
//!
//! `real_servers` runs the same two-device check against moto (S3) and
//! `WsgiDAV` (WebDAV) when `QUIRE_SYNC_REAL_SERVERS` names a Python that has
//! them (see `docs/design/0.12-sync.md`).
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use quire_sync::remote::mock::{Hostile, Kind, MockServer};
use quire_sync::remote::net::Retry;
use quire_sync::remote::{
    self, ObjectStore, RemoteTransport, S3Config, S3Credentials, S3Store, StoreError, WebDavConfig, WebDavCredentials, WebDavStore,
};
use quire_sync::{DeviceId, Engine, KdfParams, MemoryStore, SpaceId, SyncError, SyncState, TransportError, VaultKey, WrappedKey};
use zeroize::Zeroizing;

const TODAY: &str = "2026-11-10";

fn s3_server() -> MockServer {
    MockServer::start(Kind::S3 {
        bucket: "notes".into(),
        access_key: "AKIDQUIRE".into(),
        secret_key: "s3cr3t/key+".into(),
        region: "eu-central-1".into(),
    })
}

fn dav_server() -> MockServer {
    MockServer::start(Kind::WebDav { user: "asha".into(), password: "app pass".into() })
}

fn s3_with(srv: &MockServer, secret: &str, region: &str) -> S3Store {
    let cfg = S3Config {
        endpoint: srv.origin(),
        region: region.into(),
        bucket: "notes".into(),
        prefix: "/my vault/".into(),
        virtual_hosted: false,
        allow_http: false,
    };
    let creds = S3Credentials { access_key: "AKIDQUIRE".into(), secret_key: Zeroizing::new(secret.into()), session_token: None };
    S3Store::new(cfg, creds).unwrap().with_retry(Retry::QUICK)
}

fn s3(srv: &MockServer) -> Arc<dyn ObjectStore> {
    Arc::new(s3_with(srv, "s3cr3t/key+", "eu-central-1"))
}

fn dav_with(srv: &MockServer, password: &str) -> WebDavStore {
    let cfg = WebDavConfig { url: srv.dav_url(), allow_http: false };
    WebDavStore::new(&cfg, &WebDavCredentials { user: "asha".into(), password: Zeroizing::new(password.into()) })
        .unwrap()
        .with_retry(Retry::QUICK)
}

fn dav(srv: &MockServer) -> Arc<dyn ObjectStore> {
    Arc::new(dav_with(srv, "app pass"))
}

struct Dev {
    name: String,
    engine: Engine,
    local: MemoryStore,
    t: RemoteTransport,
}

impl Dev {
    /// One pass and its upload, as the app runs them. Busy rounds retry.
    fn pass(&mut self) -> Result<quire_sync::Report, SyncError> {
        let r = self.engine.sync(&mut self.local, &self.t, TODAY);
        let f = self.t.flush();
        let r = r?;
        f?;
        Ok(r)
    }
}

fn devices(store: &Arc<dyn ObjectStore>, n: usize) -> (SpaceId, Vec<Dev>) {
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    remote::create_space(&**store, &space, &WrappedKey::wrap(&key, "pass phrase", KdfParams::TEST).unwrap()).unwrap();
    let devs = (0..n)
        .map(|i| {
            let id = DeviceId::random().unwrap();
            Dev {
                name: format!("dev{i}"),
                engine: Engine::new(space.clone(), id.clone(), format!("dev{i}"), key.keys(), SyncState::default()),
                local: MemoryStore::default(),
                t: RemoteTransport::new(Arc::clone(store), &space, id, None).unwrap(),
            }
        })
        .collect();
    (space, devs)
}

/// Every line in every note and conflict copy on a device.
fn lines(d: &Dev) -> BTreeSet<String> {
    d.local.0.values().flat_map(|t| t.lines().map(str::to_string)).collect()
}

/// Two devices edit notes and sync at the very same moment, round after
/// round, then settle. Both end up with the same files, and every line
/// either of them wrote is in a note or a conflict copy.
fn concurrent_rounds(store: &Arc<dyn ObjectStore>, rounds: usize) -> Vec<Dev> {
    let (_, mut devs) = devices(store, 2);
    let mut written = BTreeSet::new();
    for round in 0..rounds {
        for (i, d) in devs.iter_mut().enumerate() {
            // A note of its own, and a shared one both append to.
            let own = format!("{}/r{round}.md", d.name);
            let line = format!("{} round {round} own", d.name);
            d.local.0.insert(own, format!("{line}\n"));
            written.insert(line);
            let shared = d.local.0.entry(format!("shared{}.md", round % 3)).or_default();
            let line = format!("{} round {round} shared {i}", d.name);
            shared.push_str(&line);
            shared.push('\n');
            written.insert(line);
        }
        let [a, b] = &mut devs[..] else { unreachable!() };
        std::thread::scope(|s| {
            let ha = s.spawn(|| a.pass());
            let hb = s.spawn(|| b.pass());
            for r in [ha.join().unwrap(), hb.join().unwrap()] {
                assert!(matches!(r, Ok(_) | Err(SyncError::Busy)), "{r:?}");
            }
        });
    }
    // Settle: a few quiet passes each.
    for _ in 0..4 {
        for d in &mut devs {
            d.pass().unwrap();
        }
    }
    assert_eq!(devs[0].local.0, devs[1].local.0, "both devices have the same files");
    let have = lines(&devs[0]);
    let lost: Vec<&String> = written.iter().filter(|l| !have.contains(*l)).collect();
    assert!(lost.is_empty(), "lost lines: {lost:?}");
    devs
}

/// What the storage holds: only ids, numbers and ciphertext.
fn assert_opaque(objects: &std::collections::BTreeMap<String, Vec<u8>>) {
    for (k, v) in objects {
        assert!(!k.contains("round") && !k.contains("shared") && !k.contains(".md"), "{k}");
        let text = String::from_utf8_lossy(v);
        assert!(!text.contains("round") && !text.contains("dev0/"), "{k} holds plain text");
    }
}

#[test]
fn s3_two_devices_at_once_lose_nothing() {
    let srv = s3_server();
    srv.state.page_size.store(3, Ordering::Relaxed); // listings take several pages
    let store = s3(&srv);
    concurrent_rounds(&store, 12);
    let objects = srv.objects();
    assert!(objects.keys().all(|k| k.starts_with("my vault/quire-sync/")), "{:?}", objects.keys().next());
    assert_opaque(&objects);
}

#[test]
fn webdav_two_devices_at_once_lose_nothing() {
    let srv = dav_server();
    let store = dav(&srv);
    concurrent_rounds(&store, 12);
    assert_opaque(&srv.objects());
}

/// Backblaze B2 answers `501 NotImplemented` to `If-None-Match: *`; WebDAV
/// servers without it ignore the header. Neither changes the outcome.
#[test]
fn storage_without_conditional_writes_loses_nothing() {
    for (srv, is_s3) in [(s3_server(), true), (dav_server(), false)] {
        srv.state.conditional.store(false, Ordering::Relaxed);
        let store = if is_s3 { s3(&srv) } else { dav(&srv) };
        let p = remote::probe(&*store).unwrap();
        assert!(!p.conditional_writes);
        concurrent_rounds(&store, 6);
    }
}

#[test]
fn overlapping_edits_make_the_usual_conflict_copy() {
    let srv = s3_server();
    let store = s3(&srv);
    let (_, mut devs) = devices(&store, 2);
    devs[0].local.0.insert("Plan.md".into(), "# Plan\nship friday\n".into());
    devs[0].pass().unwrap();
    devs[1].pass().unwrap();
    assert_eq!(devs[1].local.0["Plan.md"], "# Plan\nship friday\n");
    // Both change the same line before seeing each other's change.
    devs[0].local.0.insert("Plan.md".into(), "# Plan\nship monday\n".into());
    devs[1].local.0.insert("Plan.md".into(), "# Plan\nship tuesday\n".into());
    devs[0].pass().unwrap();
    let r = devs[1].pass().unwrap();
    assert_eq!(r.conflict_copies.len(), 1, "{r:?}");
    for _ in 0..2 {
        for d in &mut devs {
            d.pass().unwrap();
        }
    }
    assert_eq!(devs[0].local.0, devs[1].local.0);
    let all: String = devs[0].local.0.values().cloned().collect();
    assert!(all.contains("ship monday") && all.contains("ship tuesday"));
    assert!(devs[0].local.0.keys().any(|k| k.contains("conflict")), "{:?}", devs[0].local.0.keys());
}

#[test]
fn an_upload_cut_short_resumes_on_the_next_pass() {
    let srv = dav_server();
    let store = dav(&srv);
    let (_, mut devs) = devices(&store, 2);
    // The folders exist after a first note.
    devs[0].local.0.insert("first.md".into(), "first\n".into());
    devs[0].pass().unwrap();
    for i in 0..20 {
        devs[0].local.0.insert(format!("n{i}.md"), format!("note {i}\n"));
    }
    let d = &mut devs[0];
    d.engine.sync(&mut d.local, &d.t, TODAY).unwrap();
    // The connection drops after 7 uploads, and keeps dropping.
    srv.state.puts_left.store(7, Ordering::Relaxed);
    assert!(matches!(devs[0].t.flush(), Err(TransportError::Offline(_))));
    assert!(devs[0].t.pending().is_some_and(|p| p.contains("not uploaded yet")));
    let uploaded = srv.objects().keys().filter(|k| std::path::Path::new(k).extension().is_some_and(|e| e == "rev")).count();
    assert_eq!(uploaded, 1 + 7);
    // A pass while it's down: offline, nothing lost, nothing uploaded twice.
    assert!(devs[0].pass().is_err());
    srv.state.puts_left.store(u64::MAX, Ordering::Relaxed);
    devs[0].pass().unwrap();
    assert_eq!(srv.objects().keys().filter(|k| std::path::Path::new(k).extension().is_some_and(|e| e == "rev")).count(), 21);
    assert!(devs[0].t.pending().is_none());
    devs[1].pass().unwrap();
    assert_eq!(devs[1].local.0.len(), 21);
    assert_eq!(devs[0].local.0, devs[1].local.0);
    // Busy servers (503) are retried with backoff, invisibly.
    srv.fail_next(&[503, 500]);
    devs[1].local.0.insert("late.md".into(), "late\n".into());
    devs[1].pass().unwrap();
    devs[0].pass().unwrap();
    assert!(devs[0].local.0.contains_key("late.md"));
}

#[test]
fn a_wrong_passphrase_is_refused_and_the_recovery_key_works() {
    let srv = s3_server();
    let store = s3(&srv);
    let key = VaultKey::generate().unwrap();
    let space = SpaceId::random().unwrap();
    remote::create_space(&*store, &space, &WrappedKey::wrap(&key, "correct horse", KdfParams::TEST).unwrap()).unwrap();
    let listed = remote::list_spaces(&*store).unwrap();
    assert_eq!(listed.iter().map(|s| &s.space).collect::<Vec<_>>(), [&space]);
    let wrapped = remote::wrapped_key(&*store, &space).unwrap();
    assert!(wrapped.unwrap("battery staple").is_err());
    assert_eq!(wrapped.unwrap("correct horse").unwrap().recovery_key().to_string(), key.recovery_key().to_string());
    // A device with the wrong key can't read anything (and says so).
    let (_, mut devs) = devices(&store, 1);
    devs[0].local.0.insert("a.md".into(), "a\n".into());
    devs[0].pass().unwrap();
    let space2 = devs[0].engine.space.clone();
    let mut wrong = Dev {
        name: "wrong".into(),
        engine: Engine::new(
            space2.clone(),
            DeviceId::random().unwrap(),
            "wrong",
            VaultKey::generate().unwrap().keys(),
            SyncState::default(),
        ),
        local: MemoryStore::default(),
        t: RemoteTransport::new(Arc::clone(&store), &space2, DeviceId::random().unwrap(), None).unwrap(),
    };
    assert!(matches!(wrong.pass(), Err(SyncError::Crypto(_))));
    assert!(wrong.local.0.is_empty());
}

#[test]
fn credentials_clock_and_region_errors_say_what_to_fix() {
    let srv = s3_server();
    let good = s3(&srv);
    assert!(remote::probe(&*good).unwrap().conditional_writes);
    let bad = s3_with(&srv, "wrong", "eu-central-1");
    assert!(matches!(remote::probe(&bad), Err(StoreError::Auth(m)) if m.contains("secret access key is wrong")));
    let region = s3_with(&srv, "s3cr3t/key+", "us-east-1");
    assert!(matches!(remote::probe(&region), Err(StoreError::Config(m)) if m.contains("region")));
    srv.state.expired.store(true, Ordering::Relaxed);
    assert!(matches!(remote::probe(&*good), Err(StoreError::Auth(m)) if m.contains("expired")));
    srv.state.expired.store(false, Ordering::Relaxed);
    srv.state.skew_secs.store(40 * 60, Ordering::Relaxed);
    assert!(matches!(remote::probe(&*good), Err(StoreError::ClockSkew(m)) if m.contains("40 minute(s) behind")));
    srv.state.skew_secs.store(0, Ordering::Relaxed);
    // Expired credentials in the middle of syncing: the pass stops, nothing lost.
    let (_, mut devs) = devices(&good, 1);
    devs[0].local.0.insert("a.md".into(), "a\n".into());
    srv.state.expired.store(true, Ordering::Relaxed);
    let e = devs[0].pass().unwrap_err();
    assert!(e.to_string().contains("expired"), "{e}");
    srv.state.expired.store(false, Ordering::Relaxed);
    devs[0].pass().unwrap();

    let dsrv = dav_server();
    assert!(remote::probe(&*dav(&dsrv)).is_ok());
    assert!(matches!(remote::probe(&dav_with(&dsrv, "nope")), Err(StoreError::Auth(m)) if m.contains("user name or password")));
    // Plain http:// to another computer needs saying so.
    let cfg = WebDavConfig { url: "http://nas.example/dav".into(), allow_http: false };
    let creds = WebDavCredentials { user: "a".into(), password: Zeroizing::new("SECRETPW".into()) };
    assert!(matches!(WebDavStore::new(&cfg, &creds), Err(StoreError::Config(_))));
    // Secrets never show in debug output.
    assert!(!format!("{creds:?}").contains("SECRETPW"));
    let s3c = S3Credentials {
        access_key: "AK".into(),
        secret_key: Zeroizing::new("TOPSECRET".into()),
        session_token: Some(Zeroizing::new("TOK".into())),
    };
    assert!(!format!("{s3c:?}").contains("TOPSECRET") && !format!("{s3c:?}").contains("TOK"));
}

#[test]
fn hostile_servers_are_refused_without_harm() {
    for (srv, is_s3) in [(s3_server(), true), (dav_server(), false)] {
        let store = if is_s3 { s3(&srv) } else { dav(&srv) };
        let (space, mut devs) = devices(&store, 2);
        devs[0].local.0.insert("a.md".into(), "a\n".into());
        devs[0].pass().unwrap();
        let kinds: &[Hostile] = if is_s3 {
            &[Hostile::BadXml, Hostile::EntityBomb, Hostile::HugeListing, Hostile::Redirect, Hostile::EndlessListing]
        } else {
            // PROPFIND has no pages.
            &[Hostile::BadXml, Hostile::EntityBomb, Hostile::HugeListing, Hostile::Redirect]
        };
        for &h in kinds {
            srv.set_hostile(Some(h));
            let r = devs[1].pass();
            assert!(r.is_err(), "{h:?} accepted");
            let msg = r.unwrap_err().to_string();
            if h == Hostile::Redirect {
                assert!(msg.contains("elsewhere.example") && msg.contains("doesn't follow redirects"), "{msg}");
            }
            assert!(devs[1].local.0.is_empty(), "{h:?}");
        }
        // Names with `..`, absolute paths and other hosts are skipped.
        srv.set_hostile(Some(Hostile::DotDot));
        devs[1].pass().unwrap();
        assert_eq!(devs[1].local.0.get("a.md").map(String::as_str), Some("a\n"));
        let listed = store.list(&format!("quire-sync/{space}/")).unwrap();
        assert!(listed.iter().any(|e| e.name == ".."), "the store reports it…");
        assert!(remote::fingerprint(&*store, &space, &DeviceId::random().unwrap()).is_ok(), "…and sync ignores it");
        // A body far over the limit is cut off, not read.
        srv.set_hostile(Some(Hostile::HugeBody));
        let e = store.get(&format!("quire-sync/{space}/space.json"), 1024 * 1024).unwrap_err();
        assert!(e.to_string().contains("over 1 MB"), "{e}");
        srv.set_hostile(None);
    }
}

/// Against real servers: moto's S3 (`moto_server`, which checks SigV4 and
/// conditional writes) and `WsgiDAV`. Off unless `QUIRE_SYNC_REAL_SERVERS`
/// is a Python with `moto[server]` and `wsgidav` + `cheroot` installed.
#[test]
fn real_servers() {
    let Some(python) = std::env::var_os("QUIRE_SYNC_REAL_SERVERS") else {
        eprintln!("skipped: set QUIRE_SYNC_REAL_SERVERS to a Python with moto[server] and wsgidav");
        return;
    };
    let port = |base: u16| base + u16::try_from(std::process::id() % 1000).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // WsgiDAV with Basic auth over a temporary folder.
    let dav_port = port(41000);
    let conf = dir.path().join("wsgidav.yaml");
    std::fs::create_dir_all(dir.path().join("share")).unwrap();
    std::fs::write(
        &conf,
        format!(
            "host: 127.0.0.1\nport: {dav_port}\nprovider_mapping:\n  /dav: {}\nhttp_authenticator:\n  domain_controller: null\n  accept_basic: true\n  accept_digest: false\n  default_to_digest: false\nsimple_dc:\n  user_mapping:\n    '*':\n      asha:\n        password: app pass\nverbose: 1\n",
            dir.path().join("share").display()
        ),
    )
    .unwrap();
    let mut wsgidav = std::process::Command::new(&python)
        .args(["-I", "-m", "wsgidav.server.server_cli", "--config"])
        .arg(&conf)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let s3_port = port(42000);
    let mut moto = std::process::Command::new(&python)
        .args(["-I", "-m", "moto.server", "-H", "127.0.0.1", "-p", &s3_port.to_string()])
        .env("INITIAL_NO_AUTH_ACTION_COUNT", "8")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let up = |p: u16| {
        (0..100).any(|_| {
            std::net::TcpStream::connect(("127.0.0.1", p)).is_ok() || (std::thread::sleep(std::time::Duration::from_millis(100)), false).1
        })
    };
    assert!(up(dav_port) && up(s3_port), "servers didn't start");
    let result = std::panic::catch_unwind(|| {
        let dav_store: Arc<dyn ObjectStore> = Arc::new(
            WebDavStore::new(
                &WebDavConfig { url: format!("http://127.0.0.1:{dav_port}/dav/"), allow_http: false },
                &WebDavCredentials { user: "asha".into(), password: Zeroizing::new("app pass".into()) },
            )
            .unwrap(),
        );
        let p = remote::probe(&*dav_store).unwrap();
        eprintln!("WsgiDAV: {}", p.summary());
        concurrent_rounds(&dav_store, 6);
        // moto: make an IAM user and key (the three unauthenticated calls); after that moto checks every signature.
        let (s3_store, outside) = real_moto_store(&python, s3_port);
        // moto enforces the policy: the same key can't touch the rest of the bucket.
        assert!(matches!(remote::probe(&outside), Err(StoreError::Auth(_))), "{:?}", remote::probe(&outside));
        let p = remote::probe(&*s3_store).unwrap();
        eprintln!("moto: {}", p.summary());
        assert!(p.conditional_writes);
        concurrent_rounds(&s3_store, 6);
    });
    let _ = wsgidav.kill();
    let _ = moto.kill();
    let _ = wsgidav.wait();
    let _ = moto.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn real_moto_store(python: &std::ffi::OsStr, port: u16) -> (Arc<dyn ObjectStore>, S3Store) {
    // boto3 makes the bucket, and a user with the minimal policy of
    // docs/sync-backends.md; Quire then signs its own requests.
    let policy = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:ListBucket","Resource":"arn:aws:s3:::notes","Condition":{"StringLike":{"s3:prefix":["vault/quire-sync/*"]}}},{"Effect":"Allow","Action":["s3:GetObject","s3:PutObject","s3:DeleteObject"],"Resource":"arn:aws:s3:::notes/vault/quire-sync/*"}]}"#;
    let script = format!(
        "import boto3\nkw=dict(endpoint_url='http://127.0.0.1:{port}', region_name='eu-central-1', aws_access_key_id='x', aws_secret_access_key='x')\n\
boto3.client('s3', **kw).create_bucket(Bucket='notes', CreateBucketConfiguration={{'LocationConstraint':'eu-central-1'}})\n\
iam=boto3.client('iam', **kw)\niam.create_user(UserName='quire')\n\
iam.put_user_policy(UserName='quire', PolicyName='quire-sync', PolicyDocument='{policy}')\n\
k=iam.create_access_key(UserName='quire')['AccessKey']\n\
for _ in range(10):\n  try: iam.list_users()\n  except Exception: break\n\
print(k['AccessKeyId']); print(k['SecretAccessKey'])\n"
    );
    let out = std::process::Command::new(python).args(["-I", "-c", &script]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let mut lines = text.lines();
    let (ak, sk) = (lines.next().unwrap().to_string(), lines.next().unwrap().to_string());
    let cfg = S3Config {
        endpoint: format!("http://127.0.0.1:{port}"),
        region: "eu-central-1".into(),
        bucket: "notes".into(),
        prefix: "vault".into(),
        virtual_hosted: false,
        allow_http: false,
    };
    let creds = S3Credentials { access_key: ak, secret_key: Zeroizing::new(sk), session_token: None };
    let outside = S3Store::new(S3Config { prefix: "elsewhere".into(), ..cfg.clone() }, creds.clone()).unwrap();
    (Arc::new(S3Store::new(cfg, creds).unwrap()), outside)
}
