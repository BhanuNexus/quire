// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inverted index with BM25 ranking, prefix search-as-you-type and snippets.

use std::collections::HashMap;

use quire_vault::NotePath;
use unicode_segmentation::UnicodeSegmentation;

use crate::record::NoteRecord;
use crate::scope::DocStats;
use crate::terms::{TermId, Terms};
use crate::tokenize::{FastHash, Folded, fold_into, fold_str, is_content, stem_in_place, stem_owned, tokens};

const K1: f32 = 1.2;
const B: f32 = 0.75;
const TITLE_WEIGHT: f32 = 3.0;
const TAG_WEIGHT: f32 = 2.0;

/// Parsed query: `rope "undo tree" #rust -draft`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<String>,
    /// The last term when the user is still typing it.
    pub prefix: Option<String>,
    pub phrases: Vec<String>,
    pub tags: Vec<String>,
    pub exclude: Vec<String>,
}

impl Query {
    pub fn parse(q: &str) -> Self {
        let mut out = Self::default();
        let mut rest = q;
        while let Some(open) = rest.find('"') {
            let before = &rest[..open];
            out.push_words(before);
            match rest[open + 1..].find('"') {
                Some(close) => {
                    let phrase = fold_str(rest[open + 1..open + 1 + close].trim());
                    if !phrase.is_empty() {
                        out.terms.extend(tokens(&phrase));
                        out.phrases.push(phrase);
                    }
                    rest = &rest[open + close + 2..];
                }
                None => {
                    rest = &rest[open + 1..];
                }
            }
        }
        out.push_words(rest);
        if !q.ends_with(char::is_whitespace) && !q.ends_with('"') {
            // Search-as-you-type: treat the trailing bare word as a prefix.
            let last_word = rest.split_whitespace().last().unwrap_or("");
            if !last_word.starts_with(['#', '-']) && !last_word.is_empty() {
                let last_tok = tokens(last_word).last();
                if let Some(t) = last_tok
                    && out.terms.last() == Some(&t)
                {
                    out.terms.pop();
                    out.prefix = Some(t);
                }
            }
        }
        out
    }

    fn push_words(&mut self, s: &str) {
        for w in s.split_whitespace() {
            if let Some(tag) = w.strip_prefix('#').filter(|t| !t.is_empty()) {
                self.tags.push(tag.to_lowercase());
            } else if let Some(ex) = w.strip_prefix('-').filter(|t| !t.is_empty()) {
                self.exclude.extend(tokens(ex));
            } else {
                self.terms.extend(tokens(w));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.prefix.is_none() && self.phrases.is_empty() && self.tags.is_empty()
    }

    /// Every term to highlight in results.
    pub fn highlight_terms(&self) -> Vec<String> {
        let mut v = self.terms.clone();
        v.extend(self.prefix.clone());
        v
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub path: NotePath,
    pub title: String,
    pub score: f32,
    pub snippet: String,
    /// Byte ranges in `snippet` to emphasise.
    pub highlights: Vec<std::ops::Range<usize>>,
}

/// Weighted term frequencies and length for BM25, computed when a note is
/// parsed (outside the index lock) so inserting is cheap.
pub(crate) fn search_terms(plain: &str, name: &str, title: &str, aliases: &[String], tags: &[String]) -> (Vec<(String, f32)>, f32) {
    note_terms(plain, name, title, aliases, tags, None)
}

fn bump(tf: &mut HashMap<String, f32, FastHash>, t: String, by: f32) {
    match tf.get_mut(t.as_str()) {
        Some(v) => *v += by,
        None => {
            tf.insert(t, by);
        }
    }
}

fn count(terms: &mut HashMap<String, u32, FastHash>, t: &str) {
    match terms.get_mut(t) {
        Some(n) => *n += 1,
        None => {
            terms.insert(t.to_string(), 1);
        }
    }
}

/// [`search_terms`], and with `content` also the note's content terms
/// ([`crate::tokenize::content_tokens`] of `plain` and `name`, counted), in
/// one pass over the words: indexing used to segment, fold and stem the
/// text twice.
pub(crate) fn note_terms(
    plain: &str,
    name: &str,
    title: &str,
    aliases: &[String],
    tags: &[String],
    mut content: Option<&mut HashMap<String, u32, FastHash>>,
) -> (Vec<(String, f32)>, f32) {
    let mut tf: HashMap<String, f32, FastHash> = HashMap::with_capacity_and_hasher(plain.len() / 16, FastHash);
    let mut len = 0f32;
    // One buffer for every word: most words are already in the maps, so
    // most need no allocation at all.
    let mut buf = String::new();
    for w in plain.unicode_words() {
        fold_into(w, &mut buf);
        let content_word = content.is_some() && is_content(&buf);
        stem_in_place(&mut buf);
        if content_word && let Some(terms) = content.as_deref_mut() {
            count(terms, &buf);
        }
        match tf.get_mut(buf.as_str()) {
            Some(v) => *v += 1.0,
            None => {
                tf.insert(buf.clone(), 1.0);
            }
        }
        len += 1.0;
    }
    for w in name.unicode_words() {
        let folded = fold_str(w);
        let content_word = content.is_some() && is_content(&folded);
        let t = stem_owned(folded);
        if content_word && let Some(terms) = content.as_deref_mut() {
            count(terms, &t);
        }
        bump(&mut tf, t, TITLE_WEIGHT);
        len += 1.0;
    }
    for t in tokens(title).chain(aliases.iter().flat_map(|a| tokens(a))) {
        bump(&mut tf, t, TITLE_WEIGHT);
        len += 1.0;
    }
    for tag in tags {
        for t in tokens(tag) {
            bump(&mut tf, t, TAG_WEIGHT);
        }
    }
    (tf.into_iter().collect(), len)
}

/// A document of a keyword index (a note, or an attachment page).
///
/// Stable: a note keeps its id across updates. A removed note's id is given
/// again only after [`FullText::end_batch`], so everyone who saw the batch
/// that removed it saw the removal first (`docs/design/0.14-index-startup.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub u32);

impl DocId {
    /// The id as an index into a table by document.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// One term's documents and their weighted term frequency.
type Postings = HashMap<DocId, f32, FastHash>;

/// Inverted index with BM25 ranking over interned terms and dense, stable
/// document ids. IDF, N and the average length are computed when a query
/// runs, so inserting and removing a document is exact.
#[derive(Debug, Default)]
pub struct FullText {
    terms: Terms,
    /// By [`TermId`]; empty for a term no document uses any more.
    postings: Vec<Postings>,
    /// By [`DocId`]: each document's terms (to remove it, and for the agent
    /// views to add or subtract it); empty for a free id.
    doc_terms: Vec<Vec<TermId>>,
    /// By [`DocId`]: each document's length; 0 for a free id.
    doc_len: Vec<f32>,
    ids: HashMap<NotePath, DocId>,
    /// By [`DocId`]; `None` for a free id.
    paths: Vec<Option<NotePath>>,
    /// Ids that may be given again.
    free: Vec<DocId>,
    /// Ids removed in the batch being applied: free after [`Self::end_batch`].
    released: Vec<DocId>,
    total_len: f64,
}

impl FullText {
    pub(crate) fn upsert(&mut self, rec: &NoteRecord) {
        self.insert(&rec.path, &rec.search_terms.0, rec.search_terms.1);
    }

    /// Index a document that isn't a note (an attachment page).
    pub(crate) fn upsert_terms(&mut self, path: &NotePath, tf: &[(String, f32)], len: f32) {
        self.insert(path, tf, len);
    }

    /// Inverse document frequency of a term (0 when no document has it).
    pub(crate) fn term_idf(&self, term: &str) -> f32 {
        self.live(term).map_or(0.0, |p| self.idf(p.len()))
    }

    pub(crate) fn has_term(&self, term: &str) -> bool {
        self.live(term).is_some()
    }

    /// The posting list of `term` when some document uses it.
    fn live(&self, term: &str) -> Option<&Postings> {
        self.terms.get(term).map(|id| &self.postings[id.index()]).filter(|p| !p.is_empty())
    }

    /// The id of a term any document ever used in this index.
    pub fn term_id(&self, term: &str) -> Option<TermId> {
        self.terms.get(term)
    }

    /// The text of a term id this index gave.
    pub fn term(&self, id: TermId) -> &str {
        self.terms.name(id)
    }

    /// Intern `term` (for a record's content terms, which are always among
    /// its own search terms).
    pub(crate) fn intern(&mut self, term: &str) -> TermId {
        let id = self.terms.intern(term);
        if self.postings.len() < self.terms.len() {
            self.postings.resize_with(self.terms.len(), Postings::default);
        }
        id
    }

    /// The document id of `path`.
    pub fn doc_id(&self, path: &NotePath) -> Option<DocId> {
        self.ids.get(path).copied()
    }

    /// The path of a document id in use.
    pub fn path_of(&self, id: DocId) -> Option<&NotePath> {
        self.paths.get(id.index()).and_then(Option::as_ref)
    }

    /// A document's terms (empty for an id not in use), in the order its
    /// terms were given.
    pub fn doc_terms(&self, id: DocId) -> &[TermId] {
        self.doc_terms.get(id.index()).map_or(&[], Vec::as_slice)
    }

    /// A document's length for BM25 (`None` for an id not in use).
    pub fn doc_len(&self, id: DocId) -> Option<f32> {
        self.path_of(id).and_then(|_| self.doc_len.get(id.index()).copied())
    }

    /// The weighted frequency of a term in a document, if it has the term.
    pub fn weight(&self, term: TermId, doc: DocId) -> Option<f32> {
        self.postings.get(term.index()).and_then(|p| p.get(&doc).copied())
    }

    /// How many documents use `term`.
    pub fn doc_freq(&self, term: TermId) -> usize {
        self.postings.get(term.index()).map_or(0, HashMap::len)
    }

    /// How many documents are indexed.
    pub fn doc_count(&self) -> usize {
        self.ids.len()
    }

    /// One past the largest document id given: tables by [`DocId`] have
    /// this many slots.
    pub fn doc_slots(&self) -> usize {
        self.paths.len()
    }

    /// The sum of every document's length.
    pub const fn total_len(&self) -> f64 {
        self.total_len
    }

    /// The batch being applied is done: ids it removed may be given again.
    pub fn end_batch(&mut self) {
        self.free.append(&mut self.released);
    }

    /// Recompute the total length exactly (float drift from adding and
    /// subtracting never outlives a load).
    pub(crate) fn recount_len(&mut self) {
        // In id order, so the sum is the same on every run.
        self.total_len = self.paths.iter().zip(&self.doc_len).filter(|(p, _)| p.is_some()).map(|(_, l)| f64::from(*l)).sum();
    }

    fn insert(&mut self, path: &NotePath, tf: &[(String, f32)], len: f32) {
        let ids: Vec<(TermId, f32)> = tf.iter().map(|(t, f)| (self.intern(t), *f)).collect();
        self.upsert_ids(path, &ids, len);
    }

    /// Index a document whose terms are already interned here (a record
    /// from the startup cache).
    pub(crate) fn upsert_ids(&mut self, path: &NotePath, tf: &[(TermId, f32)], len: f32) {
        let id = if let Some(&id) = self.ids.get(path) {
            self.unlink(id);
            id
        } else {
            let id = self.free.pop().unwrap_or_else(|| {
                let id = DocId(u32::try_from(self.paths.len()).expect("fewer than 2^32 documents"));
                self.paths.push(None);
                self.doc_terms.push(Vec::new());
                self.doc_len.push(0.0);
                id
            });
            self.ids.insert(path.clone(), id);
            self.paths[id.index()] = Some(path.clone());
            id
        };
        let mut terms = Vec::with_capacity(tf.len());
        for &(tid, f) in tf {
            self.postings[tid.index()].insert(id, f);
            terms.push(tid);
        }
        self.doc_terms[id.index()] = terms;
        self.doc_len[id.index()] = len.max(1.0);
        self.total_len += f64::from(len.max(1.0));
    }

    /// Take a document's terms out of the postings and its length out of the
    /// total; its id stays its own.
    fn unlink(&mut self, id: DocId) {
        let i = id.index();
        self.total_len -= f64::from(std::mem::take(&mut self.doc_len[i]));
        for t in std::mem::take(&mut self.doc_terms[i]) {
            let p = &mut self.postings[t.index()];
            p.remove(&id);
            if p.is_empty() {
                // Give the memory back; the id stays until the next load.
                *p = Postings::default();
            }
        }
    }

    pub(crate) fn remove(&mut self, path: &NotePath) {
        let Some(id) = self.ids.remove(path) else {
            return;
        };
        self.unlink(id);
        self.paths[id.index()] = None;
        self.released.push(id);
    }

    fn idf(&self, df: usize) -> f32 {
        idf_n(self.ids.len(), df)
    }

    /// The statistics a ranking uses: a view's, unless it holds every
    /// document (then the index's own, which are the same numbers).
    fn stats<'a>(&self, stats: Option<&'a DocStats>) -> Option<&'a DocStats> {
        stats.filter(|s| s.n() != self.ids.len())
    }

    /// How many documents of `stats` (all without one) use the posting list's term.
    fn doc_freq_in(postings: &Postings, stats: Option<&DocStats>) -> usize {
        stats.map_or(postings.len(), |s| postings.keys().filter(|id| s.contains(**id)).count())
    }

    /// The inverse document frequency of `term` among the documents of
    /// `stats` (all without one); `None` when none of them uses it.
    pub(crate) fn term_idf_in(&self, term: &str, stats: Option<&DocStats>) -> Option<f32> {
        let stats = self.stats(stats);
        let n = stats.map_or(self.ids.len(), DocStats::n);
        let df = Self::doc_freq_in(self.live(term)?, stats);
        (df > 0).then(|| idf_n(n, df))
    }

    /// One scan of a posting list: the members' (id, weighted tf) into
    /// `buf`. Their count is the term's document frequency among them.
    fn members_of(postings: &Postings, stats: Option<&DocStats>, buf: &mut Vec<(DocId, f32)>) {
        buf.clear();
        #[expect(clippy::iter_over_hash_type, reason = "each id gets one addition per term, so the order between ids changes no sum")]
        for (&id, &tf) in postings {
            if stats.is_none_or(|s| s.contains(id)) {
                buf.push((id, tf));
            }
        }
    }

    /// BM25 of one term for the documents of `buf`, added to `acc` (times
    /// `factor` for a prefix completion) in `group`.
    fn add_term(&self, buf: &[(DocId, f32)], acc: &mut Acc, avg: f32, idf: f32, factor: Option<f32>, group: u32) {
        for &(id, tf) in buf {
            let dl = self.doc_len.get(id.index()).copied().unwrap_or(1.0);
            let s = idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avg));
            acc.add(id, factor.map_or(s, |f| s * f), group);
        }
    }

    /// Ranked (document, score) for every term matching (AND semantics), in
    /// no particular order, over every document (`stats` `None`) or over the
    /// documents of `stats` only, with their own statistics: document count,
    /// average length and each term's document frequency counted among them
    /// (`docs/design/0.14-agent-index.md` §3.3, plan A). A term none of them
    /// uses counts as absent, and prefix completions are the terms some of
    /// them use. The result is what an index holding only those documents
    /// gives, bit for bit. Pick the best with [`best`] and [`Self::by_rank`].
    ///
    /// One scan of each posting list, scores in a table by [`DocId`]
    /// (Fable's review, S5).
    pub(crate) fn rank_in(&self, q: &Query, stats: Option<&DocStats>) -> Vec<(DocId, f32)> {
        let stats = self.stats(stats);
        let (n, total) = stats.map_or((self.ids.len(), self.total_len), |s| (s.n(), s.total_len()));
        if n == 0 {
            return Vec::new();
        }
        let member = |id: DocId| stats.is_none_or(|s| s.contains(id));
        let avg = (total / n as f64) as f32;
        let mut acc = Acc::new(self.paths.len());
        let mut buf = Vec::new();
        let mut groups = 0u32;
        for term in &q.terms {
            let Some(p) = self.live(term) else { return Vec::new() };
            Self::members_of(p, stats, &mut buf);
            if buf.is_empty() {
                return Vec::new();
            }
            groups += 1;
            self.add_term(&buf, &mut acc, avg, idf_n(n, buf.len()), None, groups);
        }
        if let Some(prefix) = &q.prefix {
            groups += 1;
            let mut taken = 0;
            for (term, id) in self.terms.with_prefix(prefix) {
                if taken == 64 {
                    break;
                }
                Self::members_of(&self.postings[id.index()], stats, &mut buf);
                if buf.is_empty() {
                    continue;
                }
                taken += 1;
                // Exact matches score fully, longer completions a little less.
                let factor = if term == prefix.as_str() { 1.0 } else { 0.8 };
                self.add_term(&buf, &mut acc, avg, idf_n(n, buf.len()), Some(factor), groups);
            }
        }
        let mut out: Vec<(DocId, f32)> = if groups > 0 {
            acc.touched.iter().filter(|id| acc.count[id.index()] == groups).map(|id| (*id, acc.score[id.index()])).collect()
        } else {
            // Tag-only queries: every doc is a candidate; filters narrow it down.
            (0..self.paths.len())
                .filter_map(|i| u32::try_from(i).ok().map(DocId))
                .filter(|id| self.paths[id.index()].is_some() && member(*id))
                .map(|id| (id, 0.0))
                .collect()
        };
        for term in &q.exclude {
            if let Some(p) = self.live(term) {
                out.retain(|(id, _)| !p.contains_key(id));
            }
        }
        out
    }

    /// The order of ranked documents: score, best first, then path.
    pub(crate) fn by_rank(&self, a: &(DocId, f32), b: &(DocId, f32)) -> std::cmp::Ordering {
        b.1.total_cmp(&a.1).then_with(|| self.path_of(a.0).cmp(&self.path_of(b.0)))
    }
}

/// Scores of one ranking, by [`DocId`], and in how many of the query's
/// groups (its words, and its prefix) each document was found.
struct Acc {
    score: Vec<f32>,
    count: Vec<u32>,
    /// The last group a document was counted in.
    mark: Vec<u32>,
    touched: Vec<DocId>,
}

impl Acc {
    fn new(slots: usize) -> Self {
        Self { score: vec![0.0; slots], count: vec![0; slots], mark: vec![0; slots], touched: Vec::new() }
    }

    fn add(&mut self, id: DocId, s: f32, group: u32) {
        let i = id.index();
        self.score[i] += s;
        if self.mark[i] != group {
            if self.mark[i] == 0 {
                self.touched.push(id);
            }
            self.mark[i] = group;
            self.count[i] += 1;
        }
    }
}

/// The first `k` items of `v` in `order` that `keep` accepts: what sorting
/// all of them, filtering and taking `k` gives, without sorting all of them
/// (a partial selection, then more only while `keep` turns some down).
pub(crate) fn best<T: Copy>(
    mut v: Vec<T>,
    k: usize,
    order: impl Fn(&T, &T) -> std::cmp::Ordering,
    mut keep: impl FnMut(&T) -> bool,
) -> Vec<T> {
    let mut out = Vec::with_capacity(k.min(v.len()));
    let mut rest = v.as_mut_slice();
    while out.len() < k && !rest.is_empty() {
        let m = ((k - out.len()) * 2).max(16).min(rest.len());
        if m < rest.len() {
            rest.select_nth_unstable_by(m - 1, &order);
        }
        let (head, tail) = rest.split_at_mut(m);
        head.sort_by(&order);
        for x in head.iter() {
            if out.len() == k {
                break;
            }
            if keep(x) {
                out.push(*x);
            }
        }
        rest = tail;
    }
    out
}

/// BM25's inverse document frequency of a term `df` of `n` documents use.
fn idf_n(n: usize, df: usize) -> f32 {
    let n = n as f32;
    ((n - df as f32 + 0.5) / (df as f32 + 0.5)).ln_1p()
}

/// Build a short snippet around the first matching term. Terms are folded
/// (`tokens`), so "cafe" highlights "Café"; ranges are exact in the snippet.
pub(crate) fn snippet(plain: &str, terms: &[String], width: usize) -> (String, Vec<std::ops::Range<usize>>) {
    let folded = Folded::new(plain);
    let mut found: Vec<std::ops::Range<usize>> = terms.iter().flat_map(|t| folded.find_all(t)).collect();
    found.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    let center = found.first().map_or(0, |r| r.start);
    let floor = |mut i: usize| {
        while !plain.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let start = floor(center.saturating_sub(width / 3));
    let end = floor((start + width).min(plain.len()));
    let mut text = String::with_capacity(end - start + 8);
    let offset = if start > 0 {
        text.push('…');
        '…'.len_utf8()
    } else {
        0
    };
    // Newlines become spaces: same length, so ranges stay put.
    text.push_str(&plain[start..end].replace('\n', " "));
    if end < plain.len() {
        text.push('…');
    }
    let mut hl: Vec<std::ops::Range<usize>> = Vec::new();
    for r in found {
        if r.start < start || r.end > end || hl.last().is_some_and(|l| r.start < l.end) {
            continue;
        }
        hl.push(offset + r.start - start..offset + r.end - start);
    }
    (text, hl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_queries() {
        let q = Query::parse(r#"rope "undo tree" #rust -draft edi"#);
        assert_eq!(q.terms, vec!["rope", "undo", "tree"]);
        assert_eq!(q.phrases, vec!["undo tree"]);
        assert_eq!(q.tags, vec!["rust"]);
        assert_eq!(q.exclude, vec!["draft"]);
        assert_eq!(q.prefix.as_deref(), Some("edi"));
        assert_eq!(Query::parse("rope ").prefix, None);
    }

    #[test]
    fn snippets_fold_accents() {
        let (s, hl) = snippet("Notes from the café in Łódź about Straße names.", &["cafe".into(), "lodz".into(), "strasse".into()], 200);
        let words: Vec<&str> = hl.iter().map(|r| &s[r.clone()]).collect();
        assert_eq!(words, ["café", "Łódź", "Straße"]);
        let long = format!("{} Ångström {}", "x ".repeat(200), "y ".repeat(200));
        let (s, hl) = snippet(&long, &["angstrom".into()], 60);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(&s[hl[0].clone()], "Ångström");
    }

    #[test]
    fn snippet_highlights() {
        let (s, hl) = snippet("A long intro. Then the rope appears here, rope again.", &["rope".into()], 30);
        assert!(s.contains("rope"));
        for r in &hl {
            assert_eq!(&s[r.clone()], "rope");
        }
    }

    /// `best` (Fable's review, S5) gives what a full sort, a filter and
    /// `take(k)` give, for every `k` and many ties.
    #[test]
    fn best_is_sort_filter_take() {
        let mut x = 0x9e37_79b9_u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for round in 0..200 {
            let len = usize::try_from(next(300)).unwrap_or(0);
            let v: Vec<(u32, f32)> = (0..len).map(|i| (u32::try_from(i).unwrap_or(0), next(20) as f32)).collect();
            let order = |a: &(u32, f32), b: &(u32, f32)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
            let keep = |e: &(u32, f32)| (e.0 + round) % 3 != 0;
            for k in [0, 1, 5, 20, 64, 400] {
                let mut all = v.clone();
                all.sort_by(order);
                let want: Vec<_> = all.into_iter().filter(|e| keep(e)).take(k).collect();
                assert_eq!(best(v.clone(), k, order, keep), want, "round {round} k {k}");
            }
        }
    }
}
