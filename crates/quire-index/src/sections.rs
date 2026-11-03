// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Section-level search for agents and the panel (design §C.1).
//!
//! A hit is a *part* of a note (a heading's text up to the next heading,
//! split when long; [`crate::chunk::parts`]) with its line range, so a
//! caller can read just that part. Keyword hits rank the parts of the
//! notes BM25 finds by how much of the query each part covers; meaning hits
//! are the semantic index's passages, which share the same boundaries. The
//! two lists are fused by reciprocal rank, as note search does. Attachment
//! pages (PDF text, OCR) are a third list when asked for.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use quire_vault::NotePath;

use crate::actor::IndexService;
use crate::chunk::parts;
use crate::fulltext::{Query, snippet};
use crate::index::Index;
use crate::semantic::SemanticIndex;
use crate::tokenize::{fold_str, tokens};

/// RRF constant, as in note search.
const RRF_K: f32 = 60.0;
/// Minimum cosine for a passage found by meaning alone.
pub(crate) const MEANING_THRESHOLD: f32 = 0.2;
/// Notes whose parts are ranked for keywords, per hit asked for (at least 20).
const KEYWORD_NOTES_PER_HIT: usize = 3;
/// BM25's term-frequency saturation, used for parts too.
const K1: f32 = 1.2;

/// Which documents to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchKind {
    #[default]
    Notes,
    Attachments,
    All,
}

/// Narrow a search. Empty fields don't filter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchFilters {
    /// Folder prefix, e.g. `Projects/` (case-insensitive).
    pub folder: Option<String>,
    /// Every tag must be present (nested tags match their parent).
    pub tags: Vec<String>,
    /// A `quire-view` condition: `status != done and author.name = Ada`.
    pub where_: Option<String>,
    /// Unix ms bounds on the note's last change.
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub kind: SearchKind,
}

impl SearchFilters {
    /// Whether `where_` reads; the message says what's wrong.
    pub fn validate(&self) -> Result<(), String> {
        match &self.where_ {
            Some(w) if !w.trim().is_empty() => crate::views::parse(&format!("where: {w}")).map(drop),
            _ => Ok(()),
        }
    }

    /// Filters that only notes have (attachments can't match them).
    const fn notes_only(&self) -> bool {
        !self.tags.is_empty() || self.where_.is_some() || self.modified_after.is_some() || self.modified_before.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionHit {
    pub path: NotePath,
    /// The part's heading text, `None` before the first heading. For an
    /// attachment, "Page N".
    pub heading: Option<String>,
    /// 1-based inclusive line range of the part. For an attachment, the page twice.
    pub lines: (u32, u32),
    pub snippet: String,
    pub score: f32,
    pub by_meaning: bool,
    /// 1-based page of an attachment hit (`None` for notes).
    pub page: Option<u32>,
}

impl SectionHit {
    /// Where the hit is, for people: "paper.pdf p. 3" for a page of an
    /// attachment, the note's path otherwise.
    pub fn place(&self) -> String {
        match self.page {
            Some(p) => format!("{} p. {p}", self.path.file_name()),
            None => self.path.to_string(),
        }
    }
}

/// A candidate part while fusing.
struct Cand {
    path: NotePath,
    heading: Option<String>,
    range: Range<usize>,
    score: f32,
    keyword: bool,
}

fn folder_ok(path: &NotePath, folder: Option<&str>) -> bool {
    folder.is_none_or(|f| {
        let p = path.as_str().to_lowercase();
        p.strip_prefix(f).is_some_and(|rest| rest.starts_with('/'))
    })
}

/// `range` without trailing blank lines and spaces.
fn trimmed(src: &str, range: &Range<usize>) -> Range<usize> {
    let body = &src[range.clone()];
    let keep = body.trim_end().len();
    range.start..range.start + keep
}

impl Index {
    /// Notes that pass the filters (`None`: every note does).
    fn allowed(&self, f: &SearchFilters) -> Option<HashSet<NotePath>> {
        let folder = f.folder.as_deref().map(|x| x.trim_matches('/').to_lowercase()).filter(|x| !x.is_empty());
        let where_rows: Option<HashSet<NotePath>> = match f.where_.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
            None => None,
            Some(w) => {
                let Ok(spec) = crate::views::parse(&format!("where: {w}")) else { return Some(HashSet::new()) };
                let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                Some(crate::views::run(&spec, self, &today, None).rows.into_iter().map(|r| r.path).collect())
            }
        };
        if folder.is_none() && f.tags.is_empty() && where_rows.is_none() && f.modified_after.is_none() && f.modified_before.is_none() {
            return None;
        }
        Some(
            self.records()
                .filter(|r| folder_ok(&r.path, folder.as_deref()))
                .filter(|r| f.tags.iter().all(|t| r.has_tag(t)))
                .filter(|r| where_rows.as_ref().is_none_or(|w| w.contains(&r.path)))
                .filter(|r| {
                    let m = self.modified(&r.path).unwrap_or(0);
                    f.modified_after.is_none_or(|a| m >= a) && f.modified_before.is_none_or(|b| m <= b)
                })
                .map(|r| r.path.clone())
                .collect(),
        )
    }

    /// Parts of notes matching `query`'s keywords, best first.
    fn keyword_parts(&self, q: &Query, allowed: Option<&HashSet<NotePath>>, limit: usize) -> Vec<Cand> {
        let mut notes: Vec<(NotePath, f32)> = self
            .fulltext
            .rank(q)
            .into_iter()
            .filter(|(p, _)| allowed.is_none_or(|a| a.contains(p)))
            .filter(|(p, _)| {
                let Some(rec) = self.get(p) else { return false };
                if !q.tags.iter().all(|t| rec.has_tag(t)) {
                    return false;
                }
                q.phrases.is_empty() || {
                    let folded = fold_str(&rec.plain);
                    q.phrases.iter().all(|ph| folded.contains(ph.as_str()))
                }
            })
            .collect();
        notes.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        notes.truncate((limit * KEYWORD_NOTES_PER_HIT).max(20));
        let top = notes.first().map_or(1.0, |n| n.1.max(1e-6));
        let weights: Vec<(&str, f32)> = q.terms.iter().map(|t| (t.as_str(), self.fulltext.term_idf(t).max(0.1))).collect();
        let prefix = q.prefix.as_deref().map(|p| (p, self.fulltext.term_idf(p).max(0.5)));
        let total: f32 = weights.iter().map(|w| w.1).sum::<f32>() + prefix.map_or(0.0, |p| p.1);
        let mut out = Vec::new();
        for (path, note_score) in notes {
            let Some(src) = self.source(&path) else { continue };
            let prior = note_score / top;
            let mut best_empty: Option<Cand> = None;
            let mut any = false;
            for part in parts(src) {
                let range = trimmed(src, &part.range);
                if range.is_empty() {
                    continue;
                }
                let mut tf: HashMap<String, f32> = HashMap::new();
                for t in tokens(part.heading.as_deref().unwrap_or_default()).chain(tokens(&src[range.clone()])) {
                    *tf.entry(t).or_default() += 1.0;
                }
                let sat = |n: f32| n / (n + K1);
                let mut covered: f32 = weights.iter().map(|(t, w)| w * sat(tf.get(*t).copied().unwrap_or(0.0))).sum();
                if let Some((p, w)) = prefix {
                    let n: f32 = tf.iter().filter(|(t, _)| t.starts_with(p)).map(|(_, n)| n).sum();
                    covered += w * sat(n);
                }
                let coverage = if total > 0.0 { covered / total } else { 0.0 };
                let cand = Cand { path: path.clone(), heading: part.heading, range, score: prior * (0.3 + coverage), keyword: true };
                if coverage > 0.0 {
                    any = true;
                    out.push(cand);
                } else if best_empty.is_none() {
                    best_empty = Some(cand);
                }
            }
            // A note that matches only by its title or tags: its first part.
            if !any && let Some(c) = best_empty {
                out.push(c);
            }
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then(a.range.start.cmp(&b.range.start)));
        out
    }

    /// Search by section (design §C.1): keyword parts and meaning passages
    /// fused, then attachment pages when `filters.kind` asks for them.
    pub fn search_sections(&self, semantic: Option<&SemanticIndex>, query: &str, filters: &SearchFilters, limit: usize) -> Vec<SectionHit> {
        let q = Query::parse(query);
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let allowed = self.allowed(filters);
        let mut fused: HashMap<(NotePath, usize), Cand> = HashMap::new();
        if filters.kind != SearchKind::Attachments {
            for (rank, c) in self.keyword_parts(&q, allowed.as_ref(), limit).into_iter().enumerate() {
                let score = 1.0 / (RRF_K + rank as f32 + 1.0);
                fused.insert((c.path.clone(), c.range.start), Cand { score, ..c });
            }
            // Phrases ask for exact wording: no meaning hits.
            if q.phrases.is_empty()
                && let Some(sem) = semantic
            {
                self.meaning_parts(sem, &q, allowed.as_ref(), &mut fused);
            }
        }
        let mut hits: Vec<Cand> = fused.into_values().collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then(a.range.start.cmp(&b.range.start)));
        hits.truncate(limit);
        let terms = q.highlight_terms();
        let mut out: Vec<SectionHit> = hits
            .into_iter()
            .filter_map(|c| {
                let src = self.source(&c.path)?;
                let plain = quire_markdown::extract(&src[c.range.clone()]).plain;
                let (snip, _) = snippet(plain.trim(), &terms, 160);
                Some(SectionHit {
                    lines: self.lines_of(&c.path, &c.range),
                    heading: c.heading,
                    snippet: snip,
                    score: c.score,
                    by_meaning: !c.keyword,
                    page: None,
                    path: c.path,
                })
            })
            .collect();
        if filters.kind != SearchKind::Notes && !filters.notes_only() {
            out = self.merge_pages(out, &q, filters, limit);
        }
        out
    }

    /// Fuse meaning passages into `fused` (best passage per part).
    fn meaning_parts(
        &self,
        sem: &SemanticIndex,
        q: &Query,
        allowed: Option<&HashSet<NotePath>>,
        fused: &mut HashMap<(NotePath, usize), Cand>,
    ) {
        let text: String = q.terms.iter().chain(q.prefix.iter()).cloned().collect::<Vec<_>>().join(" ");
        let mut rank = 0usize;
        for (ci, sim) in sem.search(&text, 80) {
            let c = &sem.chunks[ci];
            let Some(rec) = self.get(&c.path) else { continue };
            let Some(src) = self.source(&c.path) else { continue };
            if allowed.is_some_and(|a| !a.contains(&c.path)) || !q.tags.iter().all(|t| rec.has_tag(t)) {
                continue;
            }
            let folded = fold_str(&c.text);
            if q.exclude.iter().any(|x| folded.contains(x.as_str())) {
                continue;
            }
            // The index may be a moment behind the note: skip passages past its end.
            if c.range.end > src.len() || !src.is_char_boundary(c.range.start) || !src.is_char_boundary(c.range.end) {
                continue;
            }
            let score = sim.clamp(0.0, 1.0) / (RRF_K + rank as f32 + 1.0);
            rank += 1;
            match fused.get_mut(&(c.path.clone(), c.range.start)) {
                Some(hit) => hit.score += score,
                None if sim >= MEANING_THRESHOLD => {
                    let range = trimmed(src, &c.range);
                    fused.insert(
                        (c.path.clone(), c.range.start),
                        Cand { path: c.path.clone(), heading: c.heading.clone(), range, score, keyword: false },
                    );
                }
                None => {}
            }
        }
    }

    /// Attachment pages matching `q`, fused with the note hits by rank.
    fn merge_pages(&self, notes: Vec<SectionHit>, q: &Query, filters: &SearchFilters, limit: usize) -> Vec<SectionHit> {
        let folder = filters.folder.as_deref().map(|x| x.trim_matches('/').to_lowercase()).filter(|x| !x.is_empty());
        let mut pages: Vec<(NotePath, usize, f32)> = self
            .attachment_text
            .rank(q)
            .into_iter()
            .filter_map(|(key, score)| {
                let (path, page) = key.as_str().rsplit_once("#page=")?;
                let page: usize = page.parse().ok()?;
                let path = NotePath::new(path);
                (score > 0.0 && folder_ok(&path, folder.as_deref())).then_some((path, page, score))
            })
            .collect();
        pages.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        pages.truncate(limit);
        let terms = q.highlight_terms();
        let page_hits = pages.into_iter().filter_map(|(path, page, _)| {
            let text = self.attachment(&path)?.pages.get(page - 1)?;
            let (snip, _) = snippet(text.trim(), &terms, 160);
            let n = u32::try_from(page).unwrap_or(u32::MAX);
            Some(SectionHit {
                path,
                heading: Some(format!("Page {n}")),
                lines: (n, n),
                snippet: snip,
                score: 0.0,
                by_meaning: false,
                page: Some(n),
            })
        });
        let mut all: Vec<SectionHit> = notes
            .into_iter()
            .enumerate()
            .chain(page_hits.enumerate())
            .map(|(rank, mut h)| {
                h.score = 1.0 / (RRF_K + rank as f32 + 1.0) + if h.page.is_some() { 0.0 } else { 1e-6 };
                h
            })
            .collect();
        all.sort_by(|a, b| b.score.total_cmp(&a.score));
        all.truncate(limit);
        all
    }
}

impl IndexService {
    /// Search, returning the matching section of each hit.
    pub fn search_sections(&self, query: &str, filters: &SearchFilters, limit: usize) -> Vec<SectionHit> {
        let sem = self.semantic();
        self.read().search_sections(sem.as_deref(), query, filters, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{AttachmentKind, AttachmentText, TextSource};

    fn svc(notes: &[(&str, &str)]) -> IndexService {
        let svc = IndexService::spawn();
        svc.load(quire_vault::Vault::in_memory(notes));
        svc.flush_semantic();
        svc
    }

    #[test]
    fn hits_are_the_matching_section_with_lines() {
        let s = svc(&[(
            "Rope Notes.md",
            "# Rope Notes\nIntro about data structures.\n\n## Undo\nRopes make undo cheap because subtrees are shared.\n\n## Lines\nEach node stores line counts.\n",
        )]);
        let hits = s.search_sections("undo subtrees", &SearchFilters::default(), 5);
        assert_eq!(hits[0].heading.as_deref(), Some("Undo"));
        assert_eq!(hits[0].lines, (4, 5));
        assert!(hits[0].snippet.contains("subtrees"), "{:?}", hits[0]);
        assert!(!hits[0].by_meaning);
        let hits = s.search_sections("line counts", &SearchFilters::default(), 5);
        assert_eq!((hits[0].heading.as_deref(), hits[0].lines), (Some("Lines"), (7, 8)));
        assert!(s.search_sections("", &SearchFilters::default(), 5).is_empty());
    }

    #[test]
    fn filters_by_folder_tag_where_and_time() {
        let s = svc(&[
            ("Projects/Rope.md", "---\nstatus: active\nauthor:\n  name: Ada\n---\n# Rope\nropes make undo cheap #cs\n"),
            ("Projects/Old.md", "---\nstatus: done\n---\n# Old\nropes from before\n"),
            ("Other/Rope two.md", "# Rope two\nropes everywhere\n"),
        ]);
        let paths = |f: &SearchFilters| -> Vec<String> {
            let mut v: Vec<String> = s.search_sections("ropes", f, 10).into_iter().map(|h| h.path.to_string()).collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(paths(&SearchFilters::default()).len(), 3);
        let f = SearchFilters { folder: Some("projects/".into()), ..Default::default() };
        assert_eq!(paths(&f), ["Projects/Old.md", "Projects/Rope.md"]);
        let f = SearchFilters { tags: vec!["cs".into()], ..Default::default() };
        assert_eq!(paths(&f), ["Projects/Rope.md"]);
        let f = SearchFilters { where_: Some("status != done".into()), ..Default::default() };
        assert_eq!(paths(&f), ["Other/Rope two.md", "Projects/Rope.md"]);
        let f = SearchFilters { where_: Some("author.name = Ada".into()), ..Default::default() };
        assert_eq!(paths(&f), ["Projects/Rope.md"]);
        let bad = SearchFilters { where_: Some("status ~~ x".into()), ..Default::default() };
        assert!(bad.validate().is_err());
        assert!(paths(&bad).is_empty());
        assert!(f.validate().is_ok());
        let p = NotePath::new("Projects/Old.md");
        s.handle().write().unwrap().set_modified(&p, 1_000);
        let f = SearchFilters { modified_before: Some(2_000), ..Default::default() };
        assert_eq!(paths(&f), ["Projects/Old.md"]);
        let f = SearchFilters { modified_after: Some(2_000), ..Default::default() };
        assert_eq!(paths(&f).len(), 2);
        let f = SearchFilters { kind: SearchKind::Attachments, ..Default::default() };
        assert!(paths(&f).is_empty());
    }

    #[test]
    fn attachment_pages_are_searched_when_asked() {
        let s = svc(&[("Notes.md", "# Notes\nropes are trees\n")]);
        s.upsert_attachment(AttachmentText {
            path: NotePath::new("papers/Ropes.pdf"),
            kind: AttachmentKind::Pdf,
            pages: vec!["Introduction to strings.".into(), "Ropes: an alternative to strings, by Boehm.".into()],
            source_hash: "ab".into(),
            by: TextSource::Embedded,
        });
        s.flush();
        let only = SearchFilters { kind: SearchKind::Attachments, ..Default::default() };
        let hits = s.search_sections("boehm", &only, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].path.as_str(), hits[0].page, hits[0].heading.as_deref()), ("papers/Ropes.pdf", Some(2), Some("Page 2")));
        assert!(s.search_sections("boehm", &SearchFilters::default(), 5).is_empty(), "notes only by default");
        let all = SearchFilters { kind: SearchKind::All, ..Default::default() };
        let hits = s.search_sections("ropes", &all, 5);
        assert_eq!(hits.len(), 3, "the note, page 2, and page 1 by the file's name");
        assert_eq!(hits[0].path.as_str(), "Notes.md", "notes win ties");
        assert!(hits.iter().any(|h| h.page == Some(2)));
        let tagged = SearchFilters { kind: SearchKind::All, tags: vec!["x".into()], ..Default::default() };
        assert!(s.search_sections("boehm", &tagged, 5).is_empty());
        s.remove_attachment(NotePath::new("papers/Ropes.pdf"));
        s.flush();
        assert!(s.search_sections("boehm", &only, 5).is_empty());
    }

    /// Timing on 2,000 notes (design §9: ≤ 15 ms in release). Run with
    /// `cargo test -p quire-index --release -- --ignored section_search_speed --nocapture`.
    #[test]
    #[ignore = "a benchmark"]
    fn section_search_speed() {
        let words = ["rope", "tree", "undo", "buffer", "editor", "café", "line", "node", "piece", "table", "gap", "cursor"];
        let notes: Vec<(String, String)> = (0..2_000)
            .map(|i| {
                let body = (0..12).fold(String::new(), |mut b, j| {
                    use std::fmt::Write as _;
                    let _ = write!(
                        b,
                        "## Part {j}\n{} {} {} and more text about {}.\n\n",
                        words[(i + j) % 12],
                        words[(i * 7 + j) % 12],
                        words[(i * 3 + j * 5) % 12],
                        words[j % 12]
                    );
                    b
                });
                (format!("Folder {}/Note {i}.md", i % 20), format!("# Note {i}\n{body}"))
            })
            .collect();
        let files: Vec<(&str, &str)> = notes.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        let s = svc(&files);
        let mut times = Vec::new();
        for q in ["rope undo", "cafe", "editor buffer", "gap cursor", "piece table", "line node"] {
            for _ in 0..5 {
                let t = std::time::Instant::now();
                let hits = s.search_sections(q, &SearchFilters::default(), 10);
                times.push(t.elapsed());
                assert!(!hits.is_empty());
            }
        }
        times.sort();
        eprintln!("section search, 2,000 notes: p50 {:?}, p95 {:?}", times[times.len() / 2], times[times.len() * 95 / 100]);
        let ix = s.read();
        let t = std::time::Instant::now();
        for q in ["rope undo", "cafe", "editor buffer", "gap cursor", "piece table", "line node"] {
            let _ = ix.search_sections(None, q, &SearchFilters::default(), 10);
        }
        eprintln!("keywords only: {:?} per query", t.elapsed() / 6);
        let sem = s.semantic().unwrap();
        let t = std::time::Instant::now();
        for q in ["rope undo", "cafe", "editor buffer", "gap cursor", "piece table", "line node"] {
            let _ = sem.search(q, 80);
        }
        eprintln!("semantic search alone ({} passages): {:?} per query", sem.len(), t.elapsed() / 6);
        let t = std::time::Instant::now();
        for q in ["rope undo", "cafe", "editor buffer", "gap cursor", "piece table", "line node"] {
            let _ = ix.search(q, 10);
        }
        eprintln!("note search (BM25): {:?} per query", t.elapsed() / 6);
    }

    #[test]
    fn meaning_hits_fill_in() {
        let s = svc(&[
            ("Cars.md", "# Cars\nThe car engine needs oil. A vehicle with a strong engine drives fast on the road."),
            ("Autos.md", "# Autos\nMy automobile is a vehicle with a loud engine; the road trip was long."),
            ("Bread.md", "# Bread\nSourdough bread needs flour, water and a slow rise in a warm oven."),
            ("Baking.md", "# Baking\nBaking cakes needs flour, sugar and an oven at the right heat."),
            ("Trucks.md", "# Trucks\nA truck is a heavy vehicle; its diesel engine hauls loads by road."),
        ]);
        let hits = s.search_sections("automobile", &SearchFilters::default(), 5);
        assert_eq!(hits[0].path.as_str(), "Autos.md");
        assert!(!hits[0].by_meaning);
        assert!(hits.iter().any(|h| h.by_meaning), "{hits:#?}");
        assert!(hits.iter().all(|h| h.path.as_str() != "Bread.md"));
    }
}
