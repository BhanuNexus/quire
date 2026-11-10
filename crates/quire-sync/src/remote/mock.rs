// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! An in-process S3 and WebDAV server for tests (feature `mock-server`).
//!
//! Plain `std::net`, one thread per connection, `Connection: close`. It
//! implements what Quire uses, the way the real servers answer it:
//!
//! * **S3** (path-style): `GET`/`HEAD`/`PUT`/`DELETE` of objects,
//!   `ListObjectsV2` with `prefix`, `delimiter`, pages and continuation
//!   tokens, `If-None-Match: *` (412, or 501 `NotImplemented` like B2 when
//!   turned off), and SigV4 checks: the access key, the signature over the
//!   headers as received, the payload hash, and the time (15 minutes).
//! * **WebDAV**: Basic auth, `PROPFIND` depth 1 (207), `MKCOL` (201, 405,
//!   409 without a parent), `PUT` (409 without a parent, `If-None-Match`),
//!   `GET`, `HEAD`, `DELETE`.
//!
//! Faults on demand: failed requests, a cut connection, expired
//! credentials, a skewed clock, and hostile replies.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::sigv4::{Signer, sha256_hex};
use super::xml::percent_decode;

/// What the server speaks.
#[derive(Debug, Clone)]
pub enum Kind {
    S3 { bucket: String, access_key: String, secret_key: String, region: String },
    WebDav { user: String, password: String },
}

/// A hostile reply, for the next requests of a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hostile {
    /// Listings never end (each page is truncated, with a new token).
    EndlessListing,
    /// Listings far over the size limit.
    HugeListing,
    /// Listings that aren't XML.
    BadXml,
    /// An entity-expansion bomb in a listing.
    EntityBomb,
    /// Every request is redirected to another host.
    Redirect,
    /// Listings name `..`, `../x` and absolute paths elsewhere.
    DotDot,
    /// Object bodies far over any limit.
    HugeBody,
}

#[derive(Debug, Default)]
pub struct State {
    /// S3: object keys. WebDAV: paths below `/dav/` (no leading `/`).
    pub objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// WebDAV collections (ending in `/`), `""` being `/dav/` itself.
    pub dirs: Mutex<BTreeSet<String>>,
    /// Honour `If-None-Match: *` (else: S3 answers 501, WebDAV ignores it).
    pub conditional: AtomicBool,
    /// Statuses to answer the next requests with (503, 500…); 0 = drop the connection.
    pub fail_next: Mutex<VecDeque<u16>>,
    pub hostile: Mutex<Option<Hostile>>,
    /// The credentials have expired.
    pub expired: AtomicBool,
    /// The server's clock is this far ahead of this computer's.
    pub skew_secs: AtomicI64,
    /// S3 keys per listing page.
    pub page_size: AtomicUsize,
    pub requests: AtomicU64,
    /// `PUT`s that still succeed before every further one drops its
    /// connection (`u64::MAX`: no limit).
    pub puts_left: AtomicU64,
    /// Requests by method, for cost checks.
    pub methods: Mutex<BTreeMap<String, u64>>,
}

/// A running server; stops when dropped.
#[derive(Debug)]
pub struct MockServer {
    pub port: u16,
    pub state: Arc<State>,
    stop: Arc<AtomicBool>,
}

struct Req {
    method: String,
    /// As received (encoded), without the query.
    raw_path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn resp(status: u16, body: impl Into<Vec<u8>>) -> Resp {
    Resp { status, headers: Vec::new(), body: body.into() }
}

fn s3_error(status: u16, code: &str, msg: &str) -> Resp {
    resp(status, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{msg}</Message></Error>"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl MockServer {
    pub fn start(kind: Kind) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let port = listener.local_addr().expect("local address").port();
        let state = Arc::new(State {
            page_size: AtomicUsize::new(1000),
            conditional: AtomicBool::new(true),
            puts_left: AtomicU64::new(u64::MAX),
            ..State::default()
        });
        state.dirs.lock().unwrap_or_else(PoisonError::into_inner).insert(String::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (Arc::clone(&state), Arc::clone(&stop));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (st, kind) = (Arc::clone(&st), kind.clone());
                std::thread::spawn(move || serve(conn, &st, &kind, port));
            }
        });
        Self { port, state, stop }
    }

    /// `http://127.0.0.1:port`
    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The WebDAV folder URL.
    pub fn dav_url(&self) -> String {
        format!("{}/dav/", self.origin())
    }

    pub fn fail_next(&self, statuses: &[u16]) {
        self.state.fail_next.lock().unwrap_or_else(PoisonError::into_inner).extend(statuses);
    }

    pub fn set_hostile(&self, h: Option<Hostile>) {
        *self.state.hostile.lock().unwrap_or_else(PoisonError::into_inner) = h;
    }

    pub fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        self.state.objects.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn count(&self, method: &str) -> u64 {
        self.state.methods.lock().unwrap_or_else(PoisonError::into_inner).get(method).copied().unwrap_or(0)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn read_req(conn: &TcpStream) -> Option<Req> {
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (raw_path, q) = target.split_once('?').map_or_else(|| (target.clone(), String::new()), |(p, q)| (p.to_string(), q.to_string()));
    let query = q
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k).unwrap_or_default(), percent_decode(v).unwrap_or_default())
        })
        .collect();
    Some(Req { method, raw_path, query, headers, body })
}

fn http_date(skew: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(skew)).format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn serve(mut conn: TcpStream, st: &State, kind: &Kind, port: u16) {
    let Some(req) = read_req(&conn) else { return };
    st.requests.fetch_add(1, Ordering::Relaxed);
    *st.methods.lock().unwrap_or_else(PoisonError::into_inner).entry(req.method.clone()).or_default() += 1;
    if req.method == "PUT" {
        let left = st.puts_left.load(Ordering::Relaxed);
        if left == 0 {
            return;
        }
        if left != u64::MAX {
            st.puts_left.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let fail = st.fail_next.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
    let res = match fail {
        Some(0) => return, // the connection drops
        Some(s) => resp(s, "busy"),
        None => {
            let hostile = *st.hostile.lock().unwrap_or_else(PoisonError::into_inner);
            if hostile == Some(Hostile::Redirect) {
                Resp { status: 307, headers: vec![("Location".into(), "https://elsewhere.example/steal".into())], body: Vec::new() }
            } else {
                match kind {
                    Kind::S3 { .. } => s3(st, kind, &req, port, hostile),
                    Kind::WebDav { .. } => dav(st, kind, &req, port, hostile),
                }
            }
        }
    };
    let mut head =
        format!("HTTP/1.1 {} X\r\nConnection: close\r\nDate: {}\r\n", res.status, http_date(st.skew_secs.load(Ordering::Relaxed)));
    let len = res
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map_or_else(|| res.body.len().to_string(), |(_, v)| v.clone());
    let _ = write!(head, "Content-Length: {len}\r\n");
    for (k, v) in &res.headers {
        if !k.eq_ignore_ascii_case("content-length") {
            let _ = write!(head, "{k}: {v}\r\n");
        }
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    if req.method != "HEAD" {
        let _ = conn.write_all(&res.body);
    }
    let _ = conn.flush();
}

/// Check a SigV4 request as S3 would.
fn s3_auth(st: &State, req: &Req, access: &str, secret: &str, region: &str, port: u16) -> Result<(), Resp> {
    let auth = req.header("authorization").ok_or_else(|| s3_error(403, "AccessDenied", "no signature"))?;
    let cred = auth.split("Credential=").nth(1).and_then(|s| s.split(',').next()).unwrap_or_default();
    let mut parts = cred.split('/');
    let (ak, date, reg) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    if ak != access {
        return Err(s3_error(403, "InvalidAccessKeyId", "The AWS Access Key Id you provided does not exist in our records."));
    }
    if st.expired.load(Ordering::Relaxed) {
        return Err(s3_error(400, "ExpiredToken", "The provided token has expired."));
    }
    if reg != region {
        return Err(s3_error(400, "AuthorizationHeaderMalformed", &format!("the region '{reg}' is wrong; expecting '{region}'")));
    }
    let amz_date = req.header("x-amz-date").unwrap_or_default();
    let when = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ").map_or(0, |d| d.and_utc().timestamp());
    let now = chrono::Utc::now().timestamp() + st.skew_secs.load(Ordering::Relaxed);
    if (now - when).abs() > 15 * 60 || !amz_date.starts_with(date) {
        return Err(s3_error(403, "RequestTimeTooSkewed", "The difference between the request time and the current time is too large."));
    }
    let payload = req.header("x-amz-content-sha256").unwrap_or_default();
    if payload != sha256_hex(&req.body) {
        return Err(s3_error(
            400,
            "XAmzContentSHA256Mismatch",
            "The provided 'x-amz-content-sha256' header does not match what was computed.",
        ));
    }
    let signed: Vec<&str> = auth.split("SignedHeaders=").nth(1).and_then(|s| s.split(',').next()).unwrap_or_default().split(';').collect();
    if !signed.contains(&"host") || !signed.contains(&"x-amz-date") {
        return Err(s3_error(403, "AccessDenied", "host and x-amz-date must be signed"));
    }
    let headers: Vec<(String, String)> = signed.iter().map(|h| ((*h).to_string(), req.header(h).unwrap_or_default().to_string())).collect();
    if req.header("host") != Some(&format!("127.0.0.1:{port}")) {
        return Err(s3_error(400, "InvalidRequest", "wrong Host"));
    }
    let signer = Signer { access_key: access, secret_key: secret, region, service: "s3" };
    let r = super::sigv4::Request {
        method: &req.method,
        path: &req.raw_path,
        query: &req.query,
        headers: &headers,
        payload_hash: payload,
        amz_date,
    };
    let want = signer.signature(&r);
    if auth.split("Signature=").nth(1) != Some(want.as_str()) {
        return Err(s3_error(
            403,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        ));
    }
    Ok(())
}

fn etag(b: &[u8]) -> String {
    format!("\"{}\"", &sha256_hex(b)[..32])
}

fn s3(st: &State, kind: &Kind, req: &Req, port: u16, hostile: Option<Hostile>) -> Resp {
    let Kind::S3 { bucket, access_key, secret_key, region } = kind else { return resp(500, "") };
    if let Err(r) = s3_auth(st, req, access_key, secret_key, region, port) {
        return r;
    }
    let Some(rest) = req.raw_path.strip_prefix(&format!("/{bucket}")) else {
        return s3_error(404, "NoSuchBucket", "The specified bucket does not exist");
    };
    let Some(key) = percent_decode(rest.trim_start_matches('/')) else { return s3_error(400, "InvalidURI", "bad escape") };
    let mut objects = st.objects.lock().unwrap_or_else(PoisonError::into_inner);
    let q = |name: &str| req.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    match (req.method.as_str(), key.is_empty()) {
        ("GET", true) if q("list-type").as_deref() == Some("2") => {
            match hostile {
                Some(Hostile::BadXml) => return resp(200, "<ListBucketResult><Contents><Key>"),
                Some(Hostile::EntityBomb) => {
                    return resp(
                        200,
                        r#"<?xml version="1.0"?><!DOCTYPE b [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">]><ListBucketResult><Contents><Key>&b;</Key></Contents></ListBucketResult>"#,
                    );
                }
                Some(Hostile::HugeListing) => {
                    let mut s = String::from("<ListBucketResult>");
                    while s.len() < 9 * 1024 * 1024 {
                        s.push_str("<Contents><Key>quire-sync/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/x</Key></Contents>");
                    }
                    s.push_str("</ListBucketResult>");
                    return resp(200, s);
                }
                Some(Hostile::EndlessListing) => {
                    let token = format!("t{}", st.requests.load(Ordering::Relaxed));
                    return resp(
                        200,
                        format!(
                            "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>{token}</NextContinuationToken><Contents><Key>{}x{token}</Key></Contents></ListBucketResult>",
                            q("prefix").unwrap_or_default()
                        ),
                    );
                }
                _ => {}
            }
            let prefix = q("prefix").unwrap_or_default();
            let delim = q("delimiter");
            let after = q("continuation-token").unwrap_or_default();
            let page = st.page_size.load(Ordering::Relaxed).min(q("max-keys").and_then(|m| m.parse().ok()).unwrap_or(1000)).max(1);
            let mut items: Vec<(String, bool)> = Vec::new();
            let mut seen_prefixes = BTreeSet::new();
            for k in objects.keys().filter(|k| k.starts_with(&prefix)) {
                let rest = &k[prefix.len()..];
                match (delim.as_deref(), rest.find('/')) {
                    (Some("/"), Some(i)) => {
                        let p = format!("{prefix}{}/", &rest[..i]);
                        if seen_prefixes.insert(p.clone()) {
                            items.push((p, true));
                        }
                    }
                    _ => items.push((k.clone(), false)),
                }
            }
            items.retain(|(k, _)| after.is_empty() || *k > after);
            let truncated = items.len() > page;
            items.truncate(page);
            let mut x = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
            );
            let _ = write!(x, "<Name>{bucket}</Name><Prefix>{}</Prefix><IsTruncated>{truncated}</IsTruncated>", xml_escape(&prefix));
            if hostile == Some(Hostile::DotDot) {
                let _ = write!(x, "<CommonPrefixes><Prefix>{}../</Prefix></CommonPrefixes>", xml_escape(&prefix));
                let _ = write!(x, "<Contents><Key>{}..</Key><ETag>\"x\"</ETag><Size>1</Size></Contents>", xml_escape(&prefix));
                x.push_str("<Contents><Key>/etc/passwd</Key></Contents>");
            }
            for (k, dir) in &items {
                if *dir {
                    let _ = write!(x, "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", xml_escape(k));
                } else {
                    let v = &objects[k];
                    let _ = write!(x, "<Contents><Key>{}</Key><ETag>{}</ETag><Size>{}</Size></Contents>", xml_escape(k), etag(v), v.len());
                }
            }
            if truncated && let Some((last, _)) = items.last() {
                let _ = write!(x, "<NextContinuationToken>{}</NextContinuationToken>", xml_escape(last));
            }
            x.push_str("</ListBucketResult>");
            resp(200, x)
        }
        ("GET" | "HEAD", false) => match objects.get(&key) {
            Some(_) if hostile == Some(Hostile::HugeBody) => Resp { status: 200, headers: Vec::new(), body: vec![b'x'; 70 * 1024 * 1024] },
            Some(v) => Resp { status: 200, headers: vec![("ETag".into(), etag(v))], body: v.clone() },
            None => s3_error(404, "NoSuchKey", "The specified key does not exist."),
        },
        ("PUT", false) => {
            if req.header("if-none-match") == Some("*") {
                if !st.conditional.load(Ordering::Relaxed) {
                    return s3_error(
                        501,
                        "NotImplemented",
                        "A header you provided implies functionality that is not implemented: If-None-Match",
                    );
                }
                if objects.contains_key(&key) {
                    return s3_error(412, "PreconditionFailed", "At least one of the pre-conditions you specified did not hold");
                }
            }
            let e = etag(&req.body);
            objects.insert(key, req.body.clone());
            Resp { status: 200, headers: vec![("ETag".into(), e)], body: Vec::new() }
        }
        ("DELETE", false) => {
            objects.remove(&key);
            resp(204, "")
        }
        _ => s3_error(405, "MethodNotAllowed", "nope"),
    }
}

fn dav(st: &State, kind: &Kind, req: &Req, port: u16, hostile: Option<Hostile>) -> Resp {
    use base64::Engine as _;
    let Kind::WebDav { user, password } = kind else { return resp(500, "") };
    let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")));
    if req.header("authorization") != Some(want.as_str()) || st.expired.load(Ordering::Relaxed) {
        return Resp { status: 401, headers: vec![("WWW-Authenticate".into(), "Basic realm=\"dav\"".into())], body: Vec::new() };
    }
    let Some(path) = percent_decode(&req.raw_path) else { return resp(400, "") };
    let Some(rel) = path.strip_prefix("/dav/").or_else(|| (path == "/dav").then_some("")) else { return resp(404, "") };
    if rel.split('/').any(|s| s == "..") {
        return resp(400, "");
    }
    let mut objects = st.objects.lock().unwrap_or_else(PoisonError::into_inner);
    let mut dirs = st.dirs.lock().unwrap_or_else(PoisonError::into_inner);
    let parent = |p: &str| -> String {
        let t = p.trim_end_matches('/');
        t.rfind('/').map_or(String::new(), |i| format!("{}/", &t[..i]))
    };
    match req.method.as_str() {
        "PROPFIND" => {
            let dir = if rel.is_empty() || rel.ends_with('/') { rel.to_string() } else { format!("{rel}/") };
            let dir = if dir == "/" { String::new() } else { dir };
            if !dirs.contains(&dir) {
                return resp(404, "");
            }
            match hostile {
                Some(Hostile::BadXml) => return resp(207, "<d:multistatus xmlns:d=\"DAV:\"><d:response>"),
                Some(Hostile::EntityBomb) => {
                    return resp(
                        207,
                        r#"<?xml version="1.0"?><!DOCTYPE b [<!ENTITY a "aaaa">]><d:multistatus xmlns:d="DAV:"><d:response><d:href>&a;</d:href></d:response></d:multistatus>"#,
                    );
                }
                Some(Hostile::HugeListing) => {
                    let mut s = String::from("<d:multistatus xmlns:d=\"DAV:\">");
                    while s.len() < 17 * 1024 * 1024 {
                        s.push_str("<d:response><d:href>/dav/x</d:href></d:response>");
                    }
                    s.push_str("</d:multistatus>");
                    return resp(207, s);
                }
                _ => {}
            }
            let enc = |s: &str| super::sigv4::encode_path(s);
            let mut x = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><d:multistatus xmlns:d=\"DAV:\">");
            let entry = |x: &mut String, href: &str, is_dir: bool, body: Option<&Vec<u8>>| {
                let rt = if is_dir { "<d:resourcetype><d:collection/></d:resourcetype>" } else { "<d:resourcetype/>" };
                let props = body.map_or(String::new(), |b| {
                    format!("<d:getetag>{}</d:getetag><d:getcontentlength>{}</d:getcontentlength>", etag(b), b.len())
                });
                let _ = write!(
                    x,
                    "<d:response><d:href>{}</d:href><d:propstat><d:prop>{rt}{props}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    xml_escape(href)
                );
            };
            entry(&mut x, &enc(&format!("/dav/{dir}")), true, None);
            if hostile == Some(Hostile::DotDot) {
                entry(&mut x, &format!("/dav/{dir}%2e%2e"), false, Some(&b"x".to_vec()));
                entry(
                    &mut x,
                    &format!("http://evil.example:{port}/dav/{dir}0123456789abcdef0123456789abcdef"),
                    false,
                    Some(&b"x".to_vec()),
                );
                entry(&mut x, "/etc/passwd", false, None);
            }
            for d in dirs.iter().filter(|d| !d.is_empty() && parent(d) == dir) {
                entry(&mut x, &enc(&format!("/dav/{d}")), true, None);
            }
            for (k, v) in objects.iter().filter(|(k, _)| parent(k) == dir) {
                entry(&mut x, &format!("http://127.0.0.1:{port}{}", enc(&format!("/dav/{k}"))), false, Some(v));
            }
            x.push_str("</d:multistatus>");
            resp(207, x)
        }
        "MKCOL" => {
            let d = format!("{}/", rel.trim_end_matches('/'));
            if dirs.contains(&d) || objects.contains_key(rel.trim_end_matches('/')) {
                resp(405, "")
            } else if !dirs.contains(&parent(&d)) {
                resp(409, "")
            } else {
                dirs.insert(d);
                resp(201, "")
            }
        }
        "PUT" => {
            if !dirs.contains(&parent(rel)) {
                return resp(409, "");
            }
            if req.header("if-none-match") == Some("*") && st.conditional.load(Ordering::Relaxed) && objects.contains_key(rel) {
                return resp(412, "");
            }
            let created = objects.insert(rel.to_string(), req.body.clone()).is_none();
            resp(if created { 201 } else { 204 }, "")
        }
        "GET" | "HEAD" => match objects.get(rel) {
            Some(_) if hostile == Some(Hostile::HugeBody) => resp(200, vec![b'x'; 70 * 1024 * 1024]),
            Some(v) => Resp { status: 200, headers: vec![("ETag".into(), etag(v))], body: v.clone() },
            None => resp(404, ""),
        },
        "DELETE" => {
            objects.remove(rel);
            resp(204, "")
        }
        _ => resp(405, ""),
    }
}
