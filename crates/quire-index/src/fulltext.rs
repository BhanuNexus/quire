// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Inverted index with BM25 ranking, prefix search-as-you-type and snippets.

use std::collections::HashMap;

use quire_vault::NotePath;
use unicode_segmentation::UnicodeSegmentation;

use crate::record::NoteRecord;
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
        for (t, f) in tf {
            let tid = self.intern(t);
            self.postings[tid.index()].insert(id, *f);
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
        let n = self.ids.len() as f32;
        ((n - df as f32 + 0.5) / (df as f32 + 0.5)).ln_1p()
    }

    fn score_term(&self, postings: &Postings, scores: &mut HashMap<DocId, f32>, avg: f32) {
        let idf = self.idf(postings.len());
        #[expect(clippy::iter_over_hash_type, reason = "each id gets one addition per term, so the order between ids changes no sum")]
        for (&id, &tf) in postings {
            let dl = self.doc_len.get(id.index()).copied().unwrap_or(1.0);
            let s = idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avg));
            *scores.entry(id).or_default() += s;
        }
    }

    /// Ranked (path, score) for every term matching (AND semantics).
    pub(crate) fn rank(&self, q: &Query) -> Vec<(NotePath, f32)> {
        if self.ids.is_empty() {
            return Vec::new();
        }
        let avg = (self.total_len / self.ids.len() as f64) as f32;
        let mut scores: HashMap<DocId, f32> = HashMap::new();
        let mut required: Option<std::collections::HashSet<DocId>> = None;
        let mut intersect = |ids: std::collections::HashSet<DocId>| {
            required = Some(match required.take() {
                None => ids,
                Some(r) => r.intersection(&ids).copied().collect(),
            });
        };
        for term in &q.terms {
            match self.live(term) {
                Some(p) => {
                    self.score_term(p, &mut scores, avg);
                    intersect(p.keys().copied().collect());
                }
                None => return Vec::new(),
            }
        }
        if let Some(prefix) = &q.prefix {
            let mut any = std::collections::HashSet::new();
            let live = self.terms.with_prefix(prefix).map(|(t, id)| (t, &self.postings[id.index()])).filter(|(_, p)| !p.is_empty());
            for (term, p) in live.take(64) {
                // Exact matches score fully, longer completions a little less.
                let mut partial = HashMap::new();
                self.score_term(p, &mut partial, avg);
                let factor = if term == prefix.as_str() { 1.0 } else { 0.8 };
                #[expect(
                    clippy::iter_over_hash_type,
                    reason = "each id gets one addition per term, so the order between ids changes no sum"
                )]
                for (id, s) in partial {
                    *scores.entry(id).or_default() += s * factor;
                    any.insert(id);
                }
            }
            intersect(any);
        }
        let mut out: Vec<(NotePath, f32)> = match required {
            Some(ids) => {
                ids.into_iter().filter_map(|id| Some((self.path_of(id)?.clone(), scores.get(&id).copied().unwrap_or(0.0)))).collect()
            }
            // Tag-only queries: every doc is a candidate; filters narrow it down.
            None => self.paths.iter().flatten().map(|p| (p.clone(), 0.0)).collect(),
        };
        for term in &q.exclude {
            if let Some(p) = self.live(term) {
                out.retain(|(path, _)| self.ids.get(path).is_none_or(|id| !p.contains_key(id)));
            }
        }
        out
    }
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
}
