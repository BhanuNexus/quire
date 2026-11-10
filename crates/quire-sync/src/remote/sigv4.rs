// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! AWS Signature Version 4 for S3 requests.
//!
//! About 100 lines over `hmac` and `sha2` (already used for sync's own
//! crypto), instead of the AWS SDK's signer and its dependency tree. Tested
//! against AWS's published examples and against `botocore`'s signer.
//!
//! S3 flavour: the path is encoded once and never normalised (a key may
//! contain `//` or `.`), and the payload's SHA-256 goes in
//! `x-amz-content-sha256`, which is signed.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::crypto::hex;

/// The algorithm name in the `Authorization` header.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// What a request is signed with.
#[derive(Clone)]
pub struct Signer<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    /// `us-east-1`; `auto` for Cloudflare R2.
    pub region: &'a str,
    /// `s3`.
    pub service: &'a str,
}

/// One request, as it will be sent.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub method: &'a str,
    /// The path as sent: already encoded (see [`encode_path`]).
    pub path: &'a str,
    /// Query parameters, not encoded.
    pub query: &'a [(String, String)],
    /// Headers to sign (`host` included). Names in any case.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the body, or `UNSIGNED-PAYLOAD`.
    pub payload_hash: &'a str,
    /// `20150830T123600Z`.
    pub amz_date: &'a str,
}

/// Percent-encode as SigV4 wants: everything but `A-Z a-z 0-9 - _ . ~`,
/// and `/` too unless `keep_slash`.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b'/' if keep_slash => out.push('/'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// An object key as a URL path (`/bucket/a b` → `/bucket/a%20b`).
pub fn encode_path(path: &str) -> String {
    uri_encode(path, true)
}

/// The query string, sorted and encoded, as both the URL and the signature use it.
pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Headers as signed: lower-case names, values with outer spaces trimmed
/// and inner runs of spaces collapsed, sorted by name. Returns the
/// canonical block and the `SignedHeaders` list.
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut hs: Vec<(String, String)> =
        headers.iter().map(|(k, v)| (k.trim().to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    hs.sort();
    // Repeated names are joined with commas, in the order given.
    let mut merged: Vec<(String, String)> = Vec::new();
    for (k, v) in hs {
        match merged.last_mut() {
            Some((lk, lv)) if *lk == k => {
                lv.push(',');
                lv.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let block = merged.iter().fold(String::new(), |mut s, (k, v)| {
        use std::fmt::Write as _;
        let _ = writeln!(s, "{k}:{v}");
        s
    });
    let names = merged.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    (block, names)
}

impl Signer<'_> {
    /// The canonical request (step 1), for tests and debugging.
    pub fn canonical_request(req: &Request<'_>) -> String {
        let (headers, signed) = canonical_headers(req.headers);
        format!("{}\n{}\n{}\n{headers}\n{signed}\n{}", req.method, req.path, canonical_query(req.query), req.payload_hash)
    }

    fn scope(&self, date: &str) -> String {
        format!("{date}/{}/{}/aws4_request", self.region, self.service)
    }

    /// The hex signature of `req`.
    pub fn signature(&self, req: &Request<'_>) -> String {
        let date = req.amz_date.get(..8).unwrap_or_default();
        let to_sign =
            format!("{ALGORITHM}\n{}\n{}\n{}", req.amz_date, self.scope(date), sha256_hex(Self::canonical_request(req).as_bytes()));
        let k_date = hmac(format!("AWS4{}", self.secret_key).as_bytes(), date.as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, self.service.as_bytes());
        let k_signing = hmac(&k_service, b"aws4_request");
        hex(&hmac(&k_signing, to_sign.as_bytes()))
    }

    /// The `Authorization` header's value.
    pub fn authorization(&self, req: &Request<'_>) -> String {
        let (_, signed) = canonical_headers(req.headers);
        let date = req.amz_date.get(..8).unwrap_or_default();
        format!(
            "{ALGORITHM} Credential={}/{}, SignedHeaders={signed}, Signature={}",
            self.access_key,
            self.scope(date),
            self.signature(req)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// The `aws-sig-v4-test-suite` credentials (`get-vanilla` and friends).
    fn suite() -> Signer<'static> {
        Signer {
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
        }
    }

    #[test]
    fn aws_test_suite_get_vanilla() {
        let headers = h(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")]);
        let req = Request { method: "GET", path: "/", query: &[], headers: &headers, payload_hash: EMPTY, amz_date: "20150830T123600Z" };
        assert_eq!(
            Signer::canonical_request(&req),
            format!("GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{EMPTY}")
        );
        assert_eq!(suite().signature(&req), "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        assert_eq!(
            suite().authorization(&req),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn aws_test_suite_post_vanilla_and_query_order() {
        let headers = h(&[("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")]);
        let post = Request { method: "POST", path: "/", query: &[], headers: &headers, payload_hash: EMPTY, amz_date: "20150830T123600Z" };
        assert_eq!(suite().signature(&post), "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b");
        // get-vanilla-query-order-key-case: `?Param2=value2&Param1=value1`.
        let q = vec![("Param2".to_string(), "value2".to_string()), ("Param1".to_string(), "value1".to_string())];
        let get = Request { method: "GET", path: "/", query: &q, headers: &headers, payload_hash: EMPTY, amz_date: "20150830T123600Z" };
        assert_eq!(suite().signature(&get), "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
    }

    /// The worked examples in the S3 API reference ("Signature Calculations
    /// for the Authorization Header: Transferring Payload in a Single Chunk").
    fn s3_example() -> Signer<'static> {
        Signer {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "s3",
        }
    }

    #[test]
    fn s3_reference_get_object() {
        let headers = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY),
            ("x-amz-date", "20130524T000000Z"),
        ]);
        let req =
            Request { method: "GET", path: "/test.txt", query: &[], headers: &headers, payload_hash: EMPTY, amz_date: "20130524T000000Z" };
        assert_eq!(s3_example().signature(&req), "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }

    #[test]
    fn s3_reference_put_object() {
        let body_hash = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(body_hash, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let headers = h(&[
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("x-amz-content-sha256", &body_hash),
        ]);
        let req = Request {
            method: "PUT",
            path: &encode_path("/test$file.text"),
            query: &[],
            headers: &headers,
            payload_hash: &body_hash,
            amz_date: "20130524T000000Z",
        };
        assert_eq!(req.path, "/test%24file.text");
        assert_eq!(s3_example().signature(&req), "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
    }

    #[test]
    fn s3_reference_get_lifecycle_and_list() {
        let headers = h(&[("Host", "examplebucket.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", EMPTY)]);
        let q = vec![("lifecycle".to_string(), String::new())];
        let req = Request { method: "GET", path: "/", query: &q, headers: &headers, payload_hash: EMPTY, amz_date: "20130524T000000Z" };
        assert_eq!(s3_example().signature(&req), "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");
        let q = vec![("max-keys".to_string(), "2".to_string()), ("prefix".to_string(), "J".to_string())];
        let req = Request { method: "GET", path: "/", query: &q, headers: &headers, payload_hash: EMPTY, amz_date: "20130524T000000Z" };
        assert_eq!(s3_example().signature(&req), "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    /// What Quire sends to MinIO-style storage: a path-style URL with a
    /// port, a key with a space, `ü` and `+`, a conditional create and a
    /// session token. The expected signature is botocore's (1.43,
    /// `S3SigV4Auth`) for the same request.
    #[test]
    fn botocore_agrees_on_a_conditional_put_with_a_session_token() {
        let body_hash = sha256_hex(b"hello");
        let headers = h(&[
            ("host", "127.0.0.1:9000"),
            ("if-none-match", "*"),
            ("x-amz-content-sha256", &body_hash),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-security-token", "SESSIONTOKEN=="),
        ]);
        let path = encode_path("/my-bucket/notes/quire-sync/a b/ü+x.rev");
        assert_eq!(path, "/my-bucket/notes/quire-sync/a%20b/%C3%BC%2Bx.rev");
        let req =
            Request { method: "PUT", path: &path, query: &[], headers: &headers, payload_hash: &body_hash, amz_date: "20130524T000000Z" };
        let signer = Signer { access_key: "minioadmin", secret_key: "minio/secret+key", region: "eu-central-1", service: "s3" };
        assert_eq!(
            signer.authorization(&req),
            "AWS4-HMAC-SHA256 Credential=minioadmin/20130524/eu-central-1/s3/aws4_request, \
             SignedHeaders=host;if-none-match;x-amz-content-sha256;x-amz-date;x-amz-security-token, \
             Signature=01e98892ffb060a4231fed198db52363f84fa4528d721123e08d2da9767ef760"
        );
    }

    #[test]
    fn encoding_follows_the_s3_rules() {
        assert_eq!(uri_encode("a b+c/d~e_f.g-h", false), "a%20b%2Bc%2Fd~e_f.g-h");
        assert_eq!(encode_path("/bucket/quire-sync/ü"), "/bucket/quire-sync/%C3%BC");
        let q = vec![("prefix".to_string(), "a/b".to_string()), ("delimiter".to_string(), "/".to_string())];
        assert_eq!(canonical_query(&q), "delimiter=%2F&prefix=a%2Fb");
        // Header values are trimmed and inner spaces collapsed.
        let (block, names) = canonical_headers(&h(&[("My-Header1", "  a   b   c "), ("Host", "x")]));
        assert_eq!((block.as_str(), names.as_str()), ("host:x\nmy-header1:a b c\n", "host;my-header1"));
    }
}
