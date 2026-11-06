// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The web app end to end on loopback, sharing its port with MCP: sign-in,
//! sessions, CSRF, Origin and Host, the path guard, editing with conflict
//! checks and provenance, read-only mode, the sample vault, and MCP with
//! OAuth approved through the web app.
#![allow(clippy::unwrap_used, reason = "test code")]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use quire_app::vault::{NotePath, Vault};
use quire_mcp::consent::ConsentStore;
use quire_mcp::http::{HttpConfig, HttpServer, cimd};
use quire_mcp::tokens::{Scopes, TokenStore};
use quire_mcp::{Server, ServerConfig};
use quire_web::WebConfig;
use quire_web::auth::{LoginStore, SessionLimits};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PASS: &str = "correct horse battery";

struct Env {
    http: HttpServer,
    base: String,
    root: PathBuf,
    clock: Arc<AtomicI64>,
    consent: ConsentStore,
    tokens: TokenStore,
    _dir: tempfile::TempDir,
}

fn write(root: &Path, p: &str, text: &str) {
    let f = root.join(p);
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(f, text).unwrap();
}

fn start_with(files: &[(&str, &str)], read_only: bool, setup: impl FnOnce(&mut WebConfig)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(&root).unwrap();
    for (p, t) in files {
        write(&root, p, t);
    }
    let vault = Vault::open_local(&root).unwrap();
    let device = quire_agent::DeviceInfo { id: "d_web".into(), name: "home server".into() };
    let srv = Server::with_config(
        vault,
        None,
        ServerConfig {
            allow_write: true,
            read_only,
            device: Some(device.clone()),
            state_dir: Some(dir.path().join("state")),
            ..ServerConfig::default()
        },
    );
    let login = LoginStore::at(dir.path().join("serve/login.toml"));
    login.set(PASS, 0).unwrap();
    let clock = Arc::new(AtomicI64::new(1_000));
    let c2 = Arc::clone(&clock);
    let consent = ConsentStore::in_dir(&dir.path().join("mcp/consent"));
    let tokens = TokenStore::in_dir(&dir.path().join("mcp"));
    let mut web = WebConfig {
        server: srv.clone(),
        read_only,
        hide: vec!["Private".into()],
        login,
        consent: consent.clone(),
        limits: SessionLimits { idle_ms: 60_000, max_ms: 600_000 },
        device,
        clock: Arc::new(move || c2.load(Ordering::SeqCst)),
    };
    setup(&mut web);
    let cfg = HttpConfig {
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        public_url: String::new(),
        tokens: tokens.clone(),
        consent: consent.clone(),
        cimd: cimd::Policy { allow_http_loopback: true },
    };
    let http = HttpServer::start_with(srv, cfg, |urls| quire_web::mount(web, urls)).unwrap();
    let base = format!("http://127.0.0.1:{}", http.addr().port());
    Env { http, base, root, clock, consent, tokens, _dir: dir }
}

const NOTES: &[(&str, &str)] = &[
    (
        "Welcome.md",
        "# Welcome\n\nSee [[Plan]] and [[Journal/Day]] and [[Private/Keys]]. #start\n\n> [!tip] Nice\n> A callout.\n\n![[pic.png]]\n",
    ),
    ("Projects/Plan.md", "# Plan\n\nThe café opens in spring. #project\n\none\ntwo\nthree\n"),
    ("Journal/Day.md", "# Day\n\nSecret diary about the café. [[Plan]]\n"),
    ("Private/Keys.md", "# Keys\n\nThe café safe code. [[Plan]]\n"),
    ("pic.png", "PNG"),
    ("evil.svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>"),
    (".quire/shared/vault.toml", "format = 1\n[web]\nhide = [\"Journal\"]\n"),
    (
        ".quire/shared/agent-policy.toml",
        "format = 1\ndefault = \"propose\"\n[[zone]]\npath = \"Projects/\"\nrole = \"private\"\nmode = \"deny\"\n",
    ),
];

fn start() -> Env {
    start_with(NOTES, false, |_| {})
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

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, n: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str())
    }

    fn cookies(&self) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")).map(|(_, v)| v.as_str()).collect()
    }

    /// The value of a hidden input or textarea named `name`.
    fn field(&self, name: &str) -> String {
        let input = format!("name=\"{name}\" value=\"");
        if let Some(i) = self.body.find(&input) {
            let rest = &self.body[i + input.len()..];
            return unescape(&rest[..rest.find('"').unwrap()]);
        }
        let ta = format!("name=\"{name}\"");
        let i = self.body.find(&ta).unwrap_or_else(|| panic!("no field {name} in {}", self.body));
        let rest = &self.body[i..];
        let open = rest.find('>').unwrap() + 1;
        let close = rest.find("</textarea>").unwrap();
        let v = unescape(&rest[open..close]);
        v.strip_prefix('\n').unwrap_or(&v).to_string()
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

fn pct(v: &str) -> String {
    v.bytes().fold(String::new(), |mut s, b| {
        if b.is_ascii_alphanumeric() {
            s.push(char::from(b));
        } else {
            let _ = write!(s, "%{b:02X}");
        }
        s
    })
}

fn into_resp(mut res: ureq::http::Response<ureq::Body>) -> Resp {
    let status = res.status().as_u16();
    let headers = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
    Resp { status, headers, body: res.body_mut().read_to_string().unwrap_or_default() }
}

/// A browser: keeps cookies, sends this server's Origin on posts.
struct Browser {
    base: String,
    cookies: HashMap<String, String>,
    origin: String,
}

impl Browser {
    fn new(env: &Env) -> Self {
        Self { base: env.base.clone(), cookies: HashMap::new(), origin: env.base.clone() }
    }

    fn keep(&mut self, r: &Resp) {
        for c in r.cookies() {
            let kv = c.split(';').next().unwrap();
            let (k, v) = kv.split_once('=').unwrap();
            if v.is_empty() || c.contains("Max-Age=0") {
                self.cookies.remove(k);
            } else {
                self.cookies.insert(k.into(), v.into());
            }
        }
    }

    fn cookie_header(&self) -> String {
        self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    fn get(&mut self, path: &str) -> Resp {
        let mut req = agent().get(&format!("{}{path}", self.base));
        if !self.cookies.is_empty() {
            req = req.header("Cookie", &self.cookie_header());
        }
        let r = into_resp(req.call().unwrap());
        self.keep(&r);
        r
    }

    fn post(&mut self, path: &str, fields: &[(&str, &str)]) -> Resp {
        let body: String = fields.iter().map(|(k, v)| format!("{k}={}", pct(v))).collect::<Vec<_>>().join("&");
        let mut req = agent().post(&format!("{}{path}", self.base)).header("Content-Type", "application/x-www-form-urlencoded");
        if !self.origin.is_empty() {
            req = req.header("Origin", &self.origin);
        }
        if !self.cookies.is_empty() {
            req = req.header("Cookie", &self.cookie_header());
        }
        let r = into_resp(req.send(body).unwrap());
        self.keep(&r);
        r
    }

    fn login(&mut self, pass: &str) -> Resp {
        let page = self.get("/login");
        let token = page.field("_login");
        self.post("/login", &[("passphrase", pass), ("_login", &token), ("next", "/")])
    }

    fn signed_in(env: &Env) -> Self {
        let mut b = Self::new(env);
        let r = b.login(PASS);
        assert_eq!(r.status, 303, "{}", r.body);
        b
    }

    /// The CSRF token from any page's sign-out form.
    fn csrf(&mut self) -> String {
        self.get("/").field("_csrf")
    }
}

#[test]
fn everything_but_assets_needs_sign_in() {
    let env = start();
    let mut b = Browser::new(&env);
    let r = b.get("/");
    assert_eq!((r.status, r.header("location")), (303, Some("/login?next=/")));
    let r = b.get("/n/Welcome.md");
    assert_eq!(r.status, 303);
    assert!(r.header("location").unwrap().starts_with("/login?next="));
    for p in ["/f/pic.png", "/search?q=cafe", "/tags", "/edit/Welcome.md", "/consent", "/d/Projects"] {
        assert_eq!(b.get(p).status, 303, "{p}");
    }
    assert_eq!(b.get("/api/quick?q=we").status, 401);
    let r = b.post("/edit/Welcome.md", &[("text", "hacked"), ("action", "save")]);
    assert_eq!(r.status, 403);
    assert!(std::fs::read_to_string(env.root.join("Welcome.md")).unwrap().starts_with("# Welcome"));
    let css = b.get("/assets/app.css");
    assert_eq!(css.status, 200);
    assert!(css.body.contains(".callout"));
    // Every page: no caching, no framing, scripts only from here.
    let r = b.get("/login");
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    let csp = r.header("content-security-policy").unwrap();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe-inline") && csp.contains("frame-ancestors 'none'"), "{csp}");
}

#[test]
fn signing_in_sets_a_strict_session_cookie() {
    let env = start();
    let mut b = Browser::new(&env);
    let r = b.login("wrong passphrase!");
    assert_eq!(r.status, 401);
    assert!(r.body.contains("That passphrase isn&#39;t right."), "{}", r.body);
    let r = b.login(PASS);
    assert_eq!((r.status, r.header("location")), (303, Some("/")));
    let session = r.cookies().into_iter().find(|c| c.starts_with("quire_session=")).unwrap().to_string();
    assert!(session.contains("HttpOnly") && session.contains("SameSite=Strict") && session.contains("Path=/"), "{session}");
    assert!(!session.contains("Secure"), "plain http on loopback");
    let home = b.get("/");
    assert_eq!(home.status, 200);
    assert!(home.body.contains("Welcome") && home.body.contains("Projects"));
    // A login form posted from another site, or without its cookie, fails.
    let mut other = Browser::new(&env);
    let token = other.get("/login").field("_login");
    other.origin = "https://evil.example".into();
    assert_eq!(other.post("/login", &[("passphrase", PASS), ("_login", &token)]).status, 403);
    let mut bare = Browser::new(&env);
    assert_eq!(bare.post("/login", &[("passphrase", PASS), ("_login", "made-up")]).status, 403);
    // Only local redirects after sign-in.
    let mut c = Browser::new(&env);
    let token = c.get("/login").field("_login");
    let r = c.post("/login", &[("passphrase", PASS), ("_login", &token), ("next", "//evil.example/x")]);
    assert_eq!(r.header("location"), Some("/"));
    // Signing out ends the session.
    let csrf = b.csrf();
    assert_eq!(b.post("/logout", &[("_csrf", &csrf)]).status, 303);
    assert_eq!(b.get("/").status, 303);
}

#[test]
fn wrong_guesses_are_limited() {
    let env = start();
    let mut b = Browser::new(&env);
    for _ in 0..5 {
        assert_eq!(b.login("not it at all").status, 401);
    }
    let r = b.login(PASS);
    assert_eq!(r.status, 429, "even the right passphrase waits");
    assert!(r.header("retry-after").is_some());
    env.clock.fetch_add(1_500, Ordering::SeqCst);
    assert_eq!(b.login(PASS).status, 303);
}

#[test]
fn sessions_expire_when_idle_and_after_the_absolute_limit() {
    let env = start();
    let mut b = Browser::signed_in(&env);
    env.clock.fetch_add(50_000, Ordering::SeqCst);
    assert_eq!(b.get("/").status, 200);
    env.clock.fetch_add(61_000, Ordering::SeqCst);
    assert_eq!(b.get("/").status, 303, "idle");
    let mut b = Browser::signed_in(&env);
    for _ in 0..13 {
        env.clock.fetch_add(50_000, Ordering::SeqCst);
        b.get("/");
    }
    assert_eq!(b.get("/").status, 303, "absolute limit");
}

#[test]
fn forms_need_the_csrf_token_and_this_origin() {
    let env = start();
    let mut b = Browser::signed_in(&env);
    let csrf = b.csrf();
    let edit = b.get("/edit/Projects/Plan.md");
    let (base, orig) = (edit.field("base"), edit.field("orig"));
    let save = |b: &mut Browser, token: &str| {
        b.post(
            "/edit/Projects/Plan.md",
            &[("_csrf", token), ("base", &base), ("orig", &orig), ("text", "# Plan\nchanged\n"), ("action", "save")],
        )
    };
    assert_eq!(save(&mut b, "").status, 403);
    assert_eq!(save(&mut b, "wrong").status, 403);
    b.origin = "https://evil.example".into();
    assert_eq!(save(&mut b, &csrf).status, 403);
    b.origin = String::new();
    assert_eq!(save(&mut b, &csrf).status, 403, "no Origin at all");
    assert!(std::fs::read_to_string(env.root.join("Projects/Plan.md")).unwrap().contains("spring"));
    b.origin = env.base.clone();
    assert_eq!(save(&mut b, &csrf).status, 303);
    assert_eq!(std::fs::read_to_string(env.root.join("Projects/Plan.md")).unwrap(), "# Plan\nchanged\n");
}

#[test]
fn host_must_be_this_server() {
    let env = start();
    let r = into_resp(agent().get(&format!("{}/login", env.base)).header("Host", "evil.example:80").call().unwrap());
    assert_eq!(r.status, 403);
    let port = env.http.addr().port();
    let r = into_resp(agent().get(&format!("{}/login", env.base)).header("Host", &format!("localhost:{port}")).call().unwrap());
    assert_eq!(r.status, 200);
}

#[test]
fn hidden_folders_dotfiles_symlinks_and_dotdot_dont_exist_for_the_web() {
    let env = start();
    #[cfg(unix)]
    {
        let outside = env.root.parent().unwrap().join("outside.md");
        std::fs::write(&outside, "# Outside\nnot in the vault\n").unwrap();
        std::os::unix::fs::symlink(&outside, env.root.join("Escape.md")).unwrap();
        std::os::unix::fs::symlink(env.root.join(".quire/shared/vault.toml"), env.root.join("Settings.md")).unwrap();
        std::os::unix::fs::symlink(env.root.join("Journal"), env.root.join("Diary")).unwrap();
    }
    let mut b = Browser::signed_in(&env);
    for p in [
        "/n/Journal/Day.md",
        "/n/journal/Day.md",
        "/n/JOURNAL/day.md",
        "/f/Journal/Day.md",
        "/d/Journal",
        "/n/Private/Keys.md",
        "/edit/Private/Keys.md",
        "/n/.quire/shared/vault.toml",
        "/f/.quire/shared/agent-policy.toml",
        "/n/%2E%2E/outside.md",
        "/n/Projects/%2E%2E/%2E%2E/outside.md",
        "/f/..%2Foutside.md",
        "/n/.trash/x.md",
        "/api/tree?dir=Journal",
        "/api/tree?dir=.quire",
        "/tags/..",
    ] {
        let r = b.get(p);
        assert!(
            r.status == 404 || (p.starts_with("/tags") && r.status == 200 && !r.body.contains("Secret")),
            "{p}: {} {}",
            r.status,
            r.body
        );
        assert!(!r.body.contains("Secret diary") && !r.body.contains("safe code") && !r.body.contains("hide ="), "{p}");
    }
    #[cfg(unix)]
    for p in ["/n/Escape.md", "/f/Escape.md", "/n/Settings.md", "/n/Diary/Day.md"] {
        let r = b.get(p);
        assert_eq!(r.status, 404, "{p}: {}", r.body);
        assert!(!r.body.contains("not in the vault") && !r.body.contains("Secret diary"));
    }
    // Lists leave them out too: search, quick open, tree, backlinks, tags, links.
    let s = b.get("/search?q=cafe");
    assert!(s.body.contains("Plan") && !s.body.contains("Day") && !s.body.contains("Keys"), "{}", s.body);
    let q = b.get("/api/quick?q=");
    assert!(!q.body.contains("Journal") && !q.body.contains("Private"), "{}", q.body);
    let q = b.get("/api/quick?q=day");
    assert!(!q.body.contains("Journal/Day"), "{}", q.body);
    let home = b.get("/");
    assert!(!home.body.contains("Journal") && !home.body.contains("Private"), "{}", home.body);
    let plan = b.get("/n/Projects/Plan.md");
    assert!(plan.body.contains("Linked from") && plan.body.contains("Welcome") && !plan.body.contains("Secret"), "{}", plan.body);
    let welcome = b.get("/n/Welcome.md");
    assert!(welcome.body.contains("<span class=\"wikilink unresolved\">Journal/Day</span>"), "{}", welcome.body);
    assert!(welcome.body.contains("<span class=\"wikilink unresolved\">Private/Keys</span>"));
    assert!(welcome.body.contains("href=\"/n/Projects/Plan.md\""));
    // An SVG from the vault never runs as one of this site's pages.
    let svg = b.get("/f/evil.svg");
    assert_eq!(svg.status, 200);
    assert!(svg.header("content-security-policy").unwrap().starts_with("sandbox"));
}

#[test]
fn private_zones_stay_hidden_from_agents_on_the_shared_port() {
    let env = start();
    let token = env.tokens.create_personal("t", Scopes::parse("notes:read").unwrap(), None, 0).unwrap();
    let call = |name: &str, args: Value| {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args,
            "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"}}}});
        into_resp(
            agent()
                .post(&format!("{}/mcp", env.base))
                .header("Accept", "application/json, text/event-stream")
                .header("Authorization", &format!("Bearer {token}"))
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", "tools/call")
                .header("Mcp-Name", name)
                .send(body.to_string())
                .unwrap(),
        )
    };
    let r = call("view", json!({"path": "Projects/Plan.md"}));
    assert_eq!(r.status, 200);
    assert!(!r.body.contains("spring") && r.body.contains("isError"), "denied zone: {}", r.body);
    let r = call("search", json!({"query": "cafe"}));
    assert!(!r.body.contains("Projects/Plan"), "{}", r.body);
    // The person sees it on the web (zones limit agents; [web] hide limits the web).
    let mut b = Browser::signed_in(&env);
    assert!(b.get("/n/Projects/Plan.md").body.contains("spring"));
}

#[test]
fn concurrent_edits_merge_or_offer_a_copy_never_overwrite() {
    let env = start();
    let mut b = Browser::signed_in(&env);
    let csrf = b.csrf();
    let path = env.root.join("Projects/Plan.md");
    // Elsewhere, a line far from ours changes: merged for review, not saved.
    let edit = b.get("/edit/Projects/Plan.md");
    let (base, orig) = (edit.field("base"), edit.field("orig"));
    std::fs::write(&path, "# Plan\n\nThe café opens in spring. #project\n\none\ntwo\nthree (from the laptop)\n").unwrap();
    let mine = "# Plan (web)\n\nThe café opens in spring. #project\n\none\ntwo\nthree\n";
    let r = b.post("/edit/Projects/Plan.md", &[("_csrf", &csrf), ("base", &base), ("orig", &orig), ("text", mine), ("action", "save")]);
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(r.body.contains("changed while you were editing"));
    let merged = r.field("text");
    assert!(merged.contains("# Plan (web)") && merged.contains("three (from the laptop)"), "{merged}");
    assert!(std::fs::read_to_string(&path).unwrap().contains("# Plan\n"), "nothing written yet");
    let (base2, orig2) = (r.field("base"), r.field("orig"));
    let r =
        b.post("/edit/Projects/Plan.md", &[("_csrf", &csrf), ("base", &base2), ("orig", &orig2), ("text", &merged), ("action", "save")]);
    assert_eq!(r.status, 303);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), merged);

    // The same lines changed on both sides: both kept, and mine can go to a copy.
    let edit = b.get("/edit/Projects/Plan.md");
    let (base, orig) = (edit.field("base"), edit.field("orig"));
    std::fs::write(&path, "# Plan (laptop)\n").unwrap();
    let r = b.post(
        "/edit/Projects/Plan.md",
        &[("_csrf", &csrf), ("base", &base), ("orig", &orig), ("text", "# Plan (phone)\n"), ("action", "save")],
    );
    assert_eq!(r.status, 409);
    assert!(r.body.contains("Save mine as a copy") && r.body.contains("Plan (laptop)") && r.body.contains("Plan (phone)"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan (laptop)\n");
    let r = b.post("/edit/Projects/Plan.md", &[("_csrf", &csrf), ("action", "copy"), ("text", "# Plan (phone)\n")]);
    assert_eq!(r.status, 303);
    let copy = r.header("location").unwrap().to_string();
    assert!(copy.starts_with("/n/Projects/Plan%20%28copy%20from%20the%20web"), "{copy}");
    let copies: Vec<_> =
        std::fs::read_dir(env.root.join("Projects")).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    assert!(
        copies
            .iter()
            .any(|n| n.starts_with("Plan (copy from the web") && Path::new(n).extension().is_some_and(|x| x.eq_ignore_ascii_case("md"))),
        "{copies:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan (laptop)\n");
}

#[test]
fn web_edits_are_atomic_and_kept_in_history_with_who_made_them() {
    let env = start();
    let mut b = Browser::signed_in(&env);
    let csrf = b.csrf();
    // A note with Windows line endings keeps them.
    write(&env.root, "Crlf.md", "# Crlf\r\nline\r\n");
    let edit = b.get("/edit/Crlf.md");
    let r = b.post(
        "/edit/Crlf.md",
        &[
            ("_csrf", &csrf),
            ("base", &edit.field("base")),
            ("orig", &edit.field("orig")),
            ("text", "# Crlf\r\nline\r\nmore\r\n"),
            ("action", "save"),
        ],
    );
    assert_eq!(r.status, 303, "{}", r.body);
    assert_eq!(std::fs::read_to_string(env.root.join("Crlf.md")).unwrap(), "# Crlf\r\nline\r\nmore\r\n");
    let vault = Vault::open_local(&env.root).unwrap();
    let hist = vault.history(&NotePath::new("Crlf.md"));
    assert_eq!(hist.len(), 2, "the version before and the web's");
    let prov = vault.read_provenance(&hist[0]).unwrap();
    assert_eq!(prov["actor"]["kind"], "human");
    assert_eq!(prov["actor"]["via"], "web");
    assert_eq!(prov["actor"]["device_name"], "home server");
    assert_eq!(prov["actor"]["device"], "d_web");
    assert_eq!(vault.read_snapshot(&hist[1]).unwrap(), "# Crlf\r\nline\r\n");
    // No temporary files left behind.
    let stray: Vec<_> =
        std::fs::read_dir(&env.root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".tmp")).collect();
    assert!(stray.is_empty());
    // New notes never replace an existing one.
    let r = b.post("/new", &[("_csrf", &csrf), ("title", "Plan"), ("folder", "Projects")]);
    assert_eq!(r.status, 303);
    assert_eq!(r.header("location"), Some("/edit/Projects/Plan%201.md"));
    assert!(std::fs::read_to_string(env.root.join("Projects/Plan.md")).unwrap().contains("spring"));
    for (title, folder) in [("../x", ""), ("x", "Journal"), ("x", ".quire"), ("x", "../up"), (".hidden", "")] {
        assert_eq!(
            b.post("/new", &[("_csrf", &csrf), ("title", title), ("folder", folder)]).status,
            200,
            "{title} in {folder}: refused, form shown again"
        );
    }
    assert!(!env.root.join("Journal/x.md").exists() && !env.root.join(".quire/x.md").exists());
}

#[test]
fn read_only_mode_changes_nothing() {
    let env = start_with(NOTES, true, |_| {});
    let mut b = Browser::signed_in(&env);
    let csrf = b.csrf();
    let note = b.get("/n/Projects/Plan.md");
    assert!(!note.body.contains("/edit/") && !note.body.contains("New note"));
    assert_eq!(b.get("/edit/Projects/Plan.md").status, 403);
    assert_eq!(b.post("/edit/Projects/Plan.md", &[("_csrf", &csrf), ("text", "x"), ("action", "save")]).status, 403);
    assert_eq!(b.post("/edit/Projects/Plan.md", &[("_csrf", &csrf), ("text", "x"), ("action", "copy")]).status, 403);
    assert_eq!(b.post("/new", &[("_csrf", &csrf), ("title", "New")]).status, 403);
    assert!(!env.root.join("New.md").exists());
    // Agents can't write either, whatever their token allows.
    let token = env.tokens.create_personal("w", Scopes::parse("notes:read,notes:propose,notes:write").unwrap(), None, 0).unwrap();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "create", "arguments": {"path": "wiki/New.md", "text": "# New\n"},
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"}}}});
    let r = into_resp(
        agent()
            .post(&format!("{}/mcp", env.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", &format!("Bearer {token}"))
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "create")
            .send(body.to_string())
            .unwrap(),
    );
    assert!(!env.root.join("wiki/New.md").exists(), "{}", r.body);
    assert!(r.body.contains("isError") || r.status == 403, "{}", r.body);
}

#[test]
fn the_sample_vault_renders() {
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sample-vault");
    let mut files: Vec<(String, String)> = Vec::new();
    let mut todo = vec![sample.clone()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if p.extension().is_some_and(|x| x == "md") {
                let rel = p.strip_prefix(&sample).unwrap().to_string_lossy().replace('\\', "/");
                files.push((rel, std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    assert!(files.len() > 3);
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let env = start_with(&refs, false, |w| w.hide.clear());
    let mut b = Browser::signed_in(&env);
    let mut callouts = 0;
    for (p, _) in &files {
        if p.starts_with('.') {
            continue;
        }
        let url = format!("/n/{}", p.split('/').map(pct).collect::<Vec<_>>().join("/"));
        let r = b.get(&url);
        assert_eq!(r.status, 200, "{p}: {}", r.body);
        assert!(r.body.contains("<article class=\"note\">"), "{p}");
        callouts += r.body.matches("class=\"callout\"").count();
    }
    assert!(callouts > 0, "the sample vault shows callouts");
    assert_eq!(b.get("/tags").status, 200);
    assert_eq!(b.get("/search?q=quire").status, 200);
}

/// 0.10 integration: a voice note plays in an `<audio>` element, an
/// Excalidraw drawing shows as a picture (a hidden one doesn't), and every
/// page says the server is Beta and holds the notes in plain text.
#[test]
fn voice_notes_play_drawings_show_and_pages_say_beta() {
    let drawing = include_str!("../../quire-canvas/testdata/excalidraw/Plain.excalidraw.md");
    let env = start_with(
        &[
            ("Daily.md", "# Daily\n\n![[Voice note 2026-11-05 153012.wav]]\n\n![[Plan.excalidraw]]\n\n![[Private/Secret.excalidraw]]\n"),
            ("Plan.excalidraw.md", drawing),
            ("Private/Secret.excalidraw.md", drawing),
            ("attachments/Voice note 2026-11-05 153012.wav", "RIFF....WAVEfmt "),
        ],
        false,
        |_| {},
    );
    let mut b = Browser::new(&env);
    let login = b.get("/login");
    assert!(login.body.contains("class=\"beta\">Beta</span>"), "{}", login.body);
    assert!(login.body.contains("decrypted copy"), "{}", login.body);
    let mut b = Browser::signed_in(&env);
    let r = b.get("/n/Daily.md");
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.body.contains("<audio controls=\"\" preload=\"metadata\" src=\"/f/attachments/Voice%20note%202026-11-05%20153012.wav\">"),
        "{}",
        r.body
    );
    assert_eq!(r.body.matches("<img src=\"data:image/svg+xml;base64,").count(), 1, "the hidden drawing isn't drawn: {}", r.body);
    assert!(r.body.contains("<footer class=\"foot\">"), "{}", r.body);
    assert!(r.header("content-security-policy").unwrap().contains("media-src 'self'"));
    let wav = b.get("/f/attachments/Voice%20note%202026-11-05%20153012.wav");
    assert_eq!((wav.status, wav.header("content-type")), (200, Some("audio/wav")));
}

/// Serve one JSON document (a client's metadata) on loopback.
fn serve_doc(make: impl Fn(&str) -> String + Send + 'static) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let me = base.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let body = make(&me);
            let mut s = s;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    base
}

#[test]
fn mcp_shares_the_port_and_oauth_is_approved_in_the_web_app() {
    let env = start();
    // Without a token: 401 pointing at the metadata, as with serve-http.
    let r =
        into_resp(agent().post(&format!("{}/mcp", env.base)).header("Accept", "application/json, text/event-stream").send("{}").unwrap());
    assert_eq!(r.status, 401);
    assert!(r.header("www-authenticate").unwrap().contains("/.well-known/oauth-protected-resource"));
    let prm: Value =
        serde_json::from_str(&into_resp(agent().get(&format!("{}/.well-known/oauth-protected-resource", env.base)).call().unwrap()).body)
            .unwrap();
    assert_eq!(prm["resource"], format!("{}/mcp", env.base));

    let doc = serve_doc(|me| {
        json!({"client_id": format!("{me}/client.json"), "client_name": "Claude", "redirect_uris": ["http://127.0.0.1:9/callback"], "token_endpoint_auth_method": "none"}).to_string()
    });
    let client_id = format!("{doc}/client.json");
    let verifier = "v".repeat(50);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirect = "http://127.0.0.1:5555/callback";
    let resource = format!("{}/mcp", env.base);
    let q = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "s1"),
        ("scope", "notes:read"),
        ("resource", resource.as_str()),
    ];
    let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", pct(v))).collect();
    // The person's browser, signed in to the web app, starts the sign-in.
    let mut b = Browser::signed_in(&env);
    let page = b.get(&format!("/oauth/authorize?{}", qs.join("&")));
    assert_eq!(page.status, 200);
    let req = env.consent.pending(quire_app::history_ops::now_ms()).remove(0);
    let link = format!("/consent?request={}", req.id);
    assert!(page.body.contains(&link), "the waiting page links to the web app");
    // Another signed-in browser can approve, but never gets the code.
    let mut other = Browser::signed_in(&env);
    let consent = other.get(&link);
    assert!(consent.body.contains("Claude") && consent.body.contains(req.verify_code()));
    let csrf = other.csrf();
    assert_eq!(other.post("/consent", &[("_csrf", "nope"), ("request", &req.id), ("decision", "approve")]).status, 403);
    let r = other.post("/consent", &[("_csrf", &csrf), ("request", &req.id), ("decision", "approve")]);
    assert_eq!(r.status, 303);
    let wait = r.header("location").unwrap().to_string();
    assert_eq!(wait, format!("/oauth/authorize/wait?request={}", req.id));
    let r = other.get(&wait);
    assert_eq!(r.status, 403, "only the browser that started it");
    assert!(!r.body.contains("code="));
    let r = b.get(&wait);
    assert_eq!(r.status, 303);
    let loc = r.header("location").unwrap().to_string();
    assert!(loc.starts_with(redirect) && loc.contains("code=") && loc.contains("state=s1"), "{loc}");
    let code = loc.split("code=").nth(1).unwrap().split('&').next().unwrap().to_string();
    let form: String = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", resource.as_str()),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", pct(v)))
    .collect::<Vec<_>>()
    .join("&");
    let t = into_resp(
        agent().post(&format!("{}/oauth/token", env.base)).header("Content-Type", "application/x-www-form-urlencoded").send(form).unwrap(),
    );
    assert_eq!(t.status, 200, "{}", t.body);
    let access = serde_json::from_str::<Value>(&t.body).unwrap()["access_token"].as_str().unwrap().to_string();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "search", "arguments": {"query": "welcome"},
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"}}}});
    let r = into_resp(
        agent()
            .post(&format!("{}/mcp", env.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", &format!("Bearer {access}"))
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "search")
            .send(body.to_string())
            .unwrap(),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("Welcome.md"), "{}", r.body);
    // An MCP token is not a web session, and a web session is not an MCP token.
    let r = into_resp(agent().get(&format!("{}/", env.base)).header("Authorization", &format!("Bearer {access}")).call().unwrap());
    assert_eq!(r.status, 303);
    let r = into_resp(
        agent()
            .post(&format!("{}/mcp", env.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Cookie", &b.cookie_header())
            .send("{}")
            .unwrap(),
    );
    assert_eq!(r.status, 401);
}
