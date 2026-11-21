// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The bytes of a note's record in the startup cache
//! (`docs/design/0.14-index-startup.md` §3.6; format: `docs/formats/index-cache.md`).
//!
//! Hand-written, chosen by measurement (`docs/design/0.14-index-startup.md` §3.6,
//! its table: it decodes 4.5 times faster than `serde_json`). Every
//! encoder destructures its type without `..`, so a new field in
//! `quire_markdown::{Link, Task, Heading, Stats}` or [`NoteRecord`] does not
//! compile until it is encoded here; the round-trip test and the golden
//! `DERIVE_VERSION` test catch the rest.
//!
//! Integers are unsigned LEB128 varints unless said otherwise; a string is
//! its byte length and its UTF-8 bytes; an option is a byte 0 or 1, then the
//! value; an `f32` is its 4 bytes of bits, little-endian. The same `Writer` and
//! `Reader` frame `records.idx` and segments. Decoding never panics: any
//! doubt is `None`, and the caller treats the whole file as "no cache".

use std::collections::BTreeMap;
use std::ops::Range;

use quire_markdown::{Heading, Link, LinkKind, Stats, Task, Value};
use quire_vault::NotePath;

use crate::record::NoteRecord;

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

    pub(crate) fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.usize(b.len());
        self.buf.extend_from_slice(b);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    fn opt_str(&mut self, s: Option<&str>) {
        self.bool(s.is_some());
        if let Some(s) = s {
            self.str(s);
        }
    }

    fn range(&mut self, r: &Range<usize>) {
        self.usize(r.start);
        self.usize(r.end);
    }

    fn strs(&mut self, v: &[String]) {
        self.usize(v.len());
        for s in v {
            self.str(s);
        }
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

    pub(crate) fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(u32::from_le_bytes(self.take(4)?.try_into().ok()?)))
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

    fn string(&mut self) -> Option<String> {
        self.str().map(str::to_string)
    }

    fn bool(&mut self) -> Option<bool> {
        match self.take(1)? {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }

    #[expect(clippy::option_option, reason = "the outer None is damaged bytes, the inner one an absent value")]
    fn opt_string(&mut self) -> Option<Option<String>> {
        if self.bool()? { self.string().map(Some) } else { Some(None) }
    }

    fn range(&mut self) -> Option<Range<usize>> {
        let start = self.usize()?;
        let end = self.usize()?;
        (start <= end).then_some(start..end)
    }

    fn strings(&mut self) -> Option<Vec<String>> {
        let n = self.count()?;
        (0..n).map(|_| self.string()).collect()
    }
}

/// One note's record as the cache holds it: terms are ids into the
/// segment's own sorted term table.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    /// The record; its `terms` are segment-local ids (`TermId(local)`).
    pub(crate) rec: NoteRecord,
    /// BM25 terms (segment-local id, weight) and the document length.
    pub(crate) bm25: Vec<(u32, f32)>,
    pub(crate) len: f32,
}

fn encode_heading(w: &mut Writer, h: &Heading) {
    let Heading { level, text, offset, anchor } = h;
    w.buf.push(*level);
    w.str(text);
    w.usize(*offset);
    w.str(anchor);
}

fn decode_heading(r: &mut Reader<'_>) -> Option<Heading> {
    let level = *r.take(1)?.first()?;
    Some(Heading { level, text: r.string()?, offset: r.usize()?, anchor: r.string()? })
}

fn encode_link(w: &mut Writer, l: &Link) {
    let Link { kind, target, heading, display, range, field } = l;
    w.buf.push(match kind {
        LinkKind::Wiki => 0,
        LinkKind::Embed => 1,
        LinkKind::Relative => 2,
        LinkKind::External => 3,
    });
    w.str(target);
    w.opt_str(heading.as_deref());
    w.str(display);
    w.range(range);
    w.opt_str(field.as_deref());
}

fn decode_link(r: &mut Reader<'_>) -> Option<Link> {
    let kind = match r.take(1)? {
        [0] => LinkKind::Wiki,
        [1] => LinkKind::Embed,
        [2] => LinkKind::Relative,
        [3] => LinkKind::External,
        _ => return None,
    };
    Some(Link { kind, target: r.string()?, heading: r.opt_string()?, display: r.string()?, range: r.range()?, field: r.opt_string()? })
}

fn encode_task(w: &mut Writer, t: &Task) {
    let Task {
        done,
        text,
        offset,
        due,
        scheduled,
        start,
        done_on,
        created,
        recurrence,
        priority,
        reminder,
        time_block,
        meta_range,
        time_spent,
        estimate,
    } = t;
    w.bool(*done);
    w.str(text);
    w.usize(*offset);
    for s in [due, scheduled, start, done_on, created, recurrence] {
        w.opt_str(s.as_deref());
    }
    w.bool(priority.is_some());
    if let Some(p) = priority {
        w.buf.push(*p);
    }
    w.opt_str(reminder.as_deref());
    w.bool(time_block.is_some());
    if let Some((a, b)) = time_block {
        w.str(a);
        w.str(b);
    }
    w.bool(meta_range.is_some());
    if let Some(m) = meta_range {
        w.range(m);
    }
    w.opt_str(time_spent.as_deref());
    w.opt_str(estimate.as_deref());
}

fn decode_task(r: &mut Reader<'_>) -> Option<Task> {
    let done = r.bool()?;
    let text = r.string()?;
    let offset = r.usize()?;
    let due = r.opt_string()?;
    let scheduled = r.opt_string()?;
    let start = r.opt_string()?;
    let done_on = r.opt_string()?;
    let created = r.opt_string()?;
    let recurrence = r.opt_string()?;
    let priority = if r.bool()? { Some(*r.take(1)?.first()?) } else { None };
    let reminder = r.opt_string()?;
    let time_block = if r.bool()? { Some((r.string()?, r.string()?)) } else { None };
    let meta_range = if r.bool()? { Some(r.range()?) } else { None };
    let time_spent = r.opt_string()?;
    let estimate = r.opt_string()?;
    Some(Task {
        done,
        text,
        offset,
        due,
        scheduled,
        start,
        done_on,
        created,
        recurrence,
        priority,
        reminder,
        time_block,
        meta_range,
        time_spent,
        estimate,
    })
}

fn encode_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Str(s) => {
            w.buf.push(0);
            w.str(s);
        }
        Value::List(l) => {
            w.buf.push(1);
            w.strs(l);
        }
        Value::Map(m) => {
            w.buf.push(2);
            w.usize(m.len());
            for (k, v) in m {
                w.str(k);
                encode_value(w, v);
            }
        }
    }
}

/// Front matter nests at most 64 levels (`frontmatter::load_yaml`); a deeper
/// value here is damage.
fn decode_value(r: &mut Reader<'_>, depth: usize) -> Option<Value> {
    if depth > 64 {
        return None;
    }
    Some(match r.take(1)? {
        [0] => Value::Str(r.string()?),
        [1] => Value::List(r.strings()?),
        [2] => {
            let n = r.count()?;
            let mut m = BTreeMap::new();
            for _ in 0..n {
                let k = r.string()?;
                m.insert(k, decode_value(r, depth + 1)?);
            }
            Value::Map(m)
        }
        _ => return None,
    })
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
    w.str(path.as_str());
    w.str(name);
    w.str(title);
    w.strs(aliases);
    w.usize(headings.len());
    for h in headings {
        encode_heading(w, h);
    }
    w.usize(links.len());
    for l in links {
        encode_link(w, l);
    }
    w.strs(tags);
    w.usize(tasks.len());
    for t in tasks {
        encode_task(w, t);
    }
    let Stats { words, chars, lines, reading_minutes } = stats;
    for n in [words, chars, lines, reading_minutes] {
        w.usize(*n);
    }
    w.usize(properties.len());
    for (k, v) in properties {
        w.str(k);
        encode_value(w, v);
    }
    w.str(plain);
    w.usize(content.len());
    for (t, n) in content {
        w.u64(u64::from(*t));
        w.u64(u64::from(*n));
    }
    w.usize(bm25.len());
    for (t, f) in bm25 {
        w.u64(u64::from(*t));
        w.f32(*f);
    }
    w.f32(len);
}

/// Read a record written by [`encode_record`]; term ids must be below `terms`.
pub(crate) fn decode_record(r: &mut Reader<'_>, terms: usize) -> Option<Stored> {
    let path = NotePath::new(r.str()?);
    let name = r.string()?;
    let title = r.string()?;
    let aliases = r.strings()?;
    let headings = (0..r.count()?).map(|_| decode_heading(r)).collect::<Option<Vec<_>>>()?;
    let links = (0..r.count()?).map(|_| decode_link(r)).collect::<Option<Vec<_>>>()?;
    let tags = r.strings()?;
    let tasks = (0..r.count()?).map(|_| decode_task(r)).collect::<Option<Vec<_>>>()?;
    let stats = Stats { words: r.usize()?, chars: r.usize()?, lines: r.usize()?, reading_minutes: r.usize()? };
    let mut properties = BTreeMap::new();
    for _ in 0..r.count()? {
        let k = r.string()?;
        properties.insert(k, decode_value(r, 0)?);
    }
    let plain = r.string()?;
    let id = |r: &mut Reader<'_>| r.u64().and_then(|t| u32::try_from(t).ok()).filter(|t| (*t as usize) < terms);
    let content = (0..r.count()?).map(|_| Some((crate::TermId(id(r)?), u32::try_from(r.u64()?).ok()?))).collect::<Option<Vec<_>>>()?;
    let bm25 = (0..r.count()?).map(|_| Some((id(r)?, r.f32()?))).collect::<Option<Vec<_>>>()?;
    let len = r.f32()?;
    let rec = NoteRecord {
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
        terms: content,
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
