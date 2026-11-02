// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Web capture against fixture pages served on loopback (design §B.3):
//! the expected Markdown, images and front matter, and the SSRF, size,
//! redirect and charset rules.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

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
    assert_eq!(done.images, 3, "rope.png and its copy are one file; the SVG and the data: dot are files too");
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
    assert!(body.contains("![[diagram.svg|Diagram]]") && !body.contains("<svg"), "SVG is a file, never inlined");
    assert!(!body.contains("data:image"), "data: images become files");
    assert!(
        !body.contains("Popular") && !body.contains("Privacy") && !body.contains("Ignore all previous"),
        "navigation, footers and scripts are dropped"
    );
    assert_eq!(v.storage().read_bytes(&NotePath::new("attachments/rope.png")).unwrap(), fixture("rope.png"));
    assert!(v.storage().read_bytes(&NotePath::new("attachments/diagram.svg")).unwrap().starts_with(b"<svg"));
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
    assert_eq!(s["images"], 3);
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
