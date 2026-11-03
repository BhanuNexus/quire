// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{BTreeMap, HashMap};

use quire_markdown::Task;
use quire_vault::NotePath;

use crate::attach::AttachmentText;
use crate::fulltext::{FullText, Query, SearchHit, snippet};
use crate::fuzzy::{QuickOpenHit, quick_open};
use crate::graph::{Backlink, LinkGraph};
use crate::record::NoteRecord;

/// The whole derived model of a vault. Synchronous and self-contained.
#[derive(Debug, Default)]
pub struct Index {
    records: HashMap<NotePath, NoteRecord>,
    sources: HashMap<NotePath, String>,
    pub(crate) fulltext: FullText,
    graph: LinkGraph,
    /// Byte offset of each line's start, per note (line numbers for hits).
    line_starts: HashMap<NotePath, Vec<u32>>,
    /// Last change, Unix ms: the file's time at load, the time of an update after.
    modified: HashMap<NotePath, i64>,
    /// Text of PDFs and images, by path.
    attachments: HashMap<NotePath, AttachmentText>,
    /// Keyword index of attachment pages (`path#page=N`).
    pub(crate) attachment_text: FullText,
}

/// Byte offsets where each line starts.
pub(crate) fn line_starts(src: &str) -> Vec<u32> {
    std::iter::once(0).chain(src.match_indices('\n').map(|(i, _)| u32::try_from(i + 1).unwrap_or(u32::MAX))).collect()
}

/// The key of an attachment page in the keyword index.
pub(crate) fn page_key(path: &NotePath, page: usize) -> NotePath {
    NotePath::new(format!("{path}#page={}", page + 1))
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a note and refresh the link graph.
    pub fn upsert(&mut self, path: NotePath, src: &str) {
        self.insert_record(NoteRecord::from_source(path, src), src.to_string());
        self.refresh();
    }

    /// Insert a pre-built record without refreshing the graph (bulk loads).
    pub fn insert_record(&mut self, rec: NoteRecord, src: String) {
        self.fulltext.upsert(&rec);
        self.line_starts.insert(rec.path.clone(), line_starts(&src));
        self.sources.insert(rec.path.clone(), src);
        self.records.insert(rec.path.clone(), rec);
    }

    pub fn remove(&mut self, path: &NotePath) {
        self.remove_no_refresh(path);
        self.refresh();
    }

    pub fn remove_no_refresh(&mut self, path: &NotePath) {
        self.records.remove(path);
        self.sources.remove(path);
        self.line_starts.remove(path);
        self.modified.remove(path);
        self.fulltext.remove(path);
    }

    /// When a note last changed (Unix ms), as far as the index knows.
    pub fn modified(&self, p: &NotePath) -> Option<i64> {
        self.modified.get(p).copied()
    }

    pub fn set_modified(&mut self, p: &NotePath, ms: i64) {
        self.modified.insert(p.clone(), ms);
    }

    /// The 1-based line holding byte `byte` of a note.
    pub fn line_of(&self, p: &NotePath, byte: usize) -> u32 {
        let Some(starts) = self.line_starts.get(p) else { return 1 };
        let b = u32::try_from(byte).unwrap_or(u32::MAX);
        u32::try_from(starts.partition_point(|s| *s <= b)).unwrap_or(u32::MAX).max(1)
    }

    /// The 1-based inclusive lines a byte range of a note covers (a range
    /// ending with its line's newline doesn't count the next line).
    pub fn lines_of(&self, p: &NotePath, range: &std::ops::Range<usize>) -> (u32, u32) {
        let first = self.line_of(p, range.start);
        let last_byte = if range.end > range.start { range.end - 1 } else { range.start };
        let last = self.line_of(p, last_byte);
        let ends_on_newline = self.source(p).is_some_and(|s| s.as_bytes().get(last_byte) == Some(&b'\n'));
        (first, if ends_on_newline && last > first { last - 1 } else { last }.max(first))
    }

    /// Rebuild the link graph (linear in the number of links).
    pub fn refresh(&mut self) {
        let sources = &self.sources;
        self.graph = LinkGraph::build(self.records.values(), &|p| sources.get(p).map(String::as_str));
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, p: &NotePath) -> Option<&NoteRecord> {
        self.records.get(p)
    }

    pub fn source(&self, p: &NotePath) -> Option<&str> {
        self.sources.get(p).map(String::as_str)
    }

    /// Store the text of an attachment (replaces older text for that path)
    /// and index its pages for search.
    pub fn upsert_attachment(&mut self, text: AttachmentText) {
        self.remove_attachment(&text.path);
        let name = text.path.stem().to_string();
        for (i, page) in text.pages.iter().enumerate() {
            // The file's name counts for its first page only, not every page.
            let title = if i == 0 { name.as_str() } else { "" };
            let (terms, len) = crate::fulltext::search_terms(page, title, title, &[], &[]);
            self.attachment_text.upsert_terms(&page_key(&text.path, i), &terms, len);
        }
        self.attachments.insert(text.path.clone(), text);
    }

    pub fn remove_attachment(&mut self, path: &NotePath) {
        if let Some(old) = self.attachments.remove(path) {
            for i in 0..old.pages.len() {
                self.attachment_text.remove(&page_key(path, i));
            }
        }
    }

    pub fn attachment(&self, path: &NotePath) -> Option<&AttachmentText> {
        self.attachments.get(path)
    }

    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentText> {
        self.attachments.values()
    }

    pub fn records(&self) -> impl Iterator<Item = &NoteRecord> + Clone {
        self.records.values()
    }

    /// Paths sorted for stable display.
    pub fn paths(&self) -> Vec<NotePath> {
        let mut v: Vec<_> = self.records.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn graph(&self) -> &LinkGraph {
        &self.graph
    }

    pub fn resolve(&self, target: &str, from: Option<&NotePath>) -> Option<NotePath> {
        self.graph.resolver.resolve(target, from)
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let q = Query::parse(query);
        if q.is_empty() {
            return Vec::new();
        }
        let terms = q.highlight_terms();
        let mut ranked: Vec<(NotePath, f32)> = self
            .fulltext
            .rank(&q)
            .into_iter()
            .filter(|(path, _)| {
                let Some(rec) = self.records.get(path) else { return false };
                if !q.tags.iter().all(|t| rec.has_tag(t)) {
                    return false;
                }
                q.phrases.is_empty() || {
                    let folded = crate::tokenize::fold_str(&rec.plain);
                    q.phrases.iter().all(|ph| folded.contains(ph.as_str()))
                }
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        // Snippets only for what is shown.
        ranked
            .into_iter()
            .filter_map(|(path, score)| {
                let rec = self.records.get(&path)?;
                let (snippet, highlights) = snippet(&rec.plain, &terms, 140);
                Some(SearchHit { path, title: rec.title.clone(), score, snippet, highlights })
            })
            .collect()
    }

    /// How rare a (folded, stemmed) word is in the vault's notes; `None`
    /// when no note uses it.
    pub fn idf(&self, term: &str) -> Option<f32> {
        let idf = self.fulltext.term_idf(term);
        self.fulltext.has_term(term).then_some(idf)
    }

    pub fn quick_open(&self, query: &str, limit: usize) -> Vec<QuickOpenHit> {
        quick_open(self.records.values(), query, limit)
    }

    pub fn backlinks(&self, p: &NotePath) -> &[Backlink] {
        self.graph.backlinks(p)
    }

    /// Every tag with its note count. Parents of nested tags (`a` for `a/b`)
    /// are included with the union count.
    pub fn tags(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, std::collections::HashSet<&NotePath>> = BTreeMap::new();
        for rec in self.records.values() {
            for tag in &rec.tags {
                let lower = tag.to_lowercase();
                let mut acc = String::new();
                for (i, part) in lower.split('/').enumerate() {
                    if i > 0 {
                        acc.push('/');
                    }
                    acc.push_str(part);
                    counts.entry(acc.clone()).or_default().insert(&rec.path);
                }
            }
        }
        counts.into_iter().map(|(k, v)| (k, v.len())).collect()
    }

    pub fn notes_with_tag(&self, tag: &str) -> Vec<NotePath> {
        let mut v: Vec<_> = self.records.values().filter(|r| r.has_tag(tag)).map(|r| r.path.clone()).collect();
        v.sort();
        v
    }

    /// Tasks across the vault, open ones first.
    pub fn tasks(&self, include_done: bool) -> Vec<(NotePath, Task)> {
        let mut v: Vec<_> = self
            .records
            .values()
            .flat_map(|r| r.tasks.iter().filter(|t| include_done || !t.done).map(|t| (r.path.clone(), t.clone())))
            .collect();
        v.sort_by(|a, b| a.1.done.cmp(&b.1.done).then_with(|| a.0.cmp(&b.0)).then(a.1.offset.cmp(&b.1.offset)));
        v
    }

    pub fn orphans(&self) -> Vec<NotePath> {
        let mut v: Vec<_> = self.records.keys().filter(|p| self.graph.is_orphan(p)).cloned().collect();
        v.sort();
        v
    }

    pub fn total_words(&self) -> usize {
        self.records.values().map(|r| r.stats.words).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut ix = Index::new();
        for (p, s) in [
            ("Rope Notes.md", "# Rope Notes\nA rope is a balanced tree of strings. Great for editors. #cs/data-structures\n"),
            ("Editors.md", "# Editors\nZed uses a rope. Helix too. See [[Rope Notes]] and [[Missing Note]].\n- [ ] try GPUI\n"),
            ("Cooking.md", "# Cooking\nA rope of dough? No: bread. #food\n- [x] bake\n"),
            ("Lonely.md", "nothing links here"),
        ] {
            ix.insert_record(NoteRecord::from_source(NotePath::new(p), s), s.to_string());
        }
        ix.refresh();
        ix
    }

    #[test]
    fn bm25_ranks_and_filters() {
        let ix = index();
        let hits = ix.search("rope", 10);
        assert_eq!(hits[0].path.as_str(), "Rope Notes.md");
        assert_eq!(hits.len(), 3);
        assert_eq!(ix.search("rope #food", 10).len(), 1);
        assert_eq!(ix.search("rope -bread", 10).len(), 2);
        assert_eq!(ix.search("\"balanced tree\"", 10).len(), 1);
        assert_eq!(ix.search("edit", 10)[0].path.as_str(), "Editors.md"); // prefix
        assert!(ix.search("rope zebra", 10).is_empty());
        assert!(ix.search("#cs", 10).iter().any(|h| h.path.as_str() == "Rope Notes.md"));
    }

    #[test]
    fn graph_queries() {
        let ix = index();
        let bl = ix.backlinks(&NotePath::new("Rope Notes.md"));
        assert_eq!(bl.len(), 1);
        let g = ix.graph();
        assert_eq!(g.neighbors(&NotePath::new("Rope Notes.md"), 1, crate::graph::Direction::In), [(NotePath::new("Editors.md"), 1)]);
        assert!(g.neighbors(&NotePath::new("Rope Notes.md"), 1, crate::graph::Direction::Out).is_empty());
        assert_eq!(g.neighbors(&NotePath::new("Editors.md"), 3, crate::graph::Direction::Both).len(), 1);
        assert_eq!(ix.line_of(&NotePath::new("Editors.md"), 0), 1);
        assert_eq!(ix.line_of(&NotePath::new("Editors.md"), 12), 2);
        assert_eq!(ix.lines_of(&NotePath::new("Editors.md"), &(0..10)), (1, 1));
        assert_eq!(ix.lines_of(&NotePath::new("Editors.md"), &(0..12)), (1, 2));
        assert_eq!(bl[0].from.as_str(), "Editors.md");
        assert!(bl[0].context.contains("Zed uses a rope"));
        assert_eq!(ix.graph().unresolved()[0].0, "Missing Note");
        assert_eq!(ix.orphans(), vec![NotePath::new("Cooking.md"), NotePath::new("Lonely.md")]);
        let tags = ix.tags();
        assert!(tags.contains(&("cs".into(), 1)) && tags.contains(&("cs/data-structures".into(), 1)));
        assert_eq!(ix.tasks(false).len(), 1);
    }

    #[test]
    fn quick_open_prefers_names() {
        let ix = index();
        let hits = ix.quick_open("rpnt", 5);
        assert_eq!(hits[0].path.as_str(), "Rope Notes.md");
        assert_eq!(ix.quick_open("", 10).len(), 4);
    }

    #[test]
    fn accents_are_optional_everywhere() {
        let mut ix = Index::new();
        for (p, s) in [
            ("Łódź.md", "# Łódź\nA city with a café on every corner. Straße-like streets.\n"),
            ("Trip.md", "# Trip\nWent to [[Lodz]] and met Zoë. See [[łódź]] too.\n"),
            ("Ångström.md", "---\naliases: [Angstrom unit]\n---\nTiny length.\n"),
            ("Units.md", "Measured in [[angstrom unit]] and [[ANGSTRÖM]].\n"),
        ] {
            ix.insert_record(NoteRecord::from_source(NotePath::new(p), s), s.to_string());
        }
        ix.refresh();
        assert_eq!(ix.search("cafe", 5)[0].path.as_str(), "Łódź.md");
        assert_eq!(ix.search("CAFÉ strasse", 5).len(), 1);
        assert_eq!(ix.search("zoe", 5)[0].path.as_str(), "Trip.md");
        assert_eq!(ix.search("\"cafe on every\"", 5).len(), 1);
        let hit = &ix.search("lodz", 5)[0];
        assert!(hit.highlights.iter().any(|r| &hit.snippet[r.clone()] == "Łódź"), "{hit:?}");
        let lodz = NotePath::new("Łódź.md");
        assert_eq!(ix.resolve("Lodz", None), Some(lodz.clone()));
        assert_eq!(ix.backlinks(&lodz).len(), 2);
        assert_eq!(ix.resolve("angstrom", None), Some(NotePath::new("Ångström.md")));
        assert_eq!(ix.backlinks(&NotePath::new("Ångström.md")).len(), 2);
        assert!(ix.graph().unresolved().is_empty());
        assert_eq!(ix.quick_open("lodz", 3)[0].path, lodz);
        assert_eq!(ix.quick_open("łódź", 3)[0].path, lodz);
    }

    #[test]
    fn removal_cleans_up() {
        let mut ix = index();
        ix.remove(&NotePath::new("Editors.md"));
        assert!(ix.backlinks(&NotePath::new("Rope Notes.md")).is_empty());
        assert_eq!(ix.search("helix", 10).len(), 0);
    }
}
