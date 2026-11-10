// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The XML that S3 and WebDAV servers send: listings and errors.
//!
//! Parsed with `roxmltree`, which rejects any DTD (so no entity expansion
//! or external entities) and is given a node limit here. Every name read
//! from a listing is checked again by the caller before it's used.

use super::{Entry, StoreError};

/// Nodes in one reply, at most (a page of 1000 S3 keys is about 10 000).
const NODES_LIMIT: u32 = 400_000;

fn parse(body: &[u8]) -> Result<roxmltree::Document<'_>, StoreError> {
    let text = std::str::from_utf8(body).map_err(|_| StoreError::Refused("the storage sent a listing that isn't UTF-8".into()))?;
    let opt = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: NODES_LIMIT, entity_resolver: None };
    roxmltree::Document::parse_with_options(text, opt)
        .map_err(|e| StoreError::Refused(format!("the storage sent XML Quire can't read ({e})")))
}

fn child_text<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name).and_then(|c| c.text())
}

/// `<Error><Code>…</Code><Message>…</Message></Error>`.
pub fn s3_error(body: &[u8]) -> Option<(String, String)> {
    let doc = parse(body).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "Error" {
        return None;
    }
    Some((child_text(root, "Code").unwrap_or_default().to_string(), child_text(root, "Message").unwrap_or_default().to_string()))
}

#[derive(Debug, Default)]
pub struct S3Page {
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub next: Option<String>,
}

/// One page of `ListObjectsV2` with `delimiter=/`, for keys under `prefix`.
/// Names are what follows `prefix`; anything not under it is dropped.
pub fn s3_list(body: &[u8], prefix: &str) -> Result<S3Page, StoreError> {
    let doc = parse(body)?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(StoreError::Refused("the storage sent something other than a listing".into()));
    }
    let mut page = S3Page {
        truncated: child_text(root, "IsTruncated").is_some_and(|t| t.trim() == "true"),
        next: child_text(root, "NextContinuationToken").map(str::to_string),
        ..S3Page::default()
    };
    for n in root.children().filter(roxmltree::Node::is_element) {
        match n.tag_name().name() {
            "Contents" => {
                let Some(name) = child_text(n, "Key").and_then(|k| k.strip_prefix(prefix)) else { continue };
                if name.is_empty() || name.contains('/') {
                    continue;
                }
                let etag = child_text(n, "ETag").unwrap_or_default().to_string();
                let stamp = format!("{}|{}", child_text(n, "Size").unwrap_or_default(), child_text(n, "LastModified").unwrap_or_default());
                page.entries.push(Entry { name: name.to_string(), dir: false, etag, stamp });
            }
            "CommonPrefixes" => {
                let Some(name) = child_text(n, "Prefix").and_then(|k| k.strip_prefix(prefix)).and_then(|k| k.strip_suffix('/')) else {
                    continue;
                };
                if name.is_empty() || name.contains('/') {
                    continue;
                }
                page.entries.push(Entry { name: name.to_string(), dir: true, etag: String::new(), stamp: String::new() });
            }
            _ => {}
        }
    }
    Ok(page)
}

/// Decode `%xx` escapes; `None` for broken ones or bytes that aren't UTF-8.
pub fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

const DAV: &str = "DAV:";

/// The entries of a WebDAV folder listing.
///
/// `body` is a `207 Multi-Status` reply to `PROPFIND` with `Depth: 1` on
/// the collection at `dir_path` (its URL path, decoded, ending in `/`) of
/// `host`. Entries directly inside it are returned; the collection itself,
/// other hosts and anything deeper are dropped.
pub fn dav_list(body: &[u8], host: &str, dir_path: &str) -> Result<Vec<Entry>, StoreError> {
    let doc = parse(body)?;
    let root = doc.root_element();
    if !(root.tag_name().name() == "multistatus" && root.tag_name().namespace() == Some(DAV)) {
        return Err(StoreError::Refused("the WebDAV server sent something other than a folder listing".into()));
    }
    let is =
        |n: &roxmltree::Node<'_, '_>, name: &str| n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(DAV);
    let mut out = Vec::new();
    for resp in root.children().filter(|n| is(n, "response")) {
        let Some(href) = resp.children().find(|n| is(n, "href")).and_then(|n| n.text()) else { continue };
        let href = href.trim();
        // An absolute URL must be this server; a path is taken as is.
        let path = if href.starts_with("http://") || href.starts_with("https://") {
            let Ok(u) = href.parse::<ureq::http::Uri>() else { continue };
            if !u.host().is_some_and(|h| h.eq_ignore_ascii_case(host)) {
                continue;
            }
            u.path().to_string()
        } else {
            href.to_string()
        };
        let Some(path) = percent_decode(&path) else { continue };
        let Some(rest) = path.strip_prefix(dir_path).or_else(|| path.strip_prefix(dir_path.trim_end_matches('/'))) else { continue };
        let name = rest.trim_matches('/');
        if name.is_empty() || name.contains('/') {
            continue;
        }
        // Properties from the `200 OK` propstat only.
        let mut dir = rest.ends_with('/');
        let (mut etag, mut stamp) = (String::new(), String::new());
        for ps in resp.children().filter(|n| is(n, "propstat")) {
            let ok = ps.children().find(|n| is(n, "status")).and_then(|n| n.text()).is_none_or(|s| s.contains(" 200"));
            let Some(prop) = ps.children().find(|n| is(n, "prop")).filter(|_| ok) else { continue };
            for p in prop.children().filter(roxmltree::Node::is_element) {
                match p.tag_name().name() {
                    "resourcetype" => dir |= p.children().any(|c| is(&c, "collection")),
                    "getetag" => etag = p.text().unwrap_or_default().trim().to_string(),
                    "getcontentlength" | "getlastmodified" => {
                        stamp.push_str(p.text().unwrap_or_default().trim());
                        stamp.push('|');
                    }
                    _ => {}
                }
            }
        }
        out.push(Entry { name: name.to_string(), dir, etag, stamp });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_listing_and_errors() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name><Prefix>q/s/</Prefix><IsTruncated>true</IsTruncated>
  <NextContinuationToken>tok==</NextContinuationToken>
  <Contents><Key>q/s/space.json</Key><ETag>"abc"</ETag><Size>12</Size></Contents>
  <Contents><Key>q/s/a/b</Key><ETag>"x"</ETag><Size>1</Size></Contents>
  <Contents><Key>elsewhere/x</Key></Contents>
  <CommonPrefixes><Prefix>q/s/devices/</Prefix></CommonPrefixes>
  <CommonPrefixes><Prefix>q/s/../</Prefix></CommonPrefixes>
</ListBucketResult>"#;
        let page = s3_list(body, "q/s/").unwrap();
        assert!(page.truncated);
        assert_eq!(page.next.as_deref(), Some("tok=="));
        let names: Vec<(&str, bool)> = page.entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        // `..` is returned as a name; the transport only ever accepts ids.
        assert_eq!(names, [("space.json", false), ("devices", true), ("..", true)]);
        assert_eq!((page.entries[0].etag.as_str(), page.entries[0].stamp.as_str()), ("\"abc\"", "12|"));
        let err = br"<Error><Code>SignatureDoesNotMatch</Code><Message>nope</Message></Error>";
        assert_eq!(s3_error(err), Some(("SignatureDoesNotMatch".into(), "nope".into())));
        assert_eq!(s3_error(b"<html>busy</html>"), None);
        assert_eq!(s3_error(b"\xff\xfe"), None);
    }

    #[test]
    fn dtds_and_entity_bombs_are_refused() {
        let bomb = br#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">]>
<ListBucketResult><Contents><Key>&lol2;</Key></Contents></ListBucketResult>"#;
        assert!(matches!(s3_list(bomb, ""), Err(StoreError::Refused(_))));
        let ext =
            br#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY e SYSTEM "file:///etc/passwd">]><multistatus xmlns="DAV:">&e;</multistatus>"#;
        assert!(dav_list(ext, "h", "/").is_err());
        assert!(s3_list(b"<ListBucketResult><Contents>", "").is_err(), "truncated XML");
        assert!(s3_list(b"<Other/>", "").is_err());
    }

    #[test]
    fn dav_listing_keeps_only_direct_children_of_this_server() {
        let body = br#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:">
 <d:response><d:href>/dav/Quire%20Sync/s/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/dav/Quire%20Sync/s/tips/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>https://cloud.example/dav/Quire%20Sync/s/space.json</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getetag>"e1"</d:getetag><d:getcontentlength>10</d:getcontentlength></d:prop>
  <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getlastmodified/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>
 <d:response><d:href>https://evil.example/dav/Quire%20Sync/s/x</d:href></d:response>
 <d:response><d:href>/dav/Quire%20Sync/s/tips/deep</d:href></d:response>
 <d:response><d:href>/dav/Quire%20Sync/s/%2e%2e</d:href></d:response>
 <d:response><d:href>/dav/Quire%20Sync/s/bad%zz</d:href></d:response>
 <d:response><d:href>/elsewhere/y</d:href></d:response>
</d:multistatus>"#;
        let list = dav_list(body, "cloud.example", "/dav/Quire Sync/s/").unwrap();
        let names: Vec<(&str, bool)> = list.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("tips", true), ("space.json", false), ("..", false)]);
        assert_eq!((list[1].etag.as_str(), list[1].stamp.as_str()), ("\"e1\"", "10|"));
        assert_eq!(percent_decode("a%20b%C3%BC"), Some("a bü".into()));
        assert_eq!(percent_decode("%ff"), None);
        assert_eq!(percent_decode("%2"), None);
    }
}
