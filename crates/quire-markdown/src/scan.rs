// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scanner for Obsidian-style extensions that `CommonMark` does not know about:
//! `[[wikilinks]]`, `![[embeds]]`, `#nested/tags` and `==highlights==`.
//!
//! The scanner runs over the raw source but skips "verbatim" regions (code,
//! math, HTML, front matter) reported by the `CommonMark` parser.

use std::ops::Range;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// Note name (without heading/alias), trimmed.
    pub target: String,
    pub heading: Option<String>,
    pub alias: Option<String>,
    pub embed: bool,
}

impl WikiLink {
    /// Text shown to readers.
    pub fn display(&self) -> &str {
        self.alias.as_deref().unwrap_or(match &self.heading {
            Some(h) if self.target.is_empty() => h,
            _ => &self.target,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    WikiLink { range: Range<usize>, link: WikiLink },
    Tag { range: Range<usize>, name: String },
    Highlight { range: Range<usize> },
}

impl Inline {
    pub fn range(&self) -> Range<usize> {
        match self {
            Self::WikiLink { range, .. } | Self::Tag { range, .. } | Self::Highlight { range } => range.clone(),
        }
    }
}

/// Byte ranges whose content must be treated literally (code, math, HTML,
/// front matter, link destinations), sorted and merged.
pub fn verbatim_ranges(src: &str) -> Vec<Range<usize>> {
    let mut v = Verbatim::default();
    let parsed = crate::parse_source(src);
    for (ev, range) in Parser::new_ext(&parsed, crate::options()).into_offset_iter() {
        v.event(src, &ev, &range);
    }
    v.finish()
}

/// Collects [`verbatim_ranges`] from parser events, so `extract` gets them
/// from the parse it already does (a third parse was 9% of indexing, 0.10
/// perf).
#[derive(Debug, Default)]
pub(crate) struct Verbatim {
    out: Vec<Range<usize>>,
    open: Option<usize>,
}

impl Verbatim {
    pub(crate) fn event(&mut self, src: &str, ev: &Event<'_>, range: &Range<usize>) {
        match ev {
            Event::Start(Tag::CodeBlock(_) | Tag::MetadataBlock(_) | Tag::HtmlBlock) => self.open = Some(range.start),
            Event::End(TagEnd::CodeBlock | TagEnd::MetadataBlock(_) | TagEnd::HtmlBlock) => {
                if let Some(s) = self.open.take() {
                    self.out.push(s..range.end);
                }
            }
            Event::Code(_) | Event::InlineMath(_) | Event::DisplayMath(_) | Event::InlineHtml(_) | Event::Html(_) => {
                self.out.push(range.clone());
            }
            Event::Start(Tag::Link { .. }) => {
                // The destination part `(...)` of an inline link is verbatim too.
                if let Some(off) = src.get(range.clone()).and_then(|r| r.rfind("](")) {
                    self.out.push(range.start + off + 1..range.end);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn finish(self) -> Vec<Range<usize>> {
        merge(self.out)
    }
}

pub(crate) fn merge(mut v: Vec<Range<usize>>) -> Vec<Range<usize>> {
    v.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(v.len());
    for r in v {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// The range of `ranges` (sorted and merged, as [`merge`] leaves them) that holds `pos`, if any.
pub(crate) fn covering(ranges: &[Range<usize>], pos: usize) -> Option<&Range<usize>> {
    let i = ranges.partition_point(|r| r.end <= pos);
    ranges.get(i).filter(|r| r.start <= pos)
}

/// Asks whether a line overlaps a sorted, merged list of ranges (code, math, front
/// matter), for a caller that walks the lines of a note in order: the index only
/// moves forward, so all the lines together cost O(lines + ranges) and not
/// O(lines x ranges) (`docs/design/0.14-index-startup.md` §3.7).
#[derive(Debug)]
pub(crate) struct Overlap<'a> {
    ranges: &'a [Range<usize>],
    /// Every range before this one ends at or before `from`.
    next: usize,
    /// The `from` asked last.
    from: usize,
}

impl<'a> Overlap<'a> {
    pub(crate) const fn new(ranges: &'a [Range<usize>]) -> Self {
        Self { ranges, next: 0, from: 0 }
    }

    /// Does `from..to` (not empty) overlap a range? A `from` before the last one
    /// asked is answered too, by walking from the first range again.
    pub(crate) fn hits(&mut self, from: usize, to: usize) -> bool {
        if from.cmp(&self.from).is_lt() {
            self.next = 0;
        }
        self.from = from;
        while self.ranges.get(self.next).is_some_and(|r| r.end <= from) {
            self.next += 1;
        }
        self.ranges.get(self.next).is_some_and(|r| r.start < to)
    }
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// Scan `src` for extension inlines, skipping verbatim regions.
pub fn scan_inlines(src: &str) -> Vec<Inline> {
    let verbatim = verbatim_ranges(src);
    scan_with(src, &verbatim)
}

pub(crate) fn scan_with(src: &str, verbatim: &[Range<usize>]) -> Vec<Inline> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut cursor = Cursor::new(src);
    let mut i = 0;
    while i < bytes.len() {
        if let Some(r) = covering(verbatim, i) {
            i = r.end;
            continue;
        }
        match bytes[i] {
            b'[' if bytes.get(i + 1) == Some(&b'[') => {
                if let Some((inline, end)) = cursor.wikilink_at(i) {
                    out.push(inline);
                    i = end;
                    continue;
                }
            }
            b'!' if src[i..].starts_with("![[") => {
                if let Some((inline, end)) = cursor.wikilink_at(i + 1) {
                    if let Inline::WikiLink { range, mut link } = inline {
                        link.embed = true;
                        out.push(Inline::WikiLink { range: i..range.end, link });
                    }
                    i = end;
                    continue;
                }
            }
            b'#' => {
                let prev = src[..i].chars().next_back();
                if prev.is_none_or(|c| c.is_whitespace() || c == '(' || c == ',') {
                    let rest = &src[i + 1..];
                    let len: usize = rest.chars().take_while(|&c| is_tag_char(c)).map(char::len_utf8).sum();
                    let name = rest[..len].trim_end_matches('/');
                    if !name.is_empty() && !name.chars().all(|c| c.is_ascii_digit()) {
                        let end = i + 1 + name.len();
                        out.push(Inline::Tag { range: i..end, name: name.to_string() });
                        i = end;
                        continue;
                    }
                }
            }
            b'=' if bytes.get(i + 1) == Some(&b'=') => {
                let body_start = i + 2;
                let line_end = cursor.line_end(body_start);
                if let Some(off) = src[body_start..line_end].find("==") {
                    let body = &src[body_start..body_start + off];
                    let ok = !body.is_empty() && !body.starts_with(char::is_whitespace) && !body.ends_with(char::is_whitespace);
                    if ok {
                        let end = body_start + off + 2;
                        out.push(Inline::Highlight { range: i..end });
                        i = end;
                        continue;
                    }
                }
            }
            _ => {}
        }
        i += src[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// The next place a needle occurs at or after a position that only moves
/// forward, remembered between calls: the text is searched once per needle
/// however many positions ask (`docs/design/0.14-index-startup.md` §3.7).
#[derive(Debug, Clone, Copy, Default)]
struct Next {
    /// The position asked about last; a later question from an earlier one starts over.
    from: usize,
    /// The first hit at or after `from`; `None` once the rest of the text has none.
    hit: Option<usize>,
    searched: bool,
}

impl Next {
    fn find(&mut self, hay: &str, from: usize, find: impl Fn(&str) -> Option<usize>) -> Option<usize> {
        let behind = self.hit.is_some_and(|h| h < from);
        if !self.searched || from < self.from || behind {
            self.hit = find(&hay[from..]).map(|n| from + n);
            self.searched = true;
        }
        self.from = from;
        self.hit
    }
}

/// A forward-only reader of `text` for the scanners: where the next `\n`, the
/// next `]]` and the next `[[` are, each searched only when the last answer is
/// behind the asking position, so a scan that asks about every `[` reads the text
/// O(length) times in all and not once per `[`.
///
/// `base` is where `text` starts in the note: positions in and out are the
/// note's, so a caller can give a cursor one line and keep note offsets.
#[derive(Debug)]
pub(crate) struct Cursor<'a> {
    text: &'a str,
    base: usize,
    newline: Next,
    close: Next,
    open: Next,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self::at(text, 0)
    }

    /// A cursor over `text`, which starts at byte `base` of the note.
    pub(crate) fn at(text: &'a str, base: usize) -> Self {
        Self { text, base, newline: Next::default(), close: Next::default(), open: Next::default() }
    }

    /// The byte of the next `\n` at or after `from`, or the end of the text.
    pub(crate) fn line_end(&mut self, from: usize) -> usize {
        self.newline_at(from).unwrap_or(self.base + self.text.len())
    }

    /// The byte of the next `]]` at or after `from`.
    pub(crate) fn close_at(&mut self, from: usize) -> Option<usize> {
        self.close.find(self.text, from - self.base, |t| t.find("]]")).map(|n| n + self.base)
    }

    fn newline_at(&mut self, from: usize) -> Option<usize> {
        self.newline.find(self.text, from - self.base, |t| t.find('\n')).map(|n| n + self.base)
    }

    fn open_at(&mut self, from: usize) -> Option<usize> {
        self.open.find(self.text, from - self.base, |t| t.find("[[")).map(|n| n + self.base)
    }

    /// The wikilink starting at `start` (its `[[`), and the byte after it.
    ///
    /// There is one when the next `]]` comes before the next `\n` and no `[[`
    /// lies between the two opening brackets and it.
    pub(crate) fn wikilink_at(&mut self, start: usize) -> Option<(Inline, usize)> {
        let body_start = start + 2;
        let close = self.close_at(body_start)?;
        if close == body_start
            // cargo-mutants: equivalent: `n` is a `\n` and `close` a `]`, so they are never the same byte and `<=` answers like `<`.
            || self.newline_at(body_start).is_some_and(|n| n < close)
            || self.open_at(body_start).is_some_and(|o| o + 2 <= close)
        {
            return None;
        }
        let inner = &self.text[body_start - self.base..close - self.base];
        let (target_part, alias) = match inner.split_once('|') {
            Some((t, a)) => (t, Some(a.trim().to_string()).filter(|a| !a.is_empty())),
            None => (inner, None),
        };
        let (target, heading) = match target_part.split_once('#') {
            Some((t, h)) => (t.trim(), Some(h.trim().to_string()).filter(|h| !h.is_empty())),
            None => (target_part.trim(), None),
        };
        if target.is_empty() && heading.is_none() {
            return None;
        }
        let end = close + 2;
        let link = WikiLink { target: target.to_string(), heading, alias, embed: false };
        Some((Inline::WikiLink { range: start..end, link }, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str) -> Vec<String> {
        scan_inlines(src)
            .into_iter()
            .map(|i| match i {
                Inline::WikiLink { link, .. } => format!("link:{}", link.target),
                Inline::Tag { name, .. } => format!("tag:{name}"),
                Inline::Highlight { range } => format!("hl:{}", &src[range]),
            })
            .collect()
    }

    #[test]
    fn finds_extensions_and_skips_code() {
        let src = "See [[Rust Notes#Ropes|ropes]] and #rust/perf ==key== idea.\n\n`[[not a link]] #nottag`\n\n```\n#nope [[nope]]\n```\n# Heading not tag\nissue #42 is not a tag\n";
        assert_eq!(names(src), vec!["link:Rust Notes", "tag:rust/perf", "hl:==key=="]);
    }

    #[test]
    fn parses_wikilink_parts() {
        let v = scan_inlines("![[Diagram.png]] [[#Local heading]]");
        let Inline::WikiLink { link, range } = &v[0] else { panic!() };
        assert!(link.embed);
        assert_eq!(*range, 0..16);
        let Inline::WikiLink { link, .. } = &v[1] else { panic!() };
        assert_eq!(link.target, "");
        assert_eq!(link.display(), "Local heading");
    }

    #[test]
    fn the_scan_carries_on_after_each_kind_and_after_verbatim_text() {
        // Ranges, one after another, with code, a link target and an unclosed form between them.
        let src = "[[a]] #t ==h== `[[no]]` [[b|c]] ![[e]] [x](#y) ==open #z\n``` \n#no\n```\n[[last]]";
        let got: Vec<(usize, usize)> = scan_inlines(src).iter().map(|i| (i.range().start, i.range().end)).collect();
        let at = |needle: &str| {
            let s = src.find(needle).unwrap();
            (s, s + needle.len())
        };
        assert_eq!(got, vec![at("[[a]]"), at("#t"), at("==h=="), at("[[b|c]]"), at("![[e]]"), at("#z"), at("[[last]]")]);
    }

    #[test]
    fn wikilink_edges() {
        assert!(names("[[]] [[ ]] [[|x]] [[a\nb]] [[a [[b]]").iter().all(|n| n != "link:"), "{:?}", names("[[]] [[|x]]"));
        assert_eq!(names("[[a [[b]]"), vec!["link:b"], "the inner link wins over an unclosed outer one");
        assert_eq!(names("[[a#h|al]] [[#only]]"), vec!["link:a", "link:"]);
        assert_eq!(names("==a== == b== ==c ==="), vec!["hl:==a=="]);
    }

    #[test]
    fn tag_in_url_is_ignored() {
        assert!(names("visit https://x.dev/#anchor now").is_empty());
        assert_eq!(names("(#idea), #todo"), vec!["tag:idea", "tag:todo"]);
    }

    /// The scanner as it was before the cursor (a `find` from every `[`), kept as the
    /// oracle: the new one must give the same inlines on every text.
    mod old {
        use super::super::{Inline, WikiLink, covering, is_tag_char};
        use std::ops::Range;

        pub(super) fn scan_with(src: &str, verbatim: &[Range<usize>]) -> Vec<Inline> {
            let bytes = src.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if let Some(r) = covering(verbatim, i) {
                    i = r.end;
                    continue;
                }
                match bytes[i] {
                    b'[' if bytes.get(i + 1) == Some(&b'[') => {
                        if let Some((inline, end)) = wikilink_at(src, i) {
                            out.push(inline);
                            i = end;
                            continue;
                        }
                    }
                    b'!' if src[i..].starts_with("![[") => {
                        if let Some((inline, end)) = wikilink_at(src, i + 1) {
                            if let Inline::WikiLink { range, mut link } = inline {
                                link.embed = true;
                                out.push(Inline::WikiLink { range: i..range.end, link });
                            }
                            i = end;
                            continue;
                        }
                    }
                    b'#' => {
                        let prev = src[..i].chars().next_back();
                        if prev.is_none_or(|c| c.is_whitespace() || c == '(' || c == ',') {
                            let rest = &src[i + 1..];
                            let len: usize = rest.chars().take_while(|&c| is_tag_char(c)).map(char::len_utf8).sum();
                            let name = rest[..len].trim_end_matches('/');
                            if !name.is_empty() && !name.chars().all(|c| c.is_ascii_digit()) {
                                let end = i + 1 + name.len();
                                out.push(Inline::Tag { range: i..end, name: name.to_string() });
                                i = end;
                                continue;
                            }
                        }
                    }
                    b'=' if bytes.get(i + 1) == Some(&b'=') => {
                        let body_start = i + 2;
                        let line_end = src[body_start..].find('\n').map_or(src.len(), |n| body_start + n);
                        if let Some(off) = src[body_start..line_end].find("==") {
                            let body = &src[body_start..body_start + off];
                            let ok = !body.is_empty() && !body.starts_with(char::is_whitespace) && !body.ends_with(char::is_whitespace);
                            if ok {
                                let end = body_start + off + 2;
                                out.push(Inline::Highlight { range: i..end });
                                i = end;
                                continue;
                            }
                        }
                    }
                    _ => {}
                }
                i += src[i..].chars().next().map_or(1, char::len_utf8);
            }
            out
        }

        pub(super) fn wikilink_at(src: &str, start: usize) -> Option<(Inline, usize)> {
            let body_start = start + 2;
            let rest = &src[body_start..];
            let close = rest.find("]]")?;
            let inner = &rest[..close];
            if inner.is_empty() || inner.contains('\n') || inner.contains("[[") {
                return None;
            }
            let (target_part, alias) = match inner.split_once('|') {
                Some((t, a)) => (t, Some(a.trim().to_string()).filter(|a| !a.is_empty())),
                None => (inner, None),
            };
            let (target, heading) = match target_part.split_once('#') {
                Some((t, h)) => (t.trim(), Some(h.trim().to_string()).filter(|h| !h.is_empty())),
                None => (target_part.trim(), None),
            };
            if target.is_empty() && heading.is_none() {
                return None;
            }
            let end = body_start + close + 2;
            let link = WikiLink { target: target.to_string(), heading, alias, embed: false };
            Some((Inline::WikiLink { range: start..end, link }, end))
        }
    }

    /// Text made of the pieces the scanner looks at, so brackets, marks, line ends
    /// and code meet in every order.
    fn pieces() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;
        let piece = prop::sample::select(vec![
            "[", "[[", "]", "]]", "![[", "!", "\n", "==", "=", "#", "#tag", "|", "a", " ", "x y", "`", "```\n", "$$", "\u{e9}", "\r\n",
            "> ", "(", "](u)",
        ]);
        prop::collection::vec(piece, 0..60).prop_map(|v| v.concat())
    }

    use crate::test_notes::fixture_notes;

    fn same_as_old(src: &str) {
        let verbatim = verbatim_ranges(src);
        assert_eq!(scan_with(src, &verbatim), old::scan_with(src, &verbatim), "{src:?}");
    }

    #[test]
    fn the_cursor_scan_gives_the_old_inlines_on_the_fixture_notes() {
        for note in fixture_notes() {
            same_as_old(&note);
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(crate::test_notes::cases(2000)))]

        #[test]
        fn the_cursor_scan_gives_the_old_inlines_on_generated_text(src in pieces()) {
            same_as_old(&src);
        }

        /// The cursor over one line, as the front-matter reader uses it, finds the
        /// link the old whole-text search found there and none that crosses the line.
        #[test]
        fn a_line_cursor_gives_the_old_link_within_the_line(src in pieces()) {
            let mut at = 0;
            for raw in src.split_inclusive('\n') {
                let text = raw.trim_end_matches(['\n', '\r']);
                let mut cursor = Cursor::at(text, at);
                for (open, _) in text.match_indices("[[") {
                    let want = old::wikilink_at(&src, at + open).filter(|(_, end)| end - at <= text.len());
                    proptest::prop_assert_eq!(cursor.wikilink_at(at + open), want);
                }
                at += raw.len();
            }
        }
    }

    #[test]
    fn a_cursor_asked_out_of_order_still_answers_right() {
        let src = "[[a]] [[b\n]] [[c]]";
        let mut cursor = Cursor::new(src);
        let at = [10, 0, 6, 14, 0, 14];
        for start in at {
            assert_eq!(cursor.wikilink_at(start), old::wikilink_at(src, start), "start {start}");
        }
    }

    #[test]
    fn the_overlap_cursor_answers_like_a_scan_of_every_range_in_any_order() {
        let ranges = vec![2..4, 6..7, 9..15, 20..21];
        let any = |a: usize, b: usize| ranges.iter().any(|r| r.start < b && a < r.end);
        let mut overlap = Overlap::new(&ranges);
        let queries: Vec<(usize, usize)> = (0..24).flat_map(|a| (a + 1..26).map(move |b| (a, b))).collect();
        // Forward, then backward, then a scrambled order: the cursor may only be faster, never different.
        let forward = queries.iter().copied();
        let backward = queries.iter().rev().copied();
        let scrambled = (0..queries.len()).map(|i| queries[(i * 37) % queries.len()]);
        for (a, b) in forward.chain(backward).chain(scrambled) {
            assert_eq!(overlap.hits(a, b), any(a, b), "{a}..{b}");
        }
        assert!(!Overlap::new(&[]).hits(0, 1));
    }

    /// A `find` that counts how often it really searches.
    fn counted<'a>(calls: &'a std::cell::Cell<u32>, needle: &'a str) -> impl Fn(&str) -> Option<usize> + 'a {
        move |t| {
            calls.set(calls.get() + 1);
            t.find(needle)
        }
    }

    #[test]
    fn next_answers_a_hit_exactly_at_the_asking_position_without_searching_again() {
        let calls = std::cell::Cell::new(0);
        let mut next = Next::default();
        assert_eq!(next.find("ab\ncd", 0, counted(&calls, "\n")), Some(2));
        assert_eq!(next.find("ab\ncd", 2, counted(&calls, "\n")), Some(2));
        assert_eq!(calls.get(), 1);
        // Past the hit it does search again, and finds there is none.
        assert_eq!(next.find("ab\ncd", 3, counted(&calls, "\n")), None);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn next_asked_again_at_the_same_position_does_not_search_again() {
        let calls = std::cell::Cell::new(0);
        let mut next = Next::default();
        assert_eq!(next.find("ab\ncd", 1, counted(&calls, "\n")), Some(2));
        assert_eq!(next.find("ab\ncd", 1, counted(&calls, "\n")), Some(2));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_cursor_with_a_base_gives_note_offsets_for_the_line_end() {
        let mut c = Cursor::at("ab\ncd", 10);
        assert_eq!(c.line_end(10), 12);
        assert_eq!(c.line_end(13), 15);
    }
}
