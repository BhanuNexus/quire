// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{BTreeMap, HashMap};

use quire_markdown::Task;
use quire_vault::NotePath;

use crate::attach::AttachmentText;
use crate::fulltext::{DocId, FullText, Query, SearchHit, snippet};
use crate::fuzzy::{QuickOpenHit, quick_open};
use crate::graph::{Backlink, LinkGraph};
use crate::record::NoteRecord;
use crate::terms::TermId;

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
    /// 0.12: which typed links are hierarchy edges (vault.toml `[links]`).
    link_config: crate::relations::LinkConfig,
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

    /// Insert or replace a note and update the link graph (in place when
    /// the note was already indexed with the same aliases, else rebuilt).
    pub fn upsert(&mut self, path: NotePath, src: &str) {
        let rec = NoteRecord::from_source(path, src);
        let patch = self.graph_patchable(&rec);
        let path = rec.path.clone();
        self.insert_record(rec, src.to_string());
        if patch {
            self.patch_graph(&[path]);
        } else {
            self.refresh();
        }
    }

    /// Whether storing `rec` keeps every link of the vault resolving as it
    /// does now, so [`Index::patch_graph`] can update the graph in place
    /// instead of rebuilding it: the note is already indexed (the set of
    /// notes stays the same) and keeps its aliases (its path, and so its
    /// name, is the same by construction). Call it before storing `rec`.
    pub fn graph_patchable(&self, rec: &NoteRecord) -> bool {
        self.records.get(&rec.path).is_some_and(|old| old.name == rec.name && old.aliases == rec.aliases)
    }

    /// Update the link graph for notes stored since it was built, each of
    /// which was [`Index::graph_patchable`] when stored. Costs the changed
    /// notes' links and one pass for derived edges, not a rebuild.
    pub fn patch_graph(&mut self, changed: &[NotePath]) {
        let sources = &self.sources;
        let recs: Vec<&NoteRecord> = changed.iter().filter_map(|p| self.records.get(p)).collect();
        self.graph.patch(&recs, self.records.values(), &|p| sources.get(p).map(String::as_str), &self.link_config);
    }

    /// Whether a note is indexed.
    pub fn contains(&self, p: &NotePath) -> bool {
        self.records.contains_key(p)
    }

    /// Insert a pre-built record without refreshing the graph (bulk loads).
    pub fn insert_record(&mut self, mut rec: NoteRecord, src: String) {
        self.fulltext.upsert(&rec);
        // 0.13.1: the search terms live in the keyword index now; the record's
        // copy was a second set of every note's words, kept for nothing.
        rec.search_terms = (Vec::new(), 0.0);
        // Content terms as ids of the same interner (they are among the
        // note's search terms, so this adds no term).
        let content = std::mem::take(&mut rec.content);
        rec.terms = content.into_iter().map(|(t, n)| (self.fulltext.intern(&t), n)).collect();
        rec.terms.sort_unstable();
        self.line_starts.insert(rec.path.clone(), line_starts(&src));
        self.sources.insert(rec.path.clone(), src);
        self.records.insert(rec.path.clone(), rec);
    }

    #[cfg(test)]
    /// Insert a record from the startup cache: `map` turns the segment's
    /// term ids into this index's ([`Self::intern_all`]). No graph refresh.
    pub(crate) fn insert_stored(&mut self, stored: crate::codec::Stored, map: &[TermId], src: String) {
        let crate::codec::Stored { mut rec, bm25, len } = stored;
        let tf: Vec<(TermId, f32)> = bm25.into_iter().map(|(t, f)| (map[t as usize], f)).collect();
        self.fulltext.upsert_ids(&rec.path, &tf, len);
        for (t, _) in &mut rec.terms {
            *t = map[t.index()];
        }
        rec.terms.sort_unstable();
        self.line_starts.insert(rec.path.clone(), line_starts(&src));
        self.sources.insert(rec.path.clone(), src);
        self.records.insert(rec.path.clone(), rec);
    }

    #[cfg(test)]
    /// Intern a segment's term table at once; the result maps its ids.
    pub(crate) fn intern_all(&mut self, table: &[&str]) -> Vec<TermId> {
        table.iter().map(|t| self.fulltext.intern(t)).collect()
    }

    pub fn remove(&mut self, path: &NotePath) {
        self.remove_no_refresh(path);
        self.refresh();
        self.end_batch();
    }

    /// A batch of updates is applied and everyone who reads by document id
    /// has seen it: ids of notes it removed may be given again
    /// (`docs/design/0.14-index-startup.md` §3.2).
    pub fn end_batch(&mut self) {
        self.fulltext.end_batch();
        self.attachment_text.end_batch();
    }

    /// After a bulk load: the keyword statistics summed again exactly.
    pub(crate) fn recount(&mut self) {
        self.fulltext.recount_len();
        self.attachment_text.recount_len();
    }

    /// The keyword index of the notes: stable document ids, interned terms,
    /// each document's terms and length (for per-view statistics).
    pub const fn fulltext(&self) -> &FullText {
        &self.fulltext
    }

    /// The keyword index of attachment pages (`path#page=N`).
    pub const fn attachment_fulltext(&self) -> &FullText {
        &self.attachment_text
    }

    /// The text of a term id of the notes' keyword index (a record's
    /// [`NoteRecord::terms`]).
    pub fn term(&self, id: TermId) -> &str {
        self.fulltext.term(id)
    }

    /// The id of a (folded, stemmed) term in the notes' keyword index.
    pub fn term_id(&self, term: &str) -> Option<TermId> {
        self.fulltext.term_id(term)
    }

    /// The document id of a note.
    pub fn doc_id(&self, path: &NotePath) -> Option<DocId> {
        self.fulltext.doc_id(path)
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
        self.graph = self.build_graph();
    }

    /// Build the link graph for the current records without touching the index, so a
    /// caller can do it under a read lock and `install_graph` it under a brief write.
    pub fn build_graph(&self) -> LinkGraph {
        let sources = &self.sources;
        LinkGraph::build(self.records.values(), &|p| sources.get(p).map(String::as_str), &self.link_config)
    }

    /// Replace the link graph with one from `build_graph` on the same records.
    pub fn install_graph(&mut self, graph: LinkGraph) {
        self.graph = graph;
    }

    /// The vault's typed-link settings in use.
    pub const fn link_config(&self) -> &crate::relations::LinkConfig {
        &self.link_config
    }

    /// Use other typed-link settings (rebuilds the link graph when they differ).
    pub fn set_link_config(&mut self, cfg: crate::relations::LinkConfig) {
        if cfg != self.link_config {
            self.link_config = cfg;
            self.refresh();
        }
    }

    /// Set the settings without rebuilding the graph (the caller refreshes).
    pub(crate) fn set_link_config_quiet(&mut self, cfg: crate::relations::LinkConfig) {
        self.link_config = cfg;
    }

    /// Typed links and the hierarchy.
    pub fn relations(&self) -> &crate::relations::Relations {
        self.graph.relations()
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

    /// Whether any attachment text (PDF or OCR pages) is indexed.
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
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
        self.search_where(query, limit, &|_| true)
    }

    /// [`Self::search`] over the notes `keep` accepts: the `limit` best of
    /// those, so other notes take no places.
    pub fn search_where(&self, query: &str, limit: usize, keep: crate::Keep<'_>) -> Vec<SearchHit> {
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
        // `keep` after sorting, until `limit` are kept: it can be costly (an
        // agent's policy), so it runs on the best hits only, not on every match.
        // Snippets only for what is shown.
        ranked
            .into_iter()
            .filter(|(path, _)| keep(path))
            .take(limit)
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
        #[expect(clippy::iter_over_hash_type, reason = "counts into a BTreeMap")]
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
        let mut out: Vec<(String, usize)> = counts.into_iter().map(|(k, v)| (k, v.len())).collect();
        // Tree order: `a`, `a/b`, `a/b/c`, then `a-b` (plain string order would put `a-b` before `a/b`).
        out.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
        out
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

    /// The graph as sorted text: what `LinkGraph::build` and `patch` must agree on
    /// (the order of lists that come from map iteration isn't part of it).
    fn graph_text(ix: &Index) -> String {
        let mut out = Vec::new();
        let mut paths = ix.paths();
        paths.push(NotePath::new("Nowhere.md"));
        for p in &paths {
            let mut o: Vec<String> = ix.graph().outgoing(p).iter().map(ToString::to_string).collect();
            o.sort();
            // Not sorted: a patch must keep the lists in the order a rebuild gives.
            let b: Vec<String> =
                ix.backlinks(p).iter().map(|b| format!("{} {:?} {:?} {}", b.from, b.link.range, b.link.field, b.context)).collect();
            let r: Vec<String> = ix.relations().of(p).iter().map(|e| format!("{e:?}")).collect();
            let i: Vec<String> = ix.relations().incoming(p).iter().map(|e| format!("{e:?}")).collect();
            out.push(format!("{p}\n out {o:?}\n back {b:?}\n rel {r:?}\n in {i:?}"));
        }
        let mut u: Vec<String> = ix
            .graph()
            .unresolved()
            .iter()
            .map(|(t, v)| {
                let v: Vec<&str> = v.iter().map(NotePath::as_str).collect();
                format!("{t} {v:?}")
            })
            .collect();
        u.sort();
        out.push(format!("unresolved {u:?}"));
        out.push(format!("fields {:?} hier {}", ix.relations().fields(), ix.relations().has_hierarchy()));
        out.join("\n")
    }

    /// 0.13.1: editing notes patches the graph in place; the result is what a
    /// (Names differ by more than case: which of `A.md` and `a.md` `[[a]]` means
    /// depends on map order, in a rebuild too.)
    /// rebuild from the same notes makes, through link and typed-link edits,
    /// tags of tag notes, list notes, Dendron names and lines that move.
    #[test]
    fn patched_graph_equals_a_rebuild() {
        use std::fmt::Write as _;
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % n as u64).expect("small")
        };
        let names = ["A", "B", "C", "D", "x.y", "x", "Deep/E", "Deep/F", "Ünï", "Index", "Plan"];
        let note = |next: &mut dyn FnMut(usize) -> usize, me: usize| -> String {
            let mut s = String::new();
            match me {
                9 => s.push_str("---\nBC-tag-note-tag: '#topic'\n---\n"),
                10 => s.push_str("---\nBC-list-note-field: down\nBC-list-note-neighbour-field: next\n---\n"),
                _ if next(3) == 0 => s.push_str("---\naliases: [Alias"),
                _ => {}
            }
            if s.ends_with("Alias") {
                let _ = writeln!(s, "{me}]\nup: \"[[{}]]\"\n---", names[next(names.len())]);
            }
            for _ in 0..next(8) {
                match next(9) {
                    0 => {
                        let _ = writeln!(s, "see [[{}]] here", names[next(names.len())]);
                    }
                    1 => {
                        let _ = writeln!(s, "up:: [[{}]]", names[next(names.len())]);
                    }
                    2 => {
                        let _ = writeln!(s, "- [[{}]]\n  - child [[{}]]", names[next(names.len())], names[next(names.len())]);
                    }
                    3 => s.push_str("#topic and #topic/sub\n"),
                    4 => s.push_str("[[Missing]] and [[Gone|shown]] and ![[pic.png]]\n"),
                    5 => s.push_str("[[unï]] [[Alias3]] [[#Heading]]\n"),
                    6 => {
                        let _ = writeln!(s, "supports:: [[{}]]", names[next(names.len())]);
                    }
                    _ => s.push_str("plain words\n"),
                }
            }
            s
        };
        let cfg = crate::relations::LinkConfig {
            dendron: crate::relations::Dendron { enabled: true, ..Default::default() },
            ..crate::relations::LinkConfig::default()
        };
        let mut ix = Index::new();
        ix.set_link_config_quiet(cfg);
        let mut texts: Vec<String> = Vec::new();
        for (i, n) in names.iter().enumerate() {
            let t = note(&mut next, i);
            ix.insert_record(NoteRecord::from_source(NotePath::new(format!("{n}.md")), &t), t.clone());
            texts.push(t);
        }
        ix.refresh();
        let mut patched = 0;
        for round in 0..400 {
            let i = next(names.len());
            let path = NotePath::new(format!("{}.md", names[i]));
            // Mostly the note's own lines moved around, sometimes a new text.
            let t = if next(2) == 0 { format!("{}\n{}", "x".repeat(next(5)), texts[i]) } else { note(&mut next, i) };
            if ix.graph_patchable(&NoteRecord::from_source(path.clone(), &t)) {
                patched += 1;
            }
            ix.upsert(path, &t);
            texts[i] = t;
            let mut fresh = Index::new();
            fresh.set_link_config_quiet(ix.link_config().clone());
            for p in ix.paths() {
                let src = ix.source(&p).expect("indexed").to_string();
                fresh.insert_record(NoteRecord::from_source(p, &src), src);
            }
            fresh.refresh();
            assert_eq!(graph_text(&ix), graph_text(&fresh), "round {round}");
        }
        assert!(patched > 200, "most edits patch the graph ({patched})");
    }

    /// Lists an agent tool cuts at a limit come out in the same order on
    /// every run: backlinks, typed links (both ways) and unresolved links do
    /// not follow hash order.
    #[test]
    fn link_lists_do_not_follow_hash_order() {
        let notes: Vec<(String, String)> = (0..40)
            .map(|i| (format!("N{i:02}.md"), format!("up:: [[Hub]]\nsee [[Hub]] and [[Missing]]\nsupports:: [[N{:02}]]\n", (i + 1) % 40)))
            .chain([("Hub.md".to_string(), "# Hub\n".to_string())])
            .collect();
        let raw = |order: &mut dyn Iterator<Item = &(String, String)>| {
            let mut ix = Index::new();
            for (p, t) in order {
                ix.insert_record(NoteRecord::from_source(NotePath::new(p.as_str()), t), t.clone());
            }
            ix.refresh();
            let hub = NotePath::new("Hub.md");
            let back: Vec<String> = ix.backlinks(&hub).iter().map(|b| format!("{} {:?}", b.from, b.link.range)).collect();
            let incoming: Vec<String> = ix.relations().incoming(&hub).iter().map(|e| format!("{e:?}")).collect();
            let unresolved: Vec<String> = ix.graph().unresolved().iter().map(|(t, v)| format!("{t} {v:?}")).collect();
            format!("{back:?}\n{incoming:?}\n{unresolved:?}")
        };
        let first = raw(&mut notes.iter());
        for _ in 0..5 {
            assert_eq!(raw(&mut notes.iter().rev()), first);
            assert_eq!(raw(&mut notes.iter()), first);
        }
    }

    /// A note keeps its document id across edits; a removed note's id is
    /// given again only after the batch that removed it
    /// (`docs/design/0.14-index-startup.md` §3.2).
    #[test]
    fn document_ids_are_stable_and_reused_only_after_the_batch() {
        let mut ix = Index::new();
        let (a, b, c, d) = (NotePath::new("Alpha.md"), NotePath::new("Beta.md"), NotePath::new("Gamma.md"), NotePath::new("Delta.md"));
        ix.upsert(a.clone(), "rope tree");
        ix.upsert(b, "rope bread");
        let id_a = ix.doc_id(&a).expect("indexed");
        let names = |ix: &Index, id: DocId| {
            let mut v: Vec<String> = ix.fulltext().doc_terms(id).iter().map(|t| ix.term(*t).to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(names(&ix, id_a), ["alpha", "rope", "tree"]);
        assert_eq!(ix.fulltext().doc_len(id_a), Some(4.0), "two words, the name and the title");
        ix.upsert(a.clone(), "rope cable");
        assert_eq!(ix.doc_id(&a), Some(id_a), "an edit keeps the id");
        assert_eq!(names(&ix, id_a), ["alpha", "cable", "rope"]);
        // The record's content terms are ids of the same interner.
        let rec = ix.get(&a).expect("indexed");
        let content: Vec<&str> = rec.terms.iter().map(|(t, _)| ix.term(*t)).collect();
        assert!(content.contains(&"cable") && content.contains(&"rope"), "{content:?}");
        ix.remove_no_refresh(&a);
        assert_eq!(ix.fulltext().doc_len(id_a), None);
        assert!(ix.fulltext().doc_terms(id_a).is_empty());
        ix.upsert(c.clone(), "rope");
        assert_ne!(ix.doc_id(&c), Some(id_a), "not reused in the batch that removed it");
        ix.end_batch();
        ix.upsert(d.clone(), "rope");
        assert_eq!(ix.doc_id(&d), Some(id_a), "reused after it");
        assert_eq!(ix.fulltext().doc_count(), 3);
        // A term no note uses any more is gone from search but keeps its id.
        assert!(ix.idf("tree").is_none() && ix.term_id("tree").is_some());
        assert!(ix.search("tree", 5).is_empty());
    }

    /// Frame rule 4 (`docs/design/0.14-architecture.md` §7): the keyword index
    /// after random edits and removals answers exactly as one built from the
    /// same notes.
    #[test]
    fn keyword_index_after_edits_equals_a_rebuild() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % n as u64).expect("small")
        };
        let words = ["rope", "ropes", "tree", "bread", "editor", "edit", "café", "cafe", "zebra", "undo", "#tag", "#tag/sub"];
        let mut ix = Index::new();
        let mut live: BTreeMap<NotePath, String> = BTreeMap::new();
        for round in 0..300 {
            let path = NotePath::new(format!("N{}.md", next(12)));
            if next(4) == 0 {
                ix.remove_no_refresh(&path);
                live.remove(&path);
            } else {
                let text: Vec<&str> = (0..next(12)).map(|_| words[next(words.len())]).collect();
                let text = text.join(" ");
                ix.upsert(path.clone(), &text);
                live.insert(path, text);
            }
            if next(3) == 0 {
                ix.end_batch();
            }
            let mut fresh = Index::new();
            for (p, t) in &live {
                fresh.insert_record(NoteRecord::from_source(p.clone(), t), t.clone());
            }
            fresh.refresh();
            for q in ["rope", "ro", "e", "cafe tree", "edit", "#tag", "zebra -undo", "\"rope tree\"", "undo ed"] {
                let got: Vec<(String, u32, String)> =
                    ix.search(q, 50).into_iter().map(|h| (h.path.to_string(), h.score.to_bits(), h.snippet)).collect();
                let want: Vec<(String, u32, String)> =
                    fresh.search(q, 50).into_iter().map(|h| (h.path.to_string(), h.score.to_bits(), h.snippet)).collect();
                assert_eq!(got, want, "round {round}, query {q:?}");
            }
            for p in live.keys() {
                let terms = |ix: &Index| {
                    let rec = ix.get(p).expect("indexed");
                    let mut v: Vec<(String, u32)> = rec.terms.iter().map(|(t, n)| (ix.term(*t).to_string(), *n)).collect();
                    v.sort();
                    v
                };
                assert_eq!(terms(&ix), terms(&fresh));
            }
            assert_eq!(ix.fulltext().doc_count(), live.len());
            assert_eq!(ix.fulltext().total_len().to_bits(), fresh.fulltext().total_len().to_bits());
        }
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

    /// 0.12 verify: `#a/b/c` is a tree. A parent tag counts and finds its children, in the
    /// tag list, in search and in `notes_with_tag`; the list is in tree order.
    #[test]
    fn nested_tags_form_a_tree() {
        let mut ix = Index::new();
        for (p, s) in [
            ("A.md", "# A\n#proj/web/ui and #proj/web/api\n"),
            ("B.md", "---\ntags: [proj/ios, Proj/Web]\n---\n# B\n"),
            ("C.md", "# C\n#proj-old and #project\n"),
            ("D.md", "# D\nin code `#proj/hidden` is not a tag\n"),
        ] {
            ix.insert_record(NoteRecord::from_source(NotePath::new(p), s), s.to_string());
        }
        ix.refresh();
        let tags = ix.tags();
        let names: Vec<&str> = tags.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(names, ["proj", "proj/ios", "proj/web", "proj/web/api", "proj/web/ui", "proj-old", "project"]);
        let count = |t: &str| tags.iter().find(|(n, _)| n == t).map(|(_, c)| *c);
        assert_eq!((count("proj"), count("proj/web"), count("proj/web/ui"), count("proj/ios")), (Some(2), Some(2), Some(1), Some(1)));
        // A parent finds the notes of every descendant, and never a lookalike.
        let paths = |v: Vec<NotePath>| v.iter().map(|p| p.as_str().to_string()).collect::<Vec<_>>();
        assert_eq!(paths(ix.notes_with_tag("proj")), ["A.md", "B.md"]);
        assert_eq!(paths(ix.notes_with_tag("#proj/web")), ["A.md", "B.md"]);
        assert_eq!(paths(ix.notes_with_tag("proj/web/ui")), ["A.md"]);
        let found = |q: &str| ix.search(q, 10).iter().map(|h| h.path.as_str().to_string()).collect::<Vec<_>>();
        assert_eq!(found("#proj"), ["A.md", "B.md"]);
        assert_eq!(found("#proj/web/api"), ["A.md"]);
        assert!(found("#proj/hidden").is_empty());
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
