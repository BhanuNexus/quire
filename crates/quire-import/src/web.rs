// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Web capture: fetch a page, keep its article, convert it to Markdown
//! (design §B.3).
//!
//! 1. GET over http or https only: at most 10 MB, 20 s and 5 redirects.
//!    Private, loopback and link-local addresses are refused unless
//!    [`FetchOptions::allow_private`] is set, at every redirect and when
//!    connecting (so a name can't switch to a private address in between).
//!    MCP clients can trigger captures, so this is SSRF defence.
//! 2. The charset comes from `Content-Type`, a byte-order mark or `<meta>`.
//! 3. A Readability port (`dom_smoothie`) keeps the article (title, byline,
//!    published time, content), falling back to `<body>`.
//! 4. `htmd` converts it to Markdown; links and images are made absolute.
//! 5. Up to 20 images of at most 5 MB each are downloaded (or decoded from
//!    `data:` URIs) and deduplicated by SHA-256. SVG is kept as a file and
//!    never inlined.
//!
//! This module has no vault: the caller writes the note and images (the MCP
//! tool and the CLI go through the agent store as `import:url`).

use std::fmt::Write as _;
use std::io::Read as _;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Limits for one capture.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Allow private, loopback and link-local addresses (off by default).
    pub allow_private: bool,
    pub max_page_bytes: u64,
    pub max_image_bytes: u64,
    pub max_images: usize,
    pub max_redirects: u32,
    /// For the page and for each image.
    pub timeout: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            allow_private: false,
            max_page_bytes: 10 * 1024 * 1024,
            max_image_bytes: 5 * 1024 * 1024,
            max_images: 20,
            max_redirects: 5,
            timeout: Duration::from_secs(20),
        }
    }
}

/// Why a capture failed. [`WebError::code`] is stable for tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    /// Not an http(s) URL, or one with a user name or password in it.
    BadUrl(String),
    /// The address is private, loopback or link-local.
    Blocked(String),
    TooLarge(u64),
    TooManyRedirects,
    /// HTTP status other than 2xx.
    Status(u16),
    /// Not HTML or text.
    NotAPage(String),
    Network(String),
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadUrl(u) => write!(f, "{u} isn't an http or https address Quire can capture"),
            Self::Blocked(h) => write!(f, "{h} is a private or local address; Quire only fetches public web pages"),
            Self::TooLarge(n) => write!(f, "the page is larger than {} MB", n / (1024 * 1024)),
            Self::TooManyRedirects => f.write_str("the page redirects too many times"),
            Self::Status(s) => write!(f, "the server answered HTTP {s}"),
            Self::NotAPage(t) => write!(f, "the address is {t}, not a web page"),
            Self::Network(m) => write!(f, "couldn't fetch the page: {m}"),
        }
    }
}

impl std::error::Error for WebError {}

impl WebError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BadUrl(_) => "invalid_args",
            Self::Blocked(_) => "blocked_address",
            Self::TooLarge(_) => "too_large",
            Self::TooManyRedirects => "too_many_redirects",
            Self::Status(_) => "http_error",
            Self::NotAPage(_) => "not_a_page",
            Self::Network(_) => "network",
        }
    }
}

// ------------------------------------------------------------------ URLs

/// The parts of an absolute http(s) URL that matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUrl {
    pub https: bool,
    /// Lowercase; IPv6 without brackets.
    pub host: String,
    pub port: u16,
    /// Path, query and fragment (`/a/b?c#d`), at least `/`.
    pub rest: String,
}

impl HttpUrl {
    pub fn parse(s: &str) -> Result<Self, WebError> {
        let bad = || WebError::BadUrl(s.to_string());
        let t = s.trim();
        let (https, after) = if let Some(r) = strip_prefix_ci(t, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(t, "http://") {
            (false, r)
        } else {
            return Err(bad());
        };
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let (authority, rest) = after.split_at(end);
        if authority.is_empty() || authority.contains('@') || t.chars().any(|c| c.is_control() || c == ' ') {
            return Err(bad());
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, tail) = v6.split_once(']').ok_or_else(bad)?;
            (h.to_string(), tail.strip_prefix(':').map(str::to_string))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p.to_string())),
                None => (authority.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => p.parse::<u16>().map_err(|_| bad())?,
            None if https => 443,
            None => 80,
        };
        if host.is_empty() {
            return Err(bad());
        }
        let rest = if rest.is_empty() {
            "/".to_string()
        } else if rest.starts_with('/') {
            rest.to_string()
        } else {
            format!("/{rest}")
        };
        Ok(Self { https, host: host.to_lowercase(), port, rest })
    }

    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let default = if self.https { 443 } else { 80 };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == default { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{}", self.port) }
    }

    pub fn as_string(&self) -> String {
        format!("{}{}", self.origin(), self.rest)
    }

    /// The site name for a file name: the host without `www.`.
    pub fn site(&self) -> String {
        self.host.trim_start_matches("www.").to_string()
    }

    /// Resolve `href` against this URL (RFC 3986, enough for links in pages).
    pub fn join(&self, href: &str) -> String {
        let h = href.trim();
        if h.is_empty() {
            return self.as_string();
        }
        if let Some(scheme_end) = h.find(':')
            && h[..scheme_end].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            && h[..scheme_end].chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        {
            return h.to_string();
        }
        if let Some(r) = h.strip_prefix("//") {
            return format!("{}://{r}", if self.https { "https" } else { "http" });
        }
        let path_end = self.rest.find(['?', '#']).unwrap_or(self.rest.len());
        let path = &self.rest[..path_end];
        if h.starts_with('#') {
            let no_frag = self.rest.split('#').next().unwrap_or("/");
            return format!("{}{no_frag}{h}", self.origin());
        }
        if h.starts_with('?') {
            return format!("{}{path}{h}", self.origin());
        }
        let (href_path, tail) = h.find(['?', '#']).map_or((h, ""), |i| h.split_at(i));
        let joined = if href_path.starts_with('/') {
            href_path.to_string()
        } else {
            format!("{}{href_path}", &path[..=path.rfind('/').unwrap_or(0)])
        };
        let mut out: Vec<&str> = Vec::new();
        let segs: Vec<&str> = joined.split('/').collect();
        for (i, seg) in segs.iter().enumerate() {
            match *seg {
                "." => {
                    if i == segs.len() - 1 {
                        out.push("");
                    }
                }
                ".." => {
                    if out.len() > 1 {
                        out.pop();
                    }
                    if i == segs.len() - 1 {
                        out.push("");
                    }
                }
                s => out.push(s),
            }
        }
        let mut p = out.join("/");
        if !p.starts_with('/') {
            p.insert(0, '/');
        }
        format!("{}{p}{tail}", self.origin())
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &s[prefix.len()..])
}

// ------------------------------------------------------------- addresses

/// Addresses a capture may not reach: loopback, private, link-local,
/// shared (CGNAT), unspecified, multicast, broadcast and reserved ranges,
/// and IPv6 forms of them.
pub fn is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                || o[0] >= 240
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_forbidden(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xffc0) == 0xfec0
                // IPv4-compatible (::a.b.c.d), NAT64 (64:ff9b::/96) and 6to4 (2002::/16) embed IPv4 addresses.
                || (seg[..6] == [0; 6])
                || (seg[0] == 0x64 && seg[1] == 0xff9b)
                || seg[0] == 0x2002
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)
        }
    }
}

/// Resolve `host:port` and check every address.
fn check_host(host: &str, port: u16, allow_private: bool) -> Result<Vec<SocketAddr>, WebError> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| WebError::Network(format!("{host}: {e}")))?.collect();
    if addrs.is_empty() {
        return Err(WebError::Network(format!("{host} has no address")));
    }
    if !allow_private && addrs.iter().any(|a| is_forbidden(a.ip())) {
        return Err(WebError::Blocked(host.to_string()));
    }
    Ok(addrs)
}

/// A resolver that refuses forbidden addresses when connecting, so DNS
/// can't hand out a private address after the check.
#[derive(Debug)]
struct GuardResolver {
    allow_private: bool,
}

impl ureq::unversioned::resolver::Resolver for GuardResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = ureq::unversioned::resolver::DefaultResolver::default().resolve(uri, config, timeout)?;
        if !self.allow_private && addrs.iter().any(|a| is_forbidden(a.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "blocked private address")));
        }
        Ok(addrs)
    }
}

// ----------------------------------------------------------------- fetch

/// A fetched resource.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// After redirects.
    pub url: HttpUrl,
    pub content_type: String,
    /// The `Cache-Control` header, for callers that cache.
    pub cache_control: String,
    pub bytes: Vec<u8>,
}

fn agent(opts: &FetchOptions, direct: bool, timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(format!("Quire/{} (+capture)", env!("CARGO_PKG_VERSION")))
        // A proxy would connect for us, past the connect-time check; the
        // pre-check below still applies, and local addresses never use one.
        .proxy(if direct { None } else { ureq::Proxy::try_from_env() })
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .build();
    ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::default(),
        GuardResolver { allow_private: opts.allow_private },
    )
}

/// GET `url` following up to `max_redirects` redirects, each checked.
pub fn fetch(url: &str, opts: &FetchOptions, limit: u64) -> Result<Fetched, WebError> {
    let started = Instant::now();
    let mut current = HttpUrl::parse(url)?;
    for _ in 0..=opts.max_redirects {
        let addrs = check_host(&current.host, current.port, opts.allow_private)?;
        let local = addrs.iter().any(|a| is_forbidden(a.ip()));
        let left = opts.timeout.checked_sub(started.elapsed()).ok_or_else(|| WebError::Network("timed out".into()))?;
        let mut res = agent(opts, local, left).get(current.as_string()).call().map_err(|e| match e {
            ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => WebError::Blocked(current.host.clone()),
            other => WebError::Network(other.to_string()),
        })?;
        let status = res.status().as_u16();
        if (300..400).contains(&status) && status != 304 {
            let loc = res.headers().get("location").and_then(|v| v.to_str().ok()).ok_or(WebError::Status(status))?;
            current = HttpUrl::parse(&current.join(loc))?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(WebError::Status(status));
        }
        if let Some(len) = res.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok())
            && len > limit
        {
            return Err(WebError::TooLarge(limit));
        }
        let header = |n: &str| res.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let (content_type, cache_control) = (header("content-type"), header("cache-control"));
        let mut bytes = Vec::new();
        res.body_mut().as_reader().take(limit + 1).read_to_end(&mut bytes).map_err(|e| WebError::Network(e.to_string()))?;
        if bytes.len() as u64 > limit {
            return Err(WebError::TooLarge(limit));
        }
        return Ok(Fetched { url: current, content_type, cache_control, bytes });
    }
    Err(WebError::TooManyRedirects)
}

// ---------------------------------------------------------------- decode

/// Decode a page with the charset from `Content-Type`, a BOM or `<meta>`.
pub fn decode(bytes: &[u8], content_type: &str) -> String {
    let from_header = charset_param(content_type);
    let enc = from_header
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .or_else(|| encoding_rs::Encoding::for_bom(bytes).map(|(e, _)| e))
        .or_else(|| meta_charset(bytes).and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())))
        .unwrap_or(encoding_rs::UTF_8);
    enc.decode(bytes).0.into_owned()
}

fn charset_param(ct: &str) -> Option<String> {
    ct.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches(['"', '\'']).to_string())
    })
}

/// `<meta charset=…>` or `<meta http-equiv="content-type" content="…; charset=…">`
/// in the first 4 KB.
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_lowercase();
    let i = head.find("charset=")?;
    let rest = head[i + 8..].trim_start_matches(['"', '\'']);
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

// --------------------------------------------------------------- extract

/// What a page says, as Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub title: String,
    pub byline: Option<String>,
    pub published: Option<String>,
    pub site_name: Option<String>,
    /// Markdown; links and image sources absolute.
    pub markdown: String,
    /// The extractor found no article and kept the whole body.
    pub whole_page: bool,
}

fn converter() -> htmd::HtmlToMarkdown {
    htmd::HtmlToMarkdown::builder()
        .options(htmd::options::Options {
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            hr_style: htmd::options::HrStyle::Dashes,
            ..htmd::options::Options::default()
        })
        .skip_tags(vec![
            "script", "style", "noscript", "iframe", "object", "embed", "form", "button", "input", "select", "textarea", "svg", "template",
        ])
        .build()
}

/// Keep the article of `html` (from `url`) as Markdown.
pub fn extract(html: &str, url: &HttpUrl) -> Article {
    let page_url = url.as_string();
    let parsed = dom_smoothie::Readability::new(html, Some(page_url.as_str()), None).and_then(|mut r| r.parse());
    let (title, byline, published, site_name, content, whole_page) = match parsed {
        Ok(a) if a.length > 0 => (a.title, a.byline, a.published_time, a.site_name, a.content.to_string(), false),
        _ => {
            let body = html
                .find("<body")
                .and_then(|i| html[i..].find('>').map(|j| i + j + 1))
                .map_or(html, |start| &html[start..html.rfind("</body>").filter(|&e| e >= start).unwrap_or(html.len())]);
            (title_tag(html).unwrap_or_default(), None, None, None, body.to_string(), true)
        }
    };
    let md = converter().convert(&content).unwrap_or_default();
    let markdown = absolutize(&tidy(&md), url);
    let title = if title.trim().is_empty() { url.site() } else { clean_title(&collapse_ws(&title), site_name.as_deref(), &url.site()) };
    Article { title, byline: byline.map(|b| collapse_ws(&b)), published, site_name, markdown, whole_page }
}

/// "Ropes | Example Blog" → "Ropes" when the other part is the site's name.
fn clean_title(title: &str, site_name: Option<&str>, host: &str) -> String {
    for sep in [" | ", " – ", " — ", " - ", " · ", " :: ", " » "] {
        if let Some((a, b)) = title.rsplit_once(sep) {
            let b = b.trim().to_lowercase();
            let is_site = site_name.is_some_and(|s| s.trim().to_lowercase() == b) || host.to_lowercase().starts_with(&b.replace(' ', ""));
            if is_site && !a.trim().is_empty() {
                return a.trim().to_string();
            }
        }
    }
    title.to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title_tag(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close = lower[open_end..].find("</title>")? + open_end;
    Some(collapse_ws(&html[open_end..close]))
}

/// Trailing spaces off, at most one blank line in a row, one final newline.
fn tidy(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank = 0;
    let mut in_fence = false;
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let l = if in_fence { line } else { line.trim_end() };
        if l.trim().is_empty() && !in_fence {
            blank += 1;
            if blank > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Spans of `[text](target)` and `![alt](target)`: (start of `(`+1, end before `)`).
fn link_targets(md: &str) -> Vec<(usize, usize, bool)> {
    let b = md.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b']' && b[i + 1] == b'(' {
            let image = md[..i].rfind('[').is_some_and(|o| o > 0 && b[o - 1] == b'!');
            let start = i + 2;
            let mut depth = 0;
            let mut j = start;
            while j < b.len() {
                match b[j] {
                    b'(' => depth += 1,
                    b')' if depth == 0 => break,
                    b')' => depth -= 1,
                    b'\n' => break,
                    _ => {}
                }
                j += 1;
            }
            if j < b.len() && b[j] == b')' {
                out.push((start, j, image));
                i = j;
            }
        }
        i += 1;
    }
    out
}

/// The URL part of a link target (`url "title"` or `<url>`).
fn target_url(t: &str) -> &str {
    let t = t.trim();
    if let Some(r) = t.strip_prefix('<') {
        return r.split('>').next().unwrap_or(r);
    }
    t.split_whitespace().next().unwrap_or(t)
}

/// Make every link and image target absolute.
fn absolutize(md: &str, base: &HttpUrl) -> String {
    let mut out = String::with_capacity(md.len());
    let mut last = 0;
    for (s, e, _) in link_targets(md) {
        let target = &md[s..e];
        let url = target_url(target);
        if url.starts_with("data:") || url.starts_with('#') {
            continue;
        }
        let abs = base.join(url);
        if abs != url {
            out.push_str(&md[last..s]);
            out.push_str(&target.replacen(url, &abs, 1));
            last = e;
        }
    }
    out.push_str(&md[last..]);
    out
}

// ---------------------------------------------------------------- images

/// An image the page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// As written in the Markdown (absolute URL or `data:` URI).
    pub src: String,
    pub bytes: Vec<u8>,
    /// `png`, `jpg`, `gif`, `webp`, `svg`, `avif`, `bmp`, `ico`.
    pub ext: &'static str,
    /// Hex SHA-256 of the bytes.
    pub sha256: String,
    /// A file name from the URL (without extension), for the attachment.
    pub name: String,
}

/// The image type of `bytes` by their magic number (SVG by its text).
pub fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    let starts = |m: &[u8]| bytes.starts_with(m);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if starts(b"\xff\xd8\xff") {
        Some("jpg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.len() > 12 && &bytes[4..8] == b"ftyp" && (&bytes[8..12] == b"avif" || &bytes[8..12] == b"avis") {
        Some("avif")
    } else if starts(b"BM") {
        Some("bmp")
    } else if starts(b"\0\0\x01\0") {
        Some("ico")
    } else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_lowercase();
        head.contains("<svg").then_some("svg")
    }
}

/// Decode a base64 `data:` image URI.
pub fn decode_data_uri(uri: &str, max: u64) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.to_lowercase().starts_with("image/") {
        return None;
    }
    let bytes = if meta.to_lowercase().ends_with(";base64") {
        base64::engine::general_purpose::STANDARD.decode(data.trim().as_bytes()).ok()?
    } else {
        percent_decode(data)
    };
    (bytes.len() as u64 <= max).then_some(bytes)
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn image_name(src: &str, ext: &str) -> String {
    if src.starts_with("data:") {
        return "image".into();
    }
    let path = src.split(['?', '#']).next().unwrap_or(src);
    let file = path.rsplit('/').next().unwrap_or("image");
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let clean: String = stem.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(60).collect();
    let clean = clean.trim_matches('-').to_string();
    if clean.is_empty() { format!("image-{ext}") } else { clean }
}

// --------------------------------------------------------------- capture

/// A captured page: the article, its images, and what went wrong on the way.
#[derive(Debug, Clone)]
pub struct Captured {
    pub url: HttpUrl,
    pub article: Article,
    /// Unique by content; `Article::markdown` still names them by `src`.
    pub images: Vec<Image>,
    pub warnings: Vec<String>,
}

/// Fetch `url` and everything it shows, within `opts`.
pub fn capture(url: &str, opts: &FetchOptions) -> Result<Captured, WebError> {
    let page = fetch(url, opts, opts.max_page_bytes)?;
    let ct = page.content_type.to_lowercase();
    let mime = ct.split(';').next().unwrap_or_default().trim().to_string();
    let article = match mime.as_str() {
        "" | "text/html" | "application/xhtml+xml" => extract(&decode(&page.bytes, &page.content_type), &page.url),
        "text/plain" | "text/markdown" | "text/x-markdown" => {
            let text = decode(&page.bytes, &page.content_type);
            let title = page.url.rest.rsplit('/').find(|s| !s.is_empty()).map_or_else(|| page.url.site(), str::to_string);
            let markdown =
                if mime == "text/plain" { format!("```text\n{}\n```\n", text.trim_end().replace("```", "ʼʼʼ")) } else { text };
            Article { title, byline: None, published: None, site_name: None, markdown, whole_page: true }
        }
        other => return Err(WebError::NotAPage(other.to_string())),
    };
    let mut images: Vec<Image> = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_src = std::collections::HashSet::new();
    for (s, e, is_image) in link_targets(&article.markdown) {
        if !is_image {
            continue;
        }
        let src = target_url(&article.markdown[s..e]).to_string();
        if !seen_src.insert(src.clone()) {
            continue;
        }
        if images.len() >= opts.max_images {
            warnings.push(format!("Only the first {} images were saved.", opts.max_images));
            break;
        }
        let bytes = if src.starts_with("data:") {
            let Some(b) = decode_data_uri(&src, opts.max_image_bytes) else {
                warnings.push("An embedded image couldn't be decoded or is too large.".into());
                continue;
            };
            b
        } else {
            match fetch(&src, &FetchOptions { timeout: opts.timeout.min(Duration::from_secs(10)), ..opts.clone() }, opts.max_image_bytes) {
                Ok(f) => f.bytes,
                Err(e) => {
                    warnings.push(format!("Image {src}: {e}."));
                    continue;
                }
            }
        };
        let Some(ext) = image_ext(&bytes) else {
            warnings.push(format!("{} isn't an image.", if src.starts_with("data:") { "An embedded file" } else { src.as_str() }));
            continue;
        };
        let sha256: String = Sha256::digest(&bytes).iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
        let name = image_name(&src, ext);
        images.push(Image { src, bytes, ext, sha256, name });
    }
    Ok(Captured { url: page.url, article, images, warnings })
}

/// Replace each image `![alt](src)` whose `src` is in `names` by `![[file]]`
/// (keeping the alt text as the embed's display text when there is one).
pub fn embed_images<S: std::hash::BuildHasher>(md: &str, names: &std::collections::HashMap<String, String, S>) -> String {
    let mut out = String::with_capacity(md.len());
    let mut last = 0;
    for (s, e, is_image) in link_targets(md) {
        if !is_image {
            continue;
        }
        let src = target_url(&md[s..e]);
        let Some(file) = names.get(src) else { continue };
        let Some(open) = md[..s].rfind("![") else { continue };
        let alt = md[open + 2..s - 2].trim();
        out.push_str(&md[last..open]);
        if alt.is_empty() || alt.contains(['[', ']', '|']) {
            let _ = write!(out, "![[{file}]]");
        } else {
            let _ = write!(out, "![[{file}|{alt}]]");
        }
        last = e + 1;
    }
    out.push_str(&md[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_and_join() {
        let u = HttpUrl::parse("HTTPS://Example.com:8443/a/b/c.html?x=1#f").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.rest.as_str()), (true, "example.com", 8443, "/a/b/c.html?x=1#f"));
        assert_eq!(u.origin(), "https://example.com:8443");
        assert_eq!(u.join("d.png"), "https://example.com:8443/a/b/d.png");
        assert_eq!(u.join("../d.png"), "https://example.com:8443/a/d.png");
        assert_eq!(u.join("../../../../d.png"), "https://example.com:8443/d.png");
        assert_eq!(u.join("/root?q"), "https://example.com:8443/root?q");
        assert_eq!(u.join("//cdn.example.com/i.png"), "https://cdn.example.com/i.png");
        assert_eq!(u.join("?y=2"), "https://example.com:8443/a/b/c.html?y=2");
        assert_eq!(u.join("#g"), "https://example.com:8443/a/b/c.html?x=1#g");
        assert_eq!(u.join("mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(HttpUrl::parse("http://[::1]:8080").unwrap().host, "::1");
        assert_eq!(HttpUrl::parse("http://h").unwrap().rest, "/");
        for bad in
            ["file:///etc/passwd", "ftp://x", "http://user:pw@host/", "http:///x", "javascript:alert(1)", "http://h:99999/", "http://a b/"]
        {
            assert!(HttpUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn private_addresses_are_forbidden() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "::127.0.0.1",
        ] {
            assert!(is_forbidden(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_forbidden(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn titles_lose_the_site_name() {
        assert_eq!(clean_title("Ropes | Example Blog", Some("Example Blog"), "blog.example.com"), "Ropes");
        assert_eq!(clean_title("Ropes - example", None, "example.com"), "Ropes");
        assert_eq!(clean_title("A - B", None, "other.org"), "A - B");
    }

    #[test]
    fn charsets() {
        assert_eq!(decode(b"d\xe9j\xe0", "text/html; charset=ISO-8859-1"), "déjà");
        assert_eq!(decode(b"<meta charset=\"windows-1252\">d\xe9j\xe0", "text/html"), "<meta charset=\"windows-1252\">déjà");
        assert_eq!(decode("déjà".as_bytes(), "text/html"), "déjà");
        assert_eq!(decode(b"\xef\xbb\xbfhi", ""), "hi");
    }

    #[test]
    fn images_are_recognised_and_embedded() {
        assert_eq!(image_ext(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(image_ext(b"<?xml version='1.0'?><svg xmlns='x'/>"), Some("svg"));
        assert_eq!(image_ext(b"<html>"), None);
        assert_eq!(decode_data_uri("data:image/png;base64,iVBORw0KGgo=", 100).unwrap()[..4], *b"\x89PNG");
        assert_eq!(decode_data_uri("data:text/html;base64,PGI+", 100), None);
        assert_eq!(decode_data_uri("data:image/png;base64,iVBORw0KGgo=", 2), None);
        assert_eq!(image_name("https://x/a/My%20Pic.final.PNG?w=1", "png"), "My-20Pic-final");
        let names = std::collections::HashMap::from([("https://x/a.png".to_string(), "a.png".to_string())]);
        assert_eq!(embed_images("x ![A cat](https://x/a.png \"t\") y ![](https://x/a.png)", &names), "x ![[a.png|A cat]] y ![[a.png]]");
        assert_eq!(embed_images("[link](https://x/a.png)", &names), "[link](https://x/a.png)", "links stay links");
    }

    #[test]
    fn markdown_is_tidied_and_absolute() {
        let base = HttpUrl::parse("https://e.com/blog/post").unwrap();
        assert_eq!(
            absolutize("[a](../x \"t\") ![b](img.png) [c](#top) [d](https://o.org/)", &base),
            "[a](https://e.com/x \"t\") ![b](https://e.com/blog/img.png) [c](#top) [d](https://o.org/)"
        );
        assert_eq!(tidy("\n\na  \n\n\n\nb\n```\nx  \n\n\n```\n\n"), "a\n\nb\n```\nx  \n\n\n```\n");
    }
}
