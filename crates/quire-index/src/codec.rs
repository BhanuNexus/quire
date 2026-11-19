// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bytes of the startup cache (`docs/design/0.14-index-startup.md` §3.6;
//! format: `docs/formats/index-cache.md`).
//!
//! A note's record is JSON through serde derives, chosen by measurement
//! (`docs/design/0.14-index-startup.md` §3.6, its table): a new field of
//! `quire_markdown::{Link, Task, Heading, Stats}` is encoded without anyone
//! remembering to, and [`encode_record`] destructures [`NoteRecord`] without
//! `..`, so a new field there does not compile until it is placed. The
//! golden `DERIVE_VERSION` test catches a change in what is derived.
//!
//! The framing around records (index rows, segment headers) uses
//! [`Writer`] and [`Reader`]: unsigned LEB128 varints, a string as its byte
//! length and its UTF-8 bytes. Decoding never panics: any doubt is `None`,
//! and the caller treats the whole file as "no cache".

use std::collections::BTreeMap;

use quire_markdown::{Heading, Link, Stats, Task, Value};
use quire_vault::NotePath;
use serde::{Deserialize, Serialize};

use crate::record::NoteRecord;
use crate::terms::TermId;

/// Bytes being written.
#[derive(Debug, Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn u64(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }

    pub(crate) fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }

    pub(crate) fn i64(&mut self, v: i64) {
        // Zigzag, so small negative numbers stay short.
        self.u64(((v << 1) ^ (v >> 63)) as u64);
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.usize(b.len());
        self.buf.extend_from_slice(b);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

/// Bytes being read; every read is `None` past the end or on bad data.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = self.buf.split_first()?;
            self.buf = rest;
            let part = u64::from(b & 0x7f);
            if shift == 63 && part > 1 {
                return None;
            }
            v |= part << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    pub(crate) fn usize(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
    }

    /// A count of items each at least one byte long: never more than the
    /// bytes left, so a damaged count can't ask for a huge allocation.
    pub(crate) fn count(&mut self) -> Option<usize> {
        self.usize().filter(|n| *n <= self.buf.len())
    }

    pub(crate) fn i64(&mut self) -> Option<i64> {
        let v = self.u64()?;
        Some(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.buf.len() {
            return None;
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Some(a)
    }

    pub(crate) fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.usize()?;
        self.take(n)
    }

    pub(crate) fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }
}

/// One note's record as the cache holds it.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    /// The record; its `terms` are ids into the segment's term table.
    pub(crate) rec: NoteRecord,
    /// BM25 terms (segment-local id, weight) and the document length.
    pub(crate) bm25: Vec<(u32, f32)>,
    pub(crate) len: f32,
}

/// A record as written: borrowed, so encoding copies nothing.
#[derive(Serialize)]
struct RecordOut<'a> {
    path: &'a str,
    name: &'a str,
    title: &'a str,
    aliases: &'a [String],
    headings: &'a [Heading],
    links: &'a [Link],
    tags: &'a [String],
    tasks: &'a [Task],
    stats: &'a Stats,
    properties: &'a BTreeMap<String, Value>,
    plain: &'a str,
    content: &'a [(u32, u32)],
    bm25: &'a [(u32, f32)],
    len: f32,
}

/// A record as read. Its fields are [`RecordOut`]'s: one missing or of
/// another shape fails the segment.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordIn {
    path: String,
    name: String,
    title: String,
    aliases: Vec<String>,
    headings: Vec<Heading>,
    links: Vec<Link>,
    tags: Vec<String>,
    tasks: Vec<Task>,
    stats: Stats,
    properties: BTreeMap<String, Value>,
    plain: String,
    content: Vec<(u32, u32)>,
    bm25: Vec<(u32, f32)>,
    len: f32,
}

/// Write a record. `content` and `bm25` are segment-local term ids.
pub(crate) fn encode_record(w: &mut Writer, rec: &NoteRecord, content: &[(u32, u32)], bm25: &[(u32, f32)], len: f32) {
    // No `..`: a new field must be encoded (or said to be derived) here.
    let NoteRecord {
        path,
        name,
        title,
        aliases,
        headings,
        links,
        tags,
        tasks,
        stats,
        properties,
        plain,
        terms: _,        // `content`: the ids of the segment
        content: _,      // the same terms as text, only before an insert
        search_terms: _, // `bm25` and `len`: what the keyword index holds
    } = rec;
    let out =
        RecordOut { path: path.as_str(), name, title, aliases, headings, links, tags, tasks, stats, properties, plain, content, bm25, len };
    // Serialising these types into memory can't fail: no map has a key that
    // isn't a string and no weight is NaN.
    let json = serde_json::to_vec(&out).unwrap_or_default();
    w.bytes(&json);
}

/// Read a record written by [`encode_record`]; term ids must be below `terms`.
pub(crate) fn decode_record(r: &mut Reader<'_>, terms: usize) -> Option<Stored> {
    let RecordIn { path, name, title, aliases, headings, links, tags, tasks, stats, properties, plain, content, bm25, len } =
        serde_json::from_slice(r.bytes()?).ok()?;
    let ok = |t: u32| (t as usize) < terms;
    if !content.iter().all(|(t, _)| ok(*t)) || !bm25.iter().all(|(t, _)| ok(*t)) || path.is_empty() {
        return None;
    }
    let rec = NoteRecord {
        path: NotePath::new(path),
        name,
        title,
        aliases,
        headings,
        links,
        tags,
        tasks,
        stats,
        properties,
        plain,
        terms: content.into_iter().map(|(t, n)| (TermId(t), n)).collect(),
        content: Vec::new(),
        search_terms: (Vec::new(), 0.0),
    };
    Some(Stored { rec, bm25, len })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Notes that use every field of a record, edge cases included.
    pub(crate) const CORPUS: &[&str] = &[
        "",
        "# Title\nplain words [[Link#Head|shown]] ![[pic.png]] [x](Other.md) [web](https://example.com)\n",
        "---\ntitle: T\naliases: [A1, \"A 2\"]\ntags: [x, y/z]\nauthor:\n  name: Ada\n  langs: [en, fr]\nup: \"[[Parent]]\"\n---\n# H1\n## H2 {#id}\n",
        "- [ ] task 📅 2026-12-01 ⏳ 2026-11-30 🛫 2026-11-01 ➕ 2026-10-01 🔁 every week ⏫ ⏰ 2026-12-01 09:00 [time:: 1h] [estimate:: 2h]\n- [x] done ✅ 2026-11-02\n- [ ] 14:00 - 15:30 block\n",
        "supports:: [[Claim]]\n[parent:: [[P]]] #tag/sub and #Tag\nÜnïcödé café Łódź\n",
        "```\ncode [[not a link]]\n```\n$$x$$ ==mark== %%comment%%\n",
    ];

    fn round_trip(src: &str) {
        let rec = NoteRecord::from_source(NotePath::new("Folder/Some note.md"), src);
        let mut table: Vec<&str> =
            rec.content.iter().map(|(t, _)| t.as_str()).chain(rec.search_terms.0.iter().map(|(t, _)| t.as_str())).collect();
        table.sort_unstable();
        table.dedup();
        let local = |t: &str| u32::try_from(table.binary_search(&t).expect("in table")).expect("small");
        let content: Vec<(u32, u32)> = rec.content.iter().map(|(t, n)| (local(t), *n)).collect();
        let bm25: Vec<(u32, f32)> = rec.search_terms.0.iter().map(|(t, f)| (local(t), *f)).collect();
        let mut w = Writer::default();
        encode_record(&mut w, &rec, &content, &bm25, rec.search_terms.1);
        let mut r = Reader::new(&w.buf);
        let got = decode_record(&mut r, table.len()).expect("decodes");
        assert!(r.is_empty(), "every byte read");
        let g = &got.rec;
        assert_eq!(
            (&g.path, &g.name, &g.title, &g.aliases, &g.headings, &g.links, &g.tags, &g.tasks, g.stats, &g.properties, &g.plain),
            (
                &rec.path,
                &rec.name,
                &rec.title,
                &rec.aliases,
                &rec.headings,
                &rec.links,
                &rec.tags,
                &rec.tasks,
                rec.stats,
                &rec.properties,
                &rec.plain
            )
        );
        let back: Vec<(String, u32)> = g.terms.iter().map(|(t, n)| (table[t.index()].to_string(), *n)).collect();
        assert_eq!(back, rec.content);
        let back: Vec<(String, f32)> = got.bm25.iter().map(|(t, f)| (table[*t as usize].to_string(), *f)).collect();
        assert_eq!(back, rec.search_terms.0);
        assert_eq!(got.len.to_bits(), rec.search_terms.1.to_bits());
        // Any cut is "no record", never a panic.
        for cut in 0..w.buf.len() {
            let mut r = Reader::new(&w.buf[..cut]);
            assert!(decode_record(&mut r, table.len()).is_none(), "cut at {cut}");
        }
    }

    #[test]
    fn records_round_trip() {
        for src in CORPUS {
            round_trip(src);
        }
        // Generated notes: every corpus line in random orders and numbers.
        let mut seed = 0x0bad_5eed_u64;
        for _ in 0..200 {
            let mut s = String::new();
            for _ in 0..(seed % 9) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                s.push_str(CORPUS[(seed % CORPUS.len() as u64) as usize]);
            }
            round_trip(&s);
        }
    }

    #[test]
    fn varints_and_damage() {
        let mut w = Writer::default();
        for v in [0, 1, 127, 128, 300, u64::MAX] {
            w.u64(v);
        }
        for v in [0, -1, 1, i64::MIN, i64::MAX] {
            w.i64(v);
        }
        let mut r = Reader::new(&w.buf);
        for v in [0, 1, 127, 128, 300, u64::MAX] {
            assert_eq!(r.u64(), Some(v));
        }
        for v in [0, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(r.i64(), Some(v));
        }
        assert!(r.is_empty());
        // Eleven continuation bytes, or a tenth byte above 1: not a u64.
        assert_eq!(Reader::new(&[0xff; 11]).u64(), None);
        assert_eq!(Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]).u64(), None);
        // A count larger than what is left.
        assert_eq!(Reader::new(&[5, 1]).count(), None);
    }
}
