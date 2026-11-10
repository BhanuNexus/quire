// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! S3-compatible object storage: AWS S3, Cloudflare R2, Backblaze B2,
//! MinIO, Wasabi and others that speak the S3 API with SigV4.

use std::time::Duration;

use zeroize::Zeroizing;

use super::net::{Http, Reply, check_scheme, skew_message, skew_secs};
use super::sigv4::{Signer, canonical_query, encode_path, sha256_hex};
use super::{Entry, ObjectStore, Put, StoreError, xml};

/// Where the bucket is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct S3Config {
    /// `https://s3.eu-central-1.amazonaws.com`, `https://<account>.r2.cloudflarestorage.com`,
    /// `https://s3.us-west-004.backblazeb2.com`, `http://127.0.0.1:9000`…
    pub endpoint: String,
    /// `us-east-1`; R2 takes `auto`.
    pub region: String,
    pub bucket: String,
    /// A folder in the bucket for Quire (`notes`), or empty.
    pub prefix: String,
    /// `bucket.host/key` (virtual-hosted) instead of `host/bucket/key`.
    /// AWS prefers it; most others take either.
    pub virtual_hosted: bool,
    /// Allow `http://` to a host on the network (not just this computer).
    pub allow_http: bool,
}

impl S3Config {
    /// AWS endpoints use virtual-hosted URLs (except for buckets with dots,
    /// which don't match the certificate); others use paths.
    pub fn default_virtual_hosted(endpoint: &str, bucket: &str) -> bool {
        super::net::host_of(endpoint).is_some_and(|h| h.ends_with(".amazonaws.com")) && !bucket.contains('.')
    }
}

/// An access key. The secret is wiped from memory when dropped and never
/// printed.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Credentials {
    pub access_key: String,
    pub secret_key: Zeroizing<String>,
    /// For temporary credentials (STS).
    pub session_token: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials").field("access_key", &self.access_key).finish_non_exhaustive()
    }
}

/// Replies other than object bodies (errors, listings) are small.
const SMALL: u64 = 8 * 1024 * 1024;
/// Entries in one folder listing, at most.
const MAX_ENTRIES: usize = 100_000;
/// Pages of one listing, at most (a server that never says it's done).
const MAX_PAGES: usize = 150;

#[derive(Debug, Clone)]
pub struct S3Store {
    cfg: S3Config,
    creds: S3Credentials,
    http: Http,
    /// `https://host:port`
    origin: String,
    /// `host` or `host:port`, as signed.
    host: String,
    /// `/bucket` for path-style URLs, else empty.
    bucket_path: String,
}

impl S3Store {
    /// The HTTP client (to check how it's set up).
    pub const fn http(&self) -> &Http {
        &self.http
    }

    pub fn new(cfg: S3Config, creds: S3Credentials) -> Result<Self, StoreError> {
        let endpoint = cfg.endpoint.trim().trim_end_matches('/').to_string();
        check_scheme(&endpoint, cfg.allow_http)?;
        let uri: ureq::http::Uri = endpoint.parse().map_err(|_| StoreError::Config(format!("“{endpoint}” isn't a web address")))?;
        if !uri.path().trim_matches('/').is_empty() {
            return Err(StoreError::Config("The endpoint is the storage's address only, without a bucket or folder after it".into()));
        }
        let bucket = cfg.bucket.trim();
        let valid_bucket = (3..=63).contains(&bucket.len())
            && bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
            && !bucket.starts_with(['-', '.'])
            && !bucket.ends_with(['-', '.']);
        if !valid_bucket {
            return Err(StoreError::Config(format!("“{bucket}” isn't a bucket name (3–63 lower-case letters, digits, dots, dashes)")));
        }
        if cfg.region.trim().is_empty() || cfg.region.contains(['/', ' ']) {
            return Err(StoreError::Config("Give the bucket's region (`us-east-1`; R2: `auto`)".into()));
        }
        if creds.access_key.trim().is_empty() || creds.secret_key.is_empty() {
            return Err(StoreError::Auth("Give the access key ID and secret access key".into()));
        }
        let scheme = uri.scheme_str().unwrap_or("https");
        let host = uri.host().unwrap_or_default();
        let port = uri.port_u16().filter(|p| !matches!((scheme, p), ("https", 443) | ("http", 80)));
        let host = if cfg.virtual_hosted { format!("{bucket}.{host}") } else { host.to_string() };
        let host = port.map_or_else(|| host.clone(), |p| format!("{host}:{p}"));
        let bucket_path = if cfg.virtual_hosted { String::new() } else { format!("/{bucket}") };
        let cfg = S3Config { prefix: super::clean_prefix(&cfg.prefix)?, endpoint, ..cfg };
        Ok(Self {
            origin: format!("{scheme}://{host}"),
            host,
            bucket_path,
            http: Http::new(Duration::from_mins(2), scheme == "http"),
            creds,
            cfg,
        })
    }

    /// Faster retries (tests).
    #[must_use]
    pub const fn with_retry(mut self, retry: super::net::Retry) -> Self {
        self.http.retry = retry;
        self
    }

    fn object_key(&self, key: &str) -> String {
        if self.cfg.prefix.is_empty() { key.to_string() } else { format!("{}/{key}", self.cfg.prefix) }
    }

    fn path(&self, object_key: &str) -> String {
        encode_path(&format!("{}/{object_key}", self.bucket_path))
    }

    /// Sign and send, retrying when worth it.
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        extra: &[(String, String)],
        body: &[u8],
        limit: u64,
    ) -> Result<Reply, StoreError> {
        let payload_hash = sha256_hex(body);
        let qs = canonical_query(query);
        let url = if qs.is_empty() { format!("{}{path}", self.origin) } else { format!("{}{path}?{qs}", self.origin) };
        let build = || {
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let mut headers = vec![
                ("host".to_string(), self.host.clone()),
                ("x-amz-content-sha256".to_string(), payload_hash.clone()),
                ("x-amz-date".to_string(), amz_date.clone()),
            ];
            if let Some(t) = &self.creds.session_token {
                headers.push(("x-amz-security-token".to_string(), t.to_string()));
            }
            headers.extend(extra.iter().cloned());
            let signer =
                Signer { access_key: &self.creds.access_key, secret_key: &self.creds.secret_key, region: &self.cfg.region, service: "s3" };
            let req = super::sigv4::Request { method, path, query, headers: &headers, payload_hash: &payload_hash, amz_date: &amz_date };
            let auth = signer.authorization(&req);
            headers.push(("authorization".to_string(), auth));
            headers
        };
        self.http.send(method, &url, &build, body, limit)
    }

    /// An error reply in words.
    fn error(&self, reply: &Reply) -> StoreError {
        let (code, message) = xml::s3_error(&reply.body).unwrap_or_default();
        let skew = reply.header("date").and_then(skew_secs).filter(|s| s.abs() > 5 * 60);
        match (reply.status, code.as_str()) {
            (_, "RequestTimeTooSkewed") => StoreError::ClockSkew(skew_message(skew.unwrap_or(16 * 60))),
            (403, _) if skew.is_some() => StoreError::ClockSkew(skew_message(skew.unwrap_or_default())),
            (_, "ExpiredToken" | "TokenRefreshRequired") => {
                StoreError::Auth("The storage credentials have expired. Enter new ones in Settings → Sync.".into())
            }
            (_, "InvalidAccessKeyId") => StoreError::Auth("The storage doesn't know this access key ID.".into()),
            (_, "SignatureDoesNotMatch") => StoreError::Auth("The secret access key is wrong.".into()),
            (_, "InvalidToken") => StoreError::Auth("The session token is wrong.".into()),
            (_, "NoSuchBucket") => StoreError::Config(format!("There's no bucket “{}” at this address.", self.cfg.bucket)),
            (_, "AuthorizationHeaderMalformed") if message.contains("region") => {
                StoreError::Config(format!("The region is wrong for this bucket: {message}"))
            }
            (401 | 403, _) => StoreError::Auth(if code.is_empty() {
                format!("The storage refused the access key (HTTP {}).", reply.status)
            } else {
                format!(
                    "The storage refused the access key: {code}{}",
                    if message.is_empty() { String::new() } else { format!(" ({message})") }
                )
            }),
            (s, _) if (500..600).contains(&s) => StoreError::Offline(format!("the storage is having trouble (HTTP {s})")),
            (s, "") => StoreError::Refused(format!("the storage answered HTTP {s}")),
            (s, c) => StoreError::Refused(format!("the storage answered HTTP {s}: {c} {message}").trim_end().to_string()),
        }
    }
}

impl ObjectStore for S3Store {
    fn get(&self, key: &str, limit: u64) -> Result<Option<Vec<u8>>, StoreError> {
        let r = self.request("GET", &self.path(&self.object_key(key)), &[], &[], &[], limit)?;
        match r.status {
            200 => Ok(Some(r.body)),
            404 if xml::s3_error(&r.body).is_none_or(|(c, _)| c != "NoSuchBucket") => Ok(None),
            _ => Err(self.error(&r)),
        }
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let r = self.request("HEAD", &self.path(&self.object_key(key)), &[], &[], &[], SMALL)?;
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(self.error(&r)),
        }
    }

    fn put(&self, key: &str, body: &[u8], create_only: bool) -> Result<Put, StoreError> {
        let mut extra = vec![("content-type".to_string(), "application/octet-stream".to_string())];
        if create_only {
            extra.push(("if-none-match".to_string(), "*".to_string()));
        }
        let path = self.path(&self.object_key(key));
        // AWS answers 409 when two conditional writes race; the next try gets 412.
        for _ in 0..3 {
            let r = self.request("PUT", &path, &[], &extra, body, SMALL)?;
            return match r.status {
                200 | 201 | 204 => Ok(Put::Done),
                412 if create_only => Ok(Put::Exists),
                409 if create_only => continue,
                400 | 501
                    if create_only && xml::s3_error(&r.body).is_some_and(|(c, m)| c == "NotImplemented" || m.contains("If-None-Match")) =>
                {
                    Err(StoreError::NoConditional)
                }
                _ => Err(self.error(&r)),
            };
        }
        Err(StoreError::Offline("the storage kept reporting a conflicting write".into()))
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        let prefix = self.object_key(dir);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut q = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), prefix.clone()),
                ("delimiter".to_string(), "/".to_string()),
                ("max-keys".to_string(), "1000".to_string()),
            ];
            if let Some(t) = &token {
                q.push(("continuation-token".to_string(), t.clone()));
            }
            let r = self.request("GET", &format!("{}/", self.bucket_path), &q, &[], &[], SMALL)?;
            if r.status != 200 {
                return Err(self.error(&r));
            }
            let page = xml::s3_list(&r.body, &prefix)?;
            out.extend(page.entries);
            if out.len() > MAX_ENTRIES {
                return Err(StoreError::Refused(format!("the storage listed over {MAX_ENTRIES} entries in one folder; refused")));
            }
            match page.next {
                Some(t) if page.truncated && token.as_ref() != Some(&t) => token = Some(t),
                _ if page.truncated => return Err(StoreError::Refused("the storage's listing didn't move on to its next page".into())),
                _ => return Ok(out),
            }
        }
        Err(StoreError::Refused(format!("the storage's listing went on for over {MAX_PAGES} pages; refused")))
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let r = self.request("DELETE", &self.path(&self.object_key(key)), &[], &[], &[], SMALL)?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(self.error(&r)),
        }
    }

    /// An S3 `ETag` is the MD5 of the object (Quire never uploads in parts).
    fn etag_tracks_content(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        let p = if self.cfg.prefix.is_empty() { String::new() } else { format!("/{}", self.cfg.prefix) };
        format!("S3 bucket {}{p} at {}", self.cfg.bucket, self.cfg.endpoint)
    }
}
