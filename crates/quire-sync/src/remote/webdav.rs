// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WebDAV: Nextcloud, ownCloud, Fastmail Files, Synology, Apache
//! `mod_dav`, `rclone serve webdav` and the like.
//!
//! Plain HTTP methods only (`GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL`,
//! `PROPFIND`), with Basic authentication over HTTPS. No `LOCK`: each
//! device writes only its own files (see the module above), so there's
//! nothing to lock.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use zeroize::Zeroizing;

use super::net::{Http, Reply, check_scheme};
use super::sigv4::encode_path;
use super::{Entry, ObjectStore, Put, StoreError, xml};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebDavConfig {
    /// The folder Quire works in, e.g.
    /// `https://cloud.example.com/remote.php/dav/files/asha/Notes`.
    pub url: String,
    /// Allow `http://` to a host on the network (not just this computer).
    pub allow_http: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct WebDavCredentials {
    pub user: String,
    /// An app password, ideally.
    pub password: Zeroizing<String>,
}

impl std::fmt::Debug for WebDavCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavCredentials").field("user", &self.user).finish_non_exhaustive()
    }
}

const SMALL: u64 = 16 * 1024 * 1024;

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getetag/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

#[derive(Debug)]
pub struct WebDavStore {
    http: Http,
    /// `https://host:port`
    origin: String,
    host: String,
    /// The base folder's path, decoded, ending in `/`.
    base_path: String,
    auth: Zeroizing<String>,
    /// Folders known to exist (made or seen), so `MKCOL` is sent once.
    made: Mutex<HashSet<String>>,
}

impl WebDavStore {
    /// The HTTP client (to check how it's set up).
    pub const fn http(&self) -> &Http {
        &self.http
    }

    pub fn new(cfg: &WebDavConfig, creds: &WebDavCredentials) -> Result<Self, StoreError> {
        let url = cfg.url.trim();
        check_scheme(url, cfg.allow_http)?;
        let uri: ureq::http::Uri = url.parse().map_err(|_| StoreError::Config(format!("“{url}” isn't a web address")))?;
        if uri.query().is_some() {
            return Err(StoreError::Config("The WebDAV address can't have a `?` part".into()));
        }
        let base_path =
            xml::percent_decode(uri.path()).ok_or_else(|| StoreError::Config("The WebDAV address has a broken %-escape".into()))?;
        if base_path.split('/').any(|s| s == ".." || s == ".") {
            return Err(StoreError::Config("The WebDAV address can't contain `.` or `..` folders".into()));
        }
        let base_path = format!("{}/", base_path.trim_end_matches('/'));
        if creds.user.contains(':') {
            return Err(StoreError::Config("A WebDAV user name can't contain “:”".into()));
        }
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", creds.user, creds.password.as_str()));
        let scheme = uri.scheme_str().unwrap_or("https");
        let host = uri.host().unwrap_or_default().to_string();
        // `check_scheme` refused any `user@` part.
        let authority = uri.authority().map_or_else(|| host.clone(), |a| a.as_str().to_string());
        Ok(Self {
            http: Http::new(Duration::from_mins(2), scheme == "http"),
            origin: format!("{scheme}://{authority}"),
            host,
            base_path,
            auth: Zeroizing::new(format!("Basic {basic}")),
            made: Mutex::new(HashSet::new()),
        })
    }

    /// Faster retries (tests).
    #[must_use]
    pub const fn with_retry(mut self, retry: super::net::Retry) -> Self {
        self.http.retry = retry;
        self
    }

    fn url(&self, key: &str) -> String {
        format!("{}{}", self.origin, encode_path(&format!("{}{key}", self.base_path)))
    }

    fn request(&self, method: &str, key: &str, extra: &[(&str, &str)], body: &[u8], limit: u64) -> Result<Reply, StoreError> {
        let build = || {
            let mut h = vec![("authorization".to_string(), self.auth.to_string())];
            h.extend(extra.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())));
            h
        };
        self.http.send(method, &self.url(key), &build, body, limit)
    }

    fn error(reply: &Reply) -> StoreError {
        match reply.status {
            401 => StoreError::Auth(
                "The WebDAV server refused the user name or password (use an app password if you have two-step login).".into(),
            ),
            403 => StoreError::Auth("The WebDAV server doesn't allow this account to write there.".into()),
            404 | 409 => StoreError::Config("That folder doesn't exist on the WebDAV server.".into()),
            405 => StoreError::Config("That address isn't a WebDAV folder.".into()),
            507 => StoreError::Refused("The WebDAV server is out of space.".into()),
            s if (500..600).contains(&s) => StoreError::Offline(format!("the WebDAV server is having trouble (HTTP {s})")),
            s => StoreError::Refused(format!("the WebDAV server answered HTTP {s}")),
        }
    }

    /// Make the folders above `key`, top down.
    fn make_parents(&self, key: &str) -> Result<(), StoreError> {
        let parts: Vec<&str> = key.split('/').collect();
        for i in 1..parts.len() {
            let dir = format!("{}/", parts[..i].join("/"));
            if self.made.lock().unwrap_or_else(PoisonError::into_inner).contains(&dir) {
                continue;
            }
            let r = self.request("MKCOL", &dir, &[], &[], SMALL)?;
            match r.status {
                // Made, or already there.
                200 | 201 | 204 | 405 => {}
                _ => return Err(Self::error(&r)),
            }
            self.made.lock().unwrap_or_else(PoisonError::into_inner).insert(dir);
        }
        Ok(())
    }
}

impl ObjectStore for WebDavStore {
    fn get(&self, key: &str, limit: u64) -> Result<Option<Vec<u8>>, StoreError> {
        let r = self.request("GET", key, &[], &[], limit)?;
        match r.status {
            200 => Ok(Some(r.body)),
            404 | 410 => Ok(None),
            _ => Err(Self::error(&r)),
        }
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let r = self.request("HEAD", key, &[], &[], SMALL)?;
        match r.status {
            200 | 204 => Ok(true),
            404 | 410 => Ok(false),
            _ => Err(Self::error(&r)),
        }
    }

    fn put(&self, key: &str, body: &[u8], create_only: bool) -> Result<Put, StoreError> {
        let mut extra = vec![("content-type", "application/octet-stream")];
        if create_only {
            extra.push(("if-none-match", "*"));
        }
        let mut made = false;
        loop {
            let r = self.request("PUT", key, &extra, body, SMALL)?;
            match r.status {
                200 | 201 | 204 => return Ok(Put::Done),
                412 if create_only => return Ok(Put::Exists),
                // The folder isn't there yet.
                404 | 409 if !made => {
                    self.make_parents(key)?;
                    made = true;
                }
                // Locked by another client for a moment.
                423 => return Err(StoreError::Offline("the file is locked on the WebDAV server; trying again later".into())),
                _ => return Err(Self::error(&r)),
            }
        }
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
        let headers = [("depth", "1"), ("content-type", "application/xml; charset=utf-8")];
        let r = self.request("PROPFIND", dir, &headers, PROPFIND.as_bytes(), SMALL)?;
        match r.status {
            207 => {
                let list = xml::dav_list(&r.body, &self.host, &format!("{}{dir}", self.base_path))?;
                if list.len() > 100_000 {
                    return Err(StoreError::Refused("the WebDAV server listed over 100000 entries in one folder; refused".into()));
                }
                self.made.lock().unwrap_or_else(PoisonError::into_inner).insert(dir.to_string());
                Ok(list)
            }
            404 => Ok(Vec::new()),
            _ => Err(Self::error(&r)),
        }
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let r = self.request("DELETE", key, &[], &[], SMALL)?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(Self::error(&r)),
        }
    }

    fn describe(&self) -> String {
        format!("WebDAV folder {}{}", self.origin, self.base_path)
    }
}
