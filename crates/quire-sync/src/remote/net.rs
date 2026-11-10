// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The HTTP underneath the S3 and WebDAV stores: ureq with rustls and the
//! system's certificates, no redirects, bounded replies, and retries with
//! backoff for the failures worth retrying.

use std::time::Duration;

use super::StoreError;

/// A reply, read whole (bounded by the caller's limit).
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// How often and how patiently a request is retried.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    /// Tries in all (at least 1).
    pub attempts: u32,
    /// The first wait; it doubles each time, with jitter.
    pub base: Duration,
    /// The longest wait, also for a server's `Retry-After`.
    pub cap: Duration,
}

impl Retry {
    pub const DEFAULT: Self = Self { attempts: 4, base: Duration::from_millis(400), cap: Duration::from_secs(8) };
    /// For tests: fast.
    pub const QUICK: Self = Self { attempts: 3, base: Duration::from_millis(5), cap: Duration::from_millis(20) };

    fn wait(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self.base.saturating_mul(1 << attempt.min(10)).min(self.cap);
        let jitter = crate::crypto::random_bytes::<2>().map_or(0, |b| u64::from(u16::from_le_bytes(b)));
        let half = exp / 2;
        let wait = half + Duration::from_millis(jitter % (u64::try_from(half.as_millis()).unwrap_or(u64::MAX) + 1));
        retry_after.map_or(wait, |r| r.min(self.cap).max(wait))
    }
}

/// One HTTP connection pool for one store.
#[derive(Debug, Clone)]
pub struct Http {
    agent: ureq::Agent,
    pub retry: Retry,
}

/// `https://`, or `http://` to this computer only, unless the person asked
/// for plain HTTP on their network (a NAS, `rclone serve`).
///
/// An address with a user or password in it (`https://user:pass@host/`) is
/// refused, and never echoed: those go in their own fields, which are kept
/// in the keychain rather than in `config.json` or status lines.
pub fn check_scheme(url: &str, allow_http: bool) -> Result<(), StoreError> {
    if has_userinfo(url) {
        return Err(StoreError::Config(
            "Leave the user name and password out of the address: put them in their own fields (they're kept in the keychain).".into(),
        ));
    }
    let uri: ureq::http::Uri = url.parse().map_err(|_| StoreError::Config(format!("“{url}” isn't a web address")))?;
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") => {
            let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
            let local = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
            if local || allow_http {
                Ok(())
            } else {
                Err(StoreError::Config(
                    "Use an https:// address: over plain http:// anyone on the network could read the login. (Allow http:// only for a server on your own network.)"
                        .into(),
                ))
            }
        }
        _ => Err(StoreError::Config(format!("“{url}” isn't an http(s):// address"))),
    }
}

/// Whether the authority of `url` has a `user[:password]@` part.
pub fn has_userinfo(url: &str) -> bool {
    let rest = url.trim().split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().is_some_and(|authority| authority.contains('@'))
}

impl Http {
    /// `plain_http`: the store's address is `http://` (this computer, or a
    /// host on the person's network). Then no proxy from the environment
    /// (`HTTP_PROXY`, `ALL_PROXY`) is used: a proxy would carry the login
    /// and the requests in the clear past the one hop the person allowed.
    /// `https://` goes through such a proxy as usual (TLS end to end).
    pub fn new(timeout: Duration, plain_http: bool) -> Self {
        let mut config = ureq::Agent::config_builder();
        if plain_http {
            config = config.proxy(None);
        }
        let agent = config
            .timeout_global(Some(timeout))
            .timeout_connect(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            // Redirects aren't followed: a store that sends Quire elsewhere
            // could send signed requests or a password to another host.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .allow_non_standard_methods(true)
            .user_agent(format!("Quire/{}", env!("CARGO_PKG_VERSION")))
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
            .build()
            .into();
        Self { agent, retry: Retry::DEFAULT }
    }

    /// Whether requests go through a proxy.
    pub fn proxied(&self) -> bool {
        self.agent.config().proxy().is_some()
    }

    /// One request, without retries. `limit` bounds the reply's body.
    pub fn once(&self, method: &str, url: &str, headers: &[(String, String)], body: &[u8], limit: u64) -> Result<Reply, StoreError> {
        let mut b = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in headers {
            b = b.header(k.as_str(), v.as_str());
        }
        let req = b.body(body.to_vec()).map_err(|e| StoreError::Config(format!("bad request: {e}")))?;
        let mut res = self.agent.run(req).map_err(|e| net_error(&e))?;
        let status = res.status().as_u16();
        let headers =
            res.headers().iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))).collect();
        let body = if method == "HEAD" {
            Vec::new()
        } else {
            res.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => {
                    StoreError::Refused(format!("the storage sent a reply over {} MB; refused", limit / (1024 * 1024)))
                }
                e => net_error(&e),
            })?
        };
        let reply = Reply { status, headers, body };
        if (300..400).contains(&status) && status != 304 {
            let to = reply.header("location").unwrap_or("somewhere else").to_string();
            return Err(StoreError::Config(format!(
                "The storage redirected to {}; Quire doesn't follow redirects. Use that address instead.",
                host_of(&to).unwrap_or(to)
            )));
        }
        Ok(reply)
    }

    /// [`Self::once`], again after a pause when the network failed or the
    /// server was busy (5xx, 429). `build` makes each try's headers (a
    /// signature has the time in it).
    pub fn send(
        &self,
        method: &str,
        url: &str,
        build: &dyn Fn() -> Vec<(String, String)>,
        body: &[u8],
        limit: u64,
    ) -> Result<Reply, StoreError> {
        let mut attempt = 0;
        loop {
            let r = self.once(method, url, &build(), body, limit);
            attempt += 1;
            let retry_after = match &r {
                Ok(reply) if reply.status == 429 || (500..600).contains(&reply.status) && reply.status != 501 => {
                    Some(reply.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()).map(Duration::from_secs))
                }
                Err(StoreError::Offline(_)) => Some(None),
                _ => None,
            };
            match retry_after {
                Some(after) if attempt < self.retry.attempts.max(1) => std::thread::sleep(self.retry.wait(attempt - 1, after)),
                _ => return r,
            }
        }
    }
}

/// `example.com` from `https://example.com/x`.
pub fn host_of(url: &str) -> Option<String> {
    url.parse::<ureq::http::Uri>().ok().and_then(|u| u.host().map(str::to_string))
}

fn net_error(e: &ureq::Error) -> StoreError {
    match e {
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => {
            StoreError::Config(format!("The storage's certificate isn't trusted on this computer ({e})"))
        }
        ureq::Error::BadUri(_) => StoreError::Config(format!("not a usable address: {e}")),
        _ => StoreError::Offline(format!("can't reach the storage: {e}")),
    }
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` → seconds from now (positive: the
/// server is ahead). For the clock-skew check.
pub fn skew_secs(date: &str) -> Option<i64> {
    let then = chrono::DateTime::parse_from_rfc2822(date.trim()).ok()?;
    Some(then.timestamp() - chrono::Utc::now().timestamp())
}

/// What a clock that's this far off means for the person.
pub fn skew_message(secs: i64) -> String {
    let mins = (secs.abs() + 30) / 60;
    format!(
        "This computer's clock is {mins} minute(s) {} the storage's, so it refuses the signed requests. Set the clock to update automatically.",
        if secs > 0 { "behind" } else { "ahead of" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_for_this_computer_unless_allowed() {
        assert!(check_scheme("https://s3.example.com", false).is_ok());
        assert!(check_scheme("http://127.0.0.1:9000", false).is_ok());
        assert!(check_scheme("http://localhost:8080/dav", false).is_ok());
        assert!(check_scheme("http://[::1]:8080/", false).is_ok());
        assert!(matches!(check_scheme("http://nas.local/dav", false), Err(StoreError::Config(_))));
        assert!(check_scheme("http://nas.local/dav", true).is_ok());
        assert!(check_scheme("ftp://x", true).is_err());
        assert!(check_scheme("not a url", true).is_err());
    }

    #[test]
    fn logins_in_addresses_are_refused_and_never_echoed() {
        for url in ["https://asha:hunter2@cloud.example.com/dav", "https://asha@cloud.example.com", "http://u:hunter2@127.0.0.1:9000/"] {
            let e = check_scheme(url, true).unwrap_err().to_string();
            assert!(e.contains("own fields") && !e.contains("hunter2") && !e.contains("asha"), "{e}");
        }
        assert!(!has_userinfo("https://cloud.example.com/dav/a@b"), "an @ in the path is fine");
        assert!(check_scheme("https://cloud.example.com/dav/a@b", false).is_ok());
    }

    #[test]
    fn plain_http_never_goes_through_an_environment_proxy() {
        let plain = Http::new(Duration::from_secs(5), true);
        assert!(plain.agent.config().proxy().is_none());
        // Through the stores: an http:// address gets such an agent.
        let creds =
            super::super::S3Credentials { access_key: "a".into(), secret_key: zeroize::Zeroizing::new("b".into()), session_token: None };
        let cfg = |endpoint: &str| super::super::S3Config {
            endpoint: endpoint.into(),
            bucket: "notes".into(),
            region: "us-east-1".into(),
            ..super::super::S3Config::default()
        };
        assert!(!super::super::S3Store::new(cfg("http://127.0.0.1:9000"), creds).unwrap().http().proxied());
        let dav = super::super::WebDavCredentials { user: "u".into(), password: zeroize::Zeroizing::new("p".into()) };
        let local = super::super::WebDavConfig { url: "http://localhost:8080/dav".into(), allow_http: false };
        assert!(!super::super::WebDavStore::new(&local, &dav).unwrap().http().proxied());
    }

    #[test]
    fn backoff_grows_and_stays_capped() {
        let r = Retry { attempts: 5, base: Duration::from_millis(100), cap: Duration::from_secs(1) };
        for a in 0..8 {
            let w = r.wait(a, None);
            assert!(w <= Duration::from_secs(1), "{a}: {w:?}");
            assert!(w >= (Duration::from_millis(100) * (1 << a)).min(Duration::from_secs(1)) / 2);
        }
        assert_eq!(r.wait(0, Some(Duration::from_mins(1))), Duration::from_secs(1), "Retry-After is capped too");
    }

    #[test]
    fn skew_reads_http_dates() {
        let now = chrono::Utc::now();
        let ahead = (now + chrono::Duration::minutes(20)).to_rfc2822();
        let s = skew_secs(&ahead).unwrap();
        assert!((1190..=1210).contains(&s), "{s}");
        assert!(skew_message(s).contains("20 minute(s) behind"));
        assert!(skew_secs("yesterday").is_none());
    }
}
