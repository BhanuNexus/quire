// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Web capture against fixture pages served on loopback (design §B.3):
//! the expected Markdown, images and front matter, and the SSRF, size,
//! redirect and charset rules.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use quire_agent::{Actor, AgentStore, DeviceInfo, FixedClock, SubmitOutcome};
use quire_app::vault::{NotePath, Vault};
use quire_import::web::{self, FetchOptions, WebError};
use quire_mcp::capture::{self, Options};
use quire_mcp::{Server, ServerConfig};
use serde_json::{Value, json};

type Route = (u16, Vec<(&'static str, String)>, Vec<u8>);

/// A tiny HTTP server: `routes` maps a path to (status, headers, body).
fn serve(routes: HashMap<&'static str, Route>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let routes = Arc::new(routes);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let routes = Arc::clone(&routes);
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, headers, body) = routes.get(path.as_str()).cloned().unwrap_or_else(|| (404, vec![], b"not found".to_vec()));
                let mut s = s;
                let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers {
                    head.push_str(k);
                    head.push_str(": ");
                    head.push_str(&v);
                    head.push_str("\r\n");
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            });
        }
    });
    base
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/web/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn html(body: Vec<u8>) -> Route {
    (200, vec![("Content-Type", "text/html; charset=utf-8".into())], body)
}

fn site() -> String {
    let png = fixture("rope.png");
    serve(HashMap::from([
        ("/blog/post", html(fixture("article.html"))),
        ("/blog/img/rope.png", (200, vec![("Content-Type", "image/png".into())], png.clone())),
        ("/cdn/rope-copy.png", (200, vec![("Content-Type", "image/png".into())], png)),
        ("/blog/diagram.svg", (200, vec![("Content-Type", "image/svg+xml".into())], fixture("diagram.svg"))),
    ]))
}

fn store(v: &Vault) -> AgentStore {
    AgentStore::open(v, DeviceInfo { id: "d_cap".into(), name: "t".into() }, FixedClock::new(1_793_577_600_000))
}

#[test]
fn a_fixture_page_becomes_an_untrusted_note_with_its_images() {
    let base = site();
    let url = format!("{base}/blog/post");
    let v = Vault::in_memory(&[("Inbox.md", "x")]);
    let st = store(&v);
    let actor = Actor::import("url", "d_cap");
    let done = capture::capture(&v, &st, &actor, &url, &Options { allow_private: true, ..Options::default() }).unwrap();
    assert!(matches!(done.outcome, SubmitOutcome::Applied { .. }), "raw/ takes new files");
    assert_eq!(done.path, NotePath::new("raw/127.0.0.1 - Ropes for text editors.md"));
    assert_eq!(done.images, 2, "rope.png and its copy are one file; the data: dot is a file too; the SVG stays a link");
    assert!(done.warnings.iter().any(|w| w.contains("diagram.svg") && w.contains("SVG")), "{:?}", done.warnings);
    let text = v.read(&done.path).unwrap();
    // Front matter (the capture time varies).
    let (fm, body) = text.split_once("\n---\n\n").unwrap();
    let fm_lines: Vec<&str> = fm.lines().filter(|l| !l.starts_with("captured: ")).collect();
    assert_eq!(
        fm_lines,
        [
            "---",
            &format!("source: {url}"),
            "site: Example Blog",
            "author: Ada Lovelace",
            "published: 2026-10-30T09:00:00Z",
            "trust: untrusted"
        ]
    );
    // The article, as Markdown: golden file (QUIRE_BLESS=1 re-records it).
    let golden = format!("{}/tests/fixtures/web/article.expected.md", env!("CARGO_MANIFEST_DIR"));
    let normalized = body.replace(&base, "http://SITE");
    if std::env::var_os("QUIRE_BLESS").is_some() {
        std::fs::write(&golden, &normalized).unwrap();
    }
    assert_eq!(normalized, std::fs::read_to_string(&golden).unwrap_or_default(), "the converted article changed");
    // What the golden file must say, whatever else changes.
    assert!(body.starts_with("# Ropes for text editors\n"));
    assert!(body.contains("[original paper](http://SITE/papers/ropes.pdf)".replace("http://SITE", &base).as_str()), "links are absolute");
    assert!(body.contains("```\nfn split(node, at) {"), "code blocks stay code");
    assert!(body.contains("| Operation | Rope") && body.contains("| Insert    | O(log n) | O(n)  |"), "tables stay tables");
    assert!(body.contains("![[rope.png|A rope tree]]") && body.contains("![[rope.png|Copy]]"), "deduplicated images are embedded");
    assert!(
        body.contains("![Diagram](") && body.contains("/blog/diagram.svg)") && !body.contains("<svg"),
        "SVG stays a link, never inlined"
    );
    assert!(!body.contains("data:image"), "data: images become files");
    assert!(
        !body.contains("Popular") && !body.contains("Privacy") && !body.contains("Ignore all previous"),
        "navigation, footers and scripts are dropped"
    );
    assert_eq!(v.storage().read_bytes(&NotePath::new("attachments/rope.png")).unwrap(), fixture("rope.png"));
    assert!(!v.exists(&NotePath::new("attachments/diagram.svg")), "SVG is never saved");
    // The same page again: the images are reused, the note gets a new name.
    let again = capture::capture(&v, &st, &actor, &url, &Options { allow_private: true, ..Options::default() }).unwrap();
    assert_eq!(again.path, NotePath::new("raw/127.0.0.1 - Ropes for text editors 1.md"));
    assert!(!v.exists(&NotePath::new("attachments/rope 1.png")));
}

#[test]
fn the_mcp_tool_captures_and_labels_the_note_untrusted() {
    let base = site();
    let v = Vault::in_memory(&[("A.md", "a")]);
    let cfg = ServerConfig {
        allow_write: true,
        allow_private_urls: true,
        device: Some(DeviceInfo { id: "d_cap".into(), name: "t".into() }),
        ..ServerConfig::default()
    };
    let srv = Server::with_config(v.clone(), None, cfg);
    let call = |id: u64, name: &str, args: Value| {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args,
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "agent", "version": "1"}}}});
        let mut out = Vec::new();
        srv.serve(std::io::Cursor::new(msg.to_string()), &mut out).unwrap();
        serde_json::from_slice::<Value>(&out).unwrap()
    };
    let r = call(1, "capture_url", json!({"url": format!("{base}/blog/post")}));
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["outcome"], "applied", "{r}");
    assert_eq!(s["images"], 0, "attachments/ is the person's (suggestions only): images stay links");
    assert!(v.storage().list().unwrap().iter().all(|p| !p.as_str().starts_with("attachments/")));
    let path = s["path"].as_str().unwrap().to_string();
    let r = call(2, "view", json!({"path": path}));
    assert_eq!(r["result"]["structuredContent"]["trust"], "untrusted");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(&format!("<untrusted-source path=\"{path}\" origin=\"{base}/blog/post\">")), "{text}");
    // Without allow_private_urls, loopback is refused (SSRF).
    let srv2 = Server::with_config(
        v,
        None,
        ServerConfig { allow_write: true, device: Some(DeviceInfo { id: "d".into(), name: "t".into() }), ..ServerConfig::default() },
    );
    let msg = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "capture_url", "arguments": {"url": format!("{base}/blog/post")},
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
    let mut out = Vec::new();
    srv2.serve(std::io::Cursor::new(msg.to_string()), &mut out).unwrap();
    let r: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r["result"]["structuredContent"]["error"], "blocked_address", "{r}");
}

#[test]
fn fetching_is_limited() {
    let page = fixture("article.html");
    let mut routes = HashMap::from([
        ("/big", html(vec![b'x'; 2048])),
        ("/latin1", (200, vec![("Content-Type", "text/html; charset=ISO-8859-1".into())], b"<html><head><title>Cr\xe8me br\xfbl\xe9e</title></head><body><article><p>Une cr\xe8me br\xfbl\xe9e, s'il vous pla\xeet. Une longue phrase pour que l'article soit retenu par l'extracteur de contenu principal.</p></article></body></html>".to_vec())),
        ("/to-file", (302, vec![("Location", "file:///etc/passwd".into())], vec![])),
        ("/pdf", (200, vec![("Content-Type", "application/pdf".into())], b"%PDF-1.7".to_vec())),
        ("/text", (200, vec![("Content-Type", "text/plain".into())], b"plain ```text```".to_vec())),
        ("/gone", (404, vec![], vec![])),
        ("/page", html(page)),
    ]);
    let hops = ["/r0", "/r1", "/r2", "/r3", "/r4", "/r5", "/r6"];
    for w in hops.windows(2) {
        routes.insert(w[0], (302, vec![("Location", w[1].into())], vec![]));
    }
    routes.insert("/r6", html(b"<p>end</p>".to_vec()));
    routes.insert("/ok-hops", (301, vec![("Location", "/r2".into())], vec![]));
    let base = serve(routes);
    let open = FetchOptions { allow_private: true, ..FetchOptions::default() };
    // Private and loopback addresses need the explicit setting, also after redirects.
    assert!(matches!(web::capture(&format!("{base}/page"), &FetchOptions::default()), Err(WebError::Blocked(_))));
    assert!(matches!(web::capture("http://169.254.169.254/latest/meta-data/", &FetchOptions::default()), Err(WebError::Blocked(_))));
    assert!(matches!(web::capture("http://[::1]:1/", &FetchOptions::default()), Err(WebError::Blocked(_))));
    // Size, redirects, schemes, types.
    assert_eq!(
        web::capture(&format!("{base}/big"), &FetchOptions { max_page_bytes: 1024, allow_private: true, ..FetchOptions::default() })
            .unwrap_err(),
        WebError::TooLarge(1024)
    );
    assert_eq!(web::capture(&format!("{base}/r0"), &open).unwrap_err(), WebError::TooManyRedirects, "six redirects");
    assert!(web::capture(&format!("{base}/ok-hops"), &open).is_ok(), "five are fine");
    assert!(matches!(web::capture(&format!("{base}/to-file"), &open), Err(WebError::BadUrl(_))));
    assert!(matches!(web::capture(&format!("{base}/pdf"), &open), Err(WebError::NotAPage(_))));
    assert_eq!(web::capture(&format!("{base}/gone"), &open).unwrap_err(), WebError::Status(404));
    let t = web::capture(&format!("{base}/text"), &open).unwrap();
    assert_eq!(t.article.markdown, "```text\nplain ʼʼʼtextʼʼʼ\n```\n", "plain text can't break out of its fence");
    // The charset from Content-Type.
    let c = web::capture(&format!("{base}/latin1"), &open).unwrap();
    assert_eq!(c.article.title, "Crème brûlée");
    assert!(c.article.markdown.contains("Une crème brûlée, s'il vous plaît."), "{}", c.article.markdown);
    // Images over the limit are left as links, with a warning.
    let small = FetchOptions { max_image_bytes: 10, ..open };
    let site = site();
    let c = web::capture(&format!("{site}/blog/post"), &small).unwrap();
    assert_eq!(c.images.len(), 0);
    assert!(c.warnings.iter().any(|w| w.contains("rope.png")), "{:?}", c.warnings);
}

/// A server whose agent policy (approved on this device) is `policy`.
fn server_with_policy(v: &Vault, policy: &str, state: &std::path::Path) -> Server {
    v.storage().write_atomic(&NotePath::new(".quire/shared/agent-policy.toml"), policy).unwrap();
    let st = store(v);
    st.set_state_dir(Some(state.to_path_buf()));
    st.approve_policy(st.policy_state().file_hash().as_ref()).unwrap();
    let cfg = ServerConfig {
        allow_write: true,
        allow_private_urls: true,
        device: Some(DeviceInfo { id: "d_cap".into(), name: "t".into() }),
        state_dir: Some(state.to_path_buf()),
        ..ServerConfig::default()
    };
    Server::with_config(v.clone(), None, cfg)
}

fn tool(srv: &Server, name: &str, args: &Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args,
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "agent", "version": "1"}}}});
    let mut out = Vec::new();
    srv.serve(std::io::Cursor::new(msg.to_string()), &mut out).unwrap();
    serde_json::from_slice::<Value>(&out).unwrap()
}

#[test]
fn capture_images_ignore_policy() {
    let page = b"<html><head><title>Payload</title></head><body><article><p>A long enough paragraph for the extractor to keep it as the article of this page.</p><p><img src=\"/a.bmp\" alt=\"a\"><img src=\"/b.png\" alt=\"b\"><img src=\"/c.svg\" alt=\"c\"></p></article></body></html>".to_vec();
    let png = fixture("rope.png");
    let base = serve(HashMap::from([
        ("/page", html(page)),
        ("/a.bmp", (200, vec![("Content-Type", "image/bmp".into())], b"BM#!/bin/sh\necho arbitrary payload\n".to_vec())),
        ("/b.png", (200, vec![("Content-Type", "image/png".into())], png.clone())),
        (
            "/c.svg",
            (
                200,
                vec![("Content-Type", "image/svg+xml".into())],
                b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec(),
            ),
        ),
    ]));
    let url = format!("{base}/page");
    // Reads everywhere, new files only in raw/: images can't be written.
    let dir = tempfile::tempdir().unwrap();
    let v = Vault::in_memory(&[("A.md", "a")]);
    let srv = server_with_policy(
        &v,
        "format = 1\ndefault = \"read\"\n[[zone]]\npath = \"raw/\"\nrole = \"raw\"\nmode = \"create-only\"\n",
        dir.path(),
    );
    let r = tool(&srv, "capture_url", &json!({"url": url}));
    let s = &r["result"]["structuredContent"];
    assert!(s["message"].as_str().unwrap_or_default().contains("Applied") || s["outcome"] == "applied", "{r}");
    assert_eq!(s["images"], 0, "{r}");
    let files: Vec<String> = v.storage().list().unwrap().into_iter().map(|p| p.to_string()).collect();
    assert!(files.iter().all(|f| !f.starts_with("attachments/")), "{files:?}");
    // Where the agent may add files, only real images are kept.
    let dir = tempfile::tempdir().unwrap();
    let v = Vault::in_memory(&[("A.md", "a")]);
    let srv = server_with_policy(
        &v,
        "format = 1\ndefault = \"read\"\n[[zone]]\npath = \"raw/\"\nrole = \"raw\"\nmode = \"create-only\"\n[[zone]]\npath = \"attachments/\"\nrole = \"agent\"\nmode = \"direct\"\n",
        dir.path(),
    );
    let r = tool(&srv, "capture_url", &json!({"url": url}));
    assert_eq!(r["result"]["structuredContent"]["images"], 1, "{r}");
    let files: Vec<String> = v.storage().list().unwrap().into_iter().map(|p| p.to_string()).collect();
    assert_eq!(files.iter().filter(|f| f.starts_with("attachments/")).collect::<Vec<_>>(), ["attachments/b.png"]);
    assert_eq!(v.storage().read_bytes(&NotePath::new("attachments/b.png")).unwrap(), png);
    // Each image is a write: a limit of 2 writes a minute leaves room for the note only after one image.
    let dir = tempfile::tempdir().unwrap();
    let v = Vault::in_memory(&[("A.md", "a")]);
    let many = b"<html><body><article><p>A long enough paragraph for the extractor to keep it as the article of this page.</p><p><img src=\"/1.png\"><img src=\"/2.png\"><img src=\"/3.png\"></p></article></body></html>".to_vec();
    let mut png2 = png.clone();
    png2.extend_from_slice(b"trailing bytes make another file");
    let mut png3 = png.clone();
    png3.extend_from_slice(b"and a third");
    let base = serve(HashMap::from([
        ("/many", html(many)),
        ("/1.png", (200, vec![], png)),
        ("/2.png", (200, vec![], png2)),
        ("/3.png", (200, vec![], png3)),
    ]));
    let srv = server_with_policy(
        &v,
        "format = 1\n[[zone]]\npath = \"raw/\"\nrole = \"raw\"\nmode = \"create-only\"\n[[zone]]\npath = \"attachments/\"\nrole = \"agent\"\nmode = \"direct\"\n[limits]\nwrites_per_minute = 2\n",
        dir.path(),
    );
    let r = tool(&srv, "capture_url", &json!({"url": format!("{base}/many")}));
    let s = &r["result"]["structuredContent"];
    assert_eq!((s["outcome"].clone(), s["images"].clone()), (json!("applied"), json!(1)), "{r}");
}

/// A proxy on loopback that tunnels with CONNECT and serves one page itself;
/// returns its URL and the CONNECT lines it saw.
fn connect_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    return;
                }
                log.lock().unwrap().push(line.trim().to_string());
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                }
                let mut s = s;
                let _ = s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                // The tunnelled request.
                let mut get = String::new();
                if r.read_line(&mut get).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                }
                let body = b"<html><head><title>Through the proxy</title></head><body><p>hello</p></body></html>";
                let head =
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body);
            });
        }
    });
    (url, seen)
}

#[test]
fn capture_through_local_proxy() {
    let (proxy, seen) = connect_proxy();
    let opts = FetchOptions { proxy: Some(proxy), ..FetchOptions::default() };
    // A public address through a proxy on loopback works.
    let got = web::fetch("http://93.184.215.14/", &opts, 1 << 20).unwrap();
    assert!(String::from_utf8_lossy(&got.bytes).contains("Through the proxy"));
    assert_eq!(seen.lock().unwrap().first().map(String::as_str), Some("CONNECT 93.184.215.14:80 HTTP/1.1"));
    // Private addresses are still refused, before the proxy is asked.
    let before = seen.lock().unwrap().len();
    for private in ["http://10.0.0.1/", "http://169.254.169.254/latest/meta-data/", "http://[::1]:8080/"] {
        assert!(matches!(web::fetch(private, &opts, 1 << 20), Err(WebError::Blocked(_))), "{private}");
    }
    assert_eq!(seen.lock().unwrap().len(), before, "the proxy never saw them");
}
