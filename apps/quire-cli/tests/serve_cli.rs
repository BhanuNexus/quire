// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.10 integration: `quire-cli sync` and `quire-cli serve --sync` against
//! a real Quire Sync server (started in this process), in both editions.
//! A laptop and a headless server sync through it; `serve` shows the notes
//! (with a voice note and a drawing) behind its Beta sign-in, answers MCP
//! `ask` with the nearest passages on `/mcp`, and sends a change made on
//! the server back to the laptop.
#![cfg(unix)]
#![allow(clippy::unwrap_used, reason = "test code")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PASS: &str = "a long sync passphrase";
const TOKEN: &str = "server-token-0123456789abcdef";
const WEB_PASS: &str = "correct horse battery";

fn sync_server() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let state = quire_sync_server::AppState::new(quire_sync_server::Store::in_memory().unwrap(), TOKEN);
            axum::serve(listener, quire_sync_server::app(Arc::new(state))).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    /// `quire-cli` as device `who` (each device has its own config).
    fn cmd(&self, who: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_quire-cli"));
        c.args(args)
            .env("HOME", self.path(&format!("{who}-home")))
            .env("XDG_CONFIG_HOME", self.path(&format!("{who}-config")))
            .env("QUIRE_SECRETS_FILE", self.path(&format!("{who}-config/secrets.toml")))
            .env("QUIRE_SYNC_PASSPHRASE", PASS)
            .env("QUIRE_SYNC_TOKEN", TOKEN)
            .env("QUIRE_WEB_PASSPHRASE", WEB_PASS)
            .env("QUIRE_NO_NOTIFICATIONS", "1")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, who: &str, args: &[&str]) -> String {
        let out: Output = self.cmd(who, args).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{args:?}: {text}");
        text
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < Duration::from_mins(2), "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A `serve` process this test started; stopped with SIGTERM when dropped.
struct Serve(Child);

impl Serve {
    /// Fails the test with what `serve` said if it has stopped.
    fn check(&mut self) {
        if let Some(status) = self.0.try_wait().unwrap() {
            let mut said = String::new();
            if let Some(e) = self.0.stderr.as_mut() {
                let _ = std::io::Read::read_to_string(e, &mut said);
            }
            panic!("serve stopped ({status}): {said}");
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-TERM", &self.0.id().to_string()]).status();
        let start = Instant::now();
        while self.0.try_wait().ok().flatten().is_none() && start.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn mcp(base: &str, token: &str, method: &str, name: Option<&str>, params: &Value) -> Value {
    let mut p = params.clone();
    p["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "serve-test", "version": "1"}});
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": p}).to_string();
    let mut req = agent()
        .post(&format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method);
    if let Some(n) = name {
        req = req.header("Mcp-Name", n);
    }
    let mut res = req.send(body).unwrap();
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap();
    assert_eq!(status, 200, "{text}");
    let json_text = text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap_or(&text).to_string();
    serde_json::from_str(&json_text).unwrap()
}

#[test]
fn sync_and_serve_through_a_quire_sync_server() {
    let env = Env { dir: tempfile::tempdir().unwrap() };
    let server = sync_server();
    let (laptop, home) = (env.path("laptop"), env.path("home-server"));
    for d in [&laptop, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let write = |root: &Path, p: &str, bytes: &[u8]| {
        let f = root.join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, bytes).unwrap();
    };
    write(&laptop, "Work/Launch.md", "# Launch\n\nThe launch moved to Friday because the venue was free.\n\n![[Voice note 2026-11-05 153012.wav]]\n\n![[Plan.excalidraw]]\n".as_bytes());
    write(&laptop, "Work/Plan.excalidraw.md", include_bytes!("../../../crates/quire-canvas/testdata/excalidraw/Plain.excalidraw.md"));
    write(
        &laptop,
        "attachments/Voice note 2026-11-05 153012.wav",
        b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x80\x3e\x00\x00\x00\x7d\x00\x00\x02\x00\x10\x00data\x00\x00\x00\x00",
    );
    write(&laptop, "Home/Garden.md", b"# Garden\n\nTomatoes go in the sunny bed.\n");
    let (l, h) = (laptop.to_str().unwrap(), home.to_str().unwrap());
    let key = env.path("recovery.txt");
    env.run(
        "laptop",
        &["sync", "join", l, "--server", &server, "--create", "--recovery-key-file", key.to_str().unwrap(), "--device", "laptop"],
    );
    env.run("laptop", &["sync", "once", l]);
    // The server holds Work/ only.
    env.run("srv", &["sync", "join", h, "--server", &server, "--device", "home server", "--only", "Work", "--only", "attachments"]);
    env.run("srv", &["serve", "passwd"]);
    let created = env.run("srv", &["mcp", "token", "create", "--name", "agent", "--scope", "notes:read,notes:ask"]);
    let token = created
        .split_whitespace()
        .find(|w| w.len() > 30 && !w.contains('/'))
        .unwrap()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_string();

    let help = env.run("srv", &["serve", "--help"]);
    assert!(help.contains("Beta, for self-hosters") && help.contains("decrypted copy"), "{help}");

    let port = free_port();
    let child = env
        .cmd("srv", &["serve", h, "--sync", "--poll", "5s", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut serve = Serve(child);
    let base = format!("http://127.0.0.1:{port}");
    wait_for("serve --sync brings the notes", || {
        serve.check();
        home.join("Work/Launch.md").exists() && home.join("attachments/Voice note 2026-11-05 153012.wav").exists()
    });
    assert!(!home.join("Home/Garden.md").exists(), "outside the selection");
    wait_for("serve listens", || agent().get(&format!("{base}/login")).call().is_ok());

    // The web app: Beta sign-in, then the note with its recording and drawing.
    let mut res = agent().get(&format!("{base}/login")).call().unwrap();
    let cookie = res
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let page = res.body_mut().read_to_string().unwrap();
    assert!(page.contains(">Beta</span>") && page.contains("decrypted copy"), "{page}");
    let i = page.find("name=\"_login\" value=\"").unwrap() + 21;
    let login_token = &page[i..i + page[i..].find('"').unwrap()];
    let res = agent()
        .post(&format!("{base}/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Origin", &base)
        .header("Cookie", &cookie)
        .send(format!("passphrase={}&_login={login_token}&next=%2F", WEB_PASS.replace(' ', "+")))
        .unwrap();
    assert_eq!(res.status().as_u16(), 303);
    let session = res
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    // The index catches up with the notes sync brought a moment ago.
    let mut note = String::new();
    let start = Instant::now();
    while !note.contains("data:image/svg+xml") && start.elapsed() < Duration::from_secs(30) {
        let mut res = agent().get(&format!("{base}/n/Work/Launch.md")).header("Cookie", &session).call().unwrap();
        assert_eq!(res.status().as_u16(), 200);
        note = res.body_mut().read_to_string().unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(note.contains("moved to Friday"), "{note}");
    assert!(note.contains("<audio controls preload=\"metadata\" src=\"/f/attachments/Voice%20note%202026-11-05%20153012.wav\">"), "{note}");
    assert!(note.contains("<img src=\"data:image/svg+xml;base64,"), "the drawing is a picture: {note}");
    assert!(note.contains("<footer class=\"foot\"><span class=\"beta\">Beta</span>"), "{note}");

    // MCP on the same port: `ask` answers, and abstains with the nearest passages.
    let r = mcp(&base, &token, "tools/call", Some("ask"), &json!({"name": "ask", "arguments": {"question": "When is the launch?"}}));
    let out = &r["result"]["structuredContent"];
    assert_eq!(out["abstained"], false, "{r}");
    assert!(out["citations"].to_string().contains("Launch"), "{r}");
    let r = mcp(
        &base,
        &token,
        "tools/call",
        Some("ask"),
        &json!({"name": "ask", "arguments": {"question": "Which colour is the delivery van that brings the stage?"}}),
    );
    let out = &r["result"]["structuredContent"];
    assert_eq!(out["abstained"], true, "{r}");
    assert!(out["citations"].as_array().unwrap().is_empty(), "{r}");
    assert!(out.get("nearest").is_some_and(Value::is_array), "{r}");

    // A change made on the server goes back to the laptop (the watcher nudges the sync).
    std::fs::write(home.join("Work/Server note.md"), "# Server note\n\nWritten on the home server.\n").unwrap();
    wait_for("the laptop gets the server's note", || {
        env.run("laptop", &["sync", "once", l]);
        laptop.join("Work/Server note.md").exists()
    });
    assert!(laptop.join("Home/Garden.md").exists(), "the laptop keeps what the server doesn't hold");
    let status = env.run("srv", &["sync", "status", h]);
    assert!(status.contains("Folders: only"), "{status}");
}
