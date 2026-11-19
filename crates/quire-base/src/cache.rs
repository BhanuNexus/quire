// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The one container every cache file under `<vault>/.quire/cache/` uses
//! (`docs/design/0.14-architecture.md` §3; format: `docs/formats/cache-container.md`).
//!
//! A fixed header names which cache the file is ([`Kind`]), the layout of
//! its body (`format`) and the version of the code that derived it
//! (`builder`), and carries the body's length and SHA-256. The body is
//! private to the cache.
//!
//! - [`read()`] gives the body only when every field matches and the hash is
//!   right. **Any doubt is "no cache"**: a cache is never migrated, a
//!   version bump discards it, and the caller builds it again.
//! - [`write()`] replaces the whole file through [`crate::fs::write_atomic_parts`]
//!   with `Options::STATE.lazy()`: atomic, not flushed. A power cut can
//!   leave an old or a missing cache, never a torn one, and the hash catches
//!   a torn body where the filesystem reorders writes. A valid file of a
//!   newer `format` than the caller's is left alone (P3): a newer build may
//!   still be using it. Valid means its length and body hash check out, so
//!   a damaged file is replaced even when its header looks newer (a flip in
//!   `format` alone, with the body intact, can't be told from a real newer
//!   file: the hash covers the body only).
//!
//! The bytes do not depend on the edition (P5).

use std::io::Read as _;
use std::path::Path;

use sha2::{Digest as _, Sha256};

use crate::fs::Options;

/// The first eight bytes of every cache file.
pub const MAGIC: [u8; 8] = *b"QUIRECA1";

/// Bytes before the body: magic 8, kind 2, format 4, builder 4, flags 4,
/// body length 8, body hash 32.
pub const HEADER_LEN: usize = 8 + 2 + 4 + 4 + 4 + 8 + 32;

/// Which cache a file holds. The numbers are a registry, kept here so two
/// caches never share one, and listed in `docs/formats/cache-container.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Kind(pub u16);

impl Kind {
    /// The vault manifest, `.quire/cache/manifest` (`quire_vault::changes`).
    pub const MANIFEST: Self = Self(1);
    /// The index's startup cache: one row per note (`docs/design/0.14-index-startup.md` §3.6).
    pub const RECORDS_INDEX: Self = Self(2);
    /// The index's startup cache: a segment of records and sources.
    pub const RECORDS_SEGMENT: Self = Self(3);
    /// The passage cache: one row per note.
    pub const PASSAGES_INDEX: Self = Self(4);
    /// The passage cache: a segment of passages and vectors.
    pub const PASSAGES_SEGMENT: Self = Self(5);
    /// A fitted LSA model.
    pub const LSA_MODEL: Self = Self(6);
}

/// What [`write()`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a cache left alone was not written"]
pub enum Written {
    /// The file now holds the body.
    Written,
    /// The file holds the same kind of cache in a newer `format` and was
    /// left as it is (P3); the caller keeps its cache in memory only.
    KeptNewer,
}

/// The header fields of a cache file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    kind: Kind,
    format: u32,
    builder: u32,
    body_len: u64,
    body_hash: [u8; 32],
}

impl Header {
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..8].copy_from_slice(&MAGIC);
        h[8..10].copy_from_slice(&self.kind.0.to_le_bytes());
        h[10..14].copy_from_slice(&self.format.to_le_bytes());
        h[14..18].copy_from_slice(&self.builder.to_le_bytes());
        // 18..22: flags, none defined; written as 0, ignored when read.
        h[22..30].copy_from_slice(&self.body_len.to_le_bytes());
        h[30..].copy_from_slice(&self.body_hash);
        h
    }

    /// The header in `h`, `None` without the magic.
    fn decode(h: &[u8; HEADER_LEN]) -> Option<Self> {
        let (magic, rest) = h.split_first_chunk::<8>()?;
        if *magic != MAGIC {
            return None;
        }
        let (kind, rest) = rest.split_first_chunk::<2>()?;
        let (format, rest) = rest.split_first_chunk::<4>()?;
        let (builder, rest) = rest.split_first_chunk::<4>()?;
        let (_flags, rest) = rest.split_first_chunk::<4>()?;
        let (body_len, rest) = rest.split_first_chunk::<8>()?;
        let body_hash: [u8; 32] = rest.try_into().ok()?;
        Some(Self {
            kind: Kind(u16::from_le_bytes(*kind)),
            format: u32::from_le_bytes(*format),
            builder: u32::from_le_bytes(*builder),
            body_len: u64::from_le_bytes(*body_len),
            body_hash,
        })
    }
}

/// The header at the start of `file`, `None` if there is none.
fn read_header(file: &mut std::fs::File) -> Option<Header> {
    let mut h = [0u8; HEADER_LEN];
    file.read_exact(&mut h).ok()?;
    Header::decode(&h)
}

/// Whether the rest of `file`, after its header `h`, is exactly the body
/// the header names: its length and its SHA-256. Read in pieces, so a big
/// file of a newer build is never held whole.
fn body_checks_out(file: &mut std::fs::File, h: &Header) -> bool {
    if file.metadata().ok().and_then(|m| m.len().checked_sub(HEADER_LEN as u64)) != Some(h.body_len) {
        return false;
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut seen = 0u64;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                seen += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    seen == h.body_len && hasher.finalize().as_slice() == h.body_hash
}

/// The body of the cache at `path`, or `None`.
///
/// `None` for a missing or unreadable file, a wrong magic, another `kind`,
/// `format` or `builder`, a file shorter or longer than its header says, or
/// a body whose hash is wrong.
///
/// The body is read once into a buffer of exactly its size: no second copy.
pub fn read(path: &Path, kind: Kind, format: u32, builder: u32) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let h = read_header(&mut file)?;
    if h.kind != kind || h.format != format || h.builder != builder {
        return None;
    }
    // Checked against the file's length before anything is allocated, so a
    // damaged length can't ask for more memory than the file has.
    if len.checked_sub(HEADER_LEN as u64) != Some(h.body_len) {
        return None;
    }
    let n = usize::try_from(h.body_len).ok()?;
    let mut body = Vec::new();
    body.try_reserve_exact(n).ok()?;
    file.take(h.body_len).read_to_end(&mut body).ok()?;
    if body.len() != n || Sha256::digest(&body).as_slice() != h.body_hash {
        return None;
    }
    Some(body)
}

/// Replace the cache at `path` with `body` (see the module docs).
///
/// Its folder is made if needed. A file there of the same `kind` and a newer `format`
/// is left alone ([`Written::KeptNewer`]) if its length and body hash check out;
/// anything else there is replaced (one flipped bit in `format` must not keep a
/// damaged file forever).
pub fn write(path: &Path, kind: Kind, format: u32, builder: u32, body: &[u8]) -> std::io::Result<Written> {
    if let Ok(mut old) = std::fs::File::open(path)
        && let Some(h) = read_header(&mut old)
        && h.kind == kind
        && h.format > format
        && body_checks_out(&mut old, &h)
    {
        return Ok(Written::KeptNewer);
    }
    let header = Header { kind, format, builder, body_len: body.len() as u64, body_hash: Sha256::digest(body).into() };
    crate::fs::write_atomic_parts(path, &[&header.encode(), body], Options::STATE.lazy())?;
    Ok(Written::Written)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use super::*;

    const K: Kind = Kind(9);

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_body_comes_back_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cache").join("thing");
        assert_eq!(write(&p, K, 3, 7, b"hello").unwrap(), Written::Written);
        assert_eq!(read(&p, K, 3, 7).as_deref(), Some(&b"hello"[..]));
        assert_eq!(write(&p, K, 3, 7, b"").unwrap(), Written::Written);
        assert_eq!(read(&p, K, 3, 7).as_deref(), Some(&b""[..]));
        assert_eq!(fs::read(&p).unwrap().len(), HEADER_LEN);
        assert_eq!(&fs::read(&p).unwrap()[..8], b"QUIRECA1");
        assert_eq!(names(&dir.path().join("cache")), ["thing"], "no temporary file left");
    }

    #[test]
    fn any_doubt_is_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c");
        assert_eq!(read(&p, K, 1, 1), None, "missing");
        assert_eq!(write(&p, K, 1, 1, b"some body bytes").unwrap(), Written::Written);
        let good = fs::read(&p).unwrap();
        assert!(read(&p, K, 1, 1).is_some());
        assert_eq!(read(&p, Kind(10), 1, 1), None, "another kind");
        assert_eq!(read(&p, K, 2, 1), None, "another format");
        assert_eq!(read(&p, K, 0, 1), None, "an older format");
        assert_eq!(read(&p, K, 1, 2), None, "another builder");
        let damaged = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut b = good.clone();
            f(&mut b);
            fs::write(&p, &b).unwrap();
            read(&p, K, 1, 1)
        };
        assert_eq!(damaged(&|b| b[0] = b'X'), None, "wrong magic");
        assert_eq!(damaged(&|b| b.truncate(HEADER_LEN - 1)), None, "short file");
        assert_eq!(damaged(&|b| b.clear()), None, "empty file");
        assert_eq!(damaged(&|b| b.truncate(b.len() - 1)), None, "truncated body");
        assert_eq!(damaged(&|b| b.push(0)), None, "a byte too many");
        for i in 0..good.len() {
            if (18..22).contains(&i) {
                continue; // flags: none defined, ignored
            }
            assert_eq!(damaged(&|b| b[i] ^= 0x01), None, "byte {i} flipped");
        }
        assert_eq!(damaged(&|b| b[20] = 0xff), Some(b"some body bytes".to_vec()), "flags are ignored");
        // A length bigger than the file allocates nothing.
        assert_eq!(damaged(&|b| b[22..30].copy_from_slice(&u64::MAX.to_le_bytes())), None);
        // A folder where the file should be.
        assert_eq!(read(dir.path(), K, 1, 1), None);
    }

    #[test]
    fn a_newer_format_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c");
        assert_eq!(write(&p, K, 5, 1, b"from a newer build").unwrap(), Written::Written);
        assert_eq!(write(&p, K, 4, 1, b"mine").unwrap(), Written::KeptNewer);
        assert_eq!(read(&p, K, 5, 1).as_deref(), Some(&b"from a newer build"[..]));
        // The same or an older format, another kind, or junk: replaced.
        assert_eq!(write(&p, K, 5, 2, b"same format").unwrap(), Written::Written);
        assert_eq!(write(&p, K, 6, 2, b"newer").unwrap(), Written::Written);
        assert_eq!(write(&p, Kind(10), 1, 1, b"other kind").unwrap(), Written::Written);
        assert_eq!(read(&p, Kind(10), 1, 1).as_deref(), Some(&b"other kind"[..]));
        fs::write(&p, b"junk").unwrap();
        assert_eq!(write(&p, K, 1, 1, b"x").unwrap(), Written::Written);
        assert_eq!(read(&p, K, 1, 1).as_deref(), Some(&b"x"[..]));
    }

    /// What a test does to a file's bytes, and its name.
    type Damage<'a> = (&'a str, &'a dyn Fn(&mut Vec<u8>));

    #[test]
    fn a_damaged_file_that_only_looks_newer_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c");
        assert_eq!(write(&p, K, 1, 1, b"a body of some length").unwrap(), Written::Written);
        let good = fs::read(&p).unwrap();
        // One flipped bit in `format` (1 becomes 0x0100_0001) makes the file
        // look newer. With any other damage (body, hash, length) it fails
        // the checks and is replaced.
        let cases: [Damage<'_>; 4] = [
            ("format bit and a body byte", &|b| {
                b[13] ^= 0x01;
                b[HEADER_LEN] ^= 0x01;
            }),
            ("format bit and the hash", &|b| {
                b[13] ^= 0x01;
                b[40] ^= 0x01;
            }),
            ("format bit and a truncated body", &|b| {
                b[13] ^= 0x01;
                b.truncate(b.len() - 1);
            }),
            ("format bit and the length", &|b| {
                b[13] ^= 0x01;
                b[22] ^= 0x01;
            }),
        ];
        for (what, damage) in cases {
            let mut b = good.clone();
            damage(&mut b);
            fs::write(&p, &b).unwrap();
            assert_eq!(write(&p, K, 1, 1, b"mine").unwrap(), Written::Written, "{what}");
            assert_eq!(read(&p, K, 1, 1).as_deref(), Some(&b"mine"[..]), "{what}");
            fs::write(&p, &good).unwrap();
        }
        // The flipped format alone, with a sound body, can't be told from a
        // real newer file (the hash covers the body, not the header): kept.
        let mut b = good;
        b[13] ^= 0x01;
        fs::write(&p, &b).unwrap();
        assert_eq!(write(&p, K, 1, 1, b"mine").unwrap(), Written::KeptNewer);
    }

    #[test]
    fn a_failed_write_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c");
        assert_eq!(write(&p, K, 1, 1, b"old").unwrap(), Written::Written);
        assert!(crate::fs::failing(|| write(&p, K, 1, 1, b"new")).is_err());
        assert_eq!(read(&p, K, 1, 1).as_deref(), Some(&b"old"[..]));
        assert_eq!(names(dir.path()), ["c"]);
    }

    #[test]
    fn the_kinds_are_distinct() {
        let all =
            [Kind::MANIFEST, Kind::RECORDS_INDEX, Kind::RECORDS_SEGMENT, Kind::PASSAGES_INDEX, Kind::PASSAGES_SEGMENT, Kind::LSA_MODEL];
        let set: std::collections::HashSet<Kind> = all.into_iter().collect();
        assert_eq!(set.len(), all.len());
    }

    /// Budget (`docs/design/0.14-build-plan.md`, A1; report only): a 5 MB
    /// body reads in 30 ms or less, into one buffer of exactly its size.
    #[test]
    fn a_five_megabyte_body_reads_quickly_into_one_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big");
        let body: Vec<u8> = (0..5_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        assert_eq!(write(&p, K, 1, 1, &body).unwrap(), Written::Written);
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let got = read(&p, K, 1, 1).unwrap();
            best = best.min(t.elapsed());
            assert_eq!(got.len(), body.len());
            assert_eq!(got.capacity(), body.len(), "no spare or second buffer");
            assert_eq!(got, body);
        }
        println!("cache::read of a 5 MB body: best of 5 {best:?} (budget 30 ms, report only)");
    }
}
