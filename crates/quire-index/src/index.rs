// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{BTreeMap, HashMap};

use quire_markdown::Task;
use quire_vault::NotePath;

use crate::fulltext::{FullText, Query, SearchHit, snippet};
use crate::fuzzy::{QuickOpenHit, quick_open};
use crate::graph::{Backlink, LinkGraph};
use crate::record::NoteRecord;

/// The whole derived model of a vault. Synchronous and self-contained.
#[derive(Debug, Default)]
pub struct Index {
    records: HashMap<NotePath, NoteRecord>,
    sources: HashMap<NotePath, String>,
    fulltext: FullText,
    graph: LinkGraph,
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
        self.fulltext.remove(path);
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
        let mut hits: Vec<SearchHit> = self
            .fulltext
            .rank(&q)
            .into_iter()
            .filter_map(|(path, score)| {
                let rec = self.records.get(&path)?;
                if !q.tags.iter().all(|t| rec.has_tag(t)) {
                    return None;
                }
                if !q.phrases.is_empty() {
                    let lower = rec.plain.to_lowercase();
                    if !q.phrases.iter().all(|ph| lower.contains(ph.as_str())) {
                        return None;
                    }
                }
                let (snippet, highlights) = snippet(&rec.plain, &terms, 140);
                Some(SearchHit { path, title: rec.title.clone(), score, snippet, highlights })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(limit);
        hits
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
    fn removal_cleans_up() {
        let mut ix = index();
        ix.remove(&NotePath::new("Editors.md"));
        assert!(ix.backlinks(&NotePath::new("Rope Notes.md")).is_empty());
        assert_eq!(ix.search("helix", 10).len(), 0);
    }
}
