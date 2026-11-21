// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Search statistics per agent view (`docs/design/0.14-agent-index.md` §3).
//!
//! A *view* (type [`Scope`]: `views.rs` already means Bases views) is the
//! set of indexed documents one caller may read: notes, and attachment
//! pages by their file's path. It holds only statistics: which documents
//! are members (by stable [`DocId`]), how many, and their total length.
//! Records, postings and sources are the index's own and are shared.
//! Ranking through a view ([`FullText::rank_in`](crate::FullText)) counts
//! each term's document frequency among members only, so a search gives
//! exactly what an index holding only the view's notes gives: a word found
//! only in notes the caller can't see changes nothing it is told.
//!
//! - **Keyed** by the caller's visibility fingerprint ([`ScopeKey`]):
//!   callers that see the same notes share one view.
//! - **Built on the calling thread** under the index's read lock
//!   ([`crate::IndexService::scoped`]); the registry sits beside the index
//!   (lock order: index, then registry).
//! - **Patched** by the index worker, under its write lock, for every note
//!   a batch changed, before the batch's removed document ids are freed.
//!   A load starts a new build of the index ([`Index::build`]): a view of an
//!   older build is built again on its next use.
//! - **At most [`MAX_SCOPES`]**, the least recently used dropped first; no
//!   idle timer.
//! - **Nothing on disk**: views live in memory only.
//!
//! In 0.14.0 a view that hides a note has no meaning search: the meaning
//! model is fitted on every note ([`ScopeRead::meaning`]).

use std::sync::Arc;

use quire_vault::NotePath;

use crate::fulltext::{DocId, SearchHit};
use crate::index::Index;

/// A note's path and its record's tags to "may this caller read it".
pub type Visible = Arc<dyn Fn(&NotePath, &[String]) -> bool + Send + Sync>;

/// What a view is known by: the caller's visibility fingerprint.
pub type ScopeKey = [u8; 32];

/// Views with keyword statistics kept at most (`docs/design/0.14-agent-index.md` §3.6).
pub const MAX_SCOPES: usize = 32;

/// Which documents of one keyword index a view holds, and their totals.
#[derive(Debug, Clone, Default)]
pub struct DocStats {
    /// By [`DocId`]: a member's length, 0 for any other document (every
    /// indexed document is at least 1 long).
    lens: Vec<f32>,
    n: usize,
    /// Sums of whole numbers: exact in `f64`, whatever the order.
    total_len: f64,
}

impl DocStats {
    /// How many documents are members.
    pub const fn n(&self) -> usize {
        self.n
    }

    /// The members' total length.
    pub const fn total_len(&self) -> f64 {
        self.total_len
    }

    /// Whether `id` is a member.
    pub fn contains(&self, id: DocId) -> bool {
        self.lens.get(id.index()).is_some_and(|l| *l > 0.0)
    }

    /// Bytes this holds on the heap.
    pub const fn heap_bytes(&self) -> usize {
        self.lens.capacity() * size_of::<f32>()
    }

    /// Make `id` a member of length `len`, or no member (`None`).
    fn set(&mut self, id: DocId, len: Option<f32>) {
        let i = id.index();
        if i >= self.lens.len() {
            if len.is_none() {
                return;
            }
            self.lens.resize(i + 1, 0.0);
        }
        let old = self.lens[i];
        if old > 0.0 {
            self.n -= 1;
            self.total_len -= f64::from(old);
        }
        let new = len.unwrap_or(0.0);
        self.lens[i] = new;
        if new > 0.0 {
            self.n += 1;
            self.total_len += f64::from(new);
        }
    }
}

/// One caller's view of the index (see the module docs).
#[derive(Clone)]
pub struct Scope {
    visible: Visible,
    /// The [`Index::build`] these statistics belong to.
    build: u64,
    notes: DocStats,
    pages: DocStats,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("build", &self.build)
            .field("notes", &self.notes.n)
            .field("pages", &self.pages.n)
            .finish_non_exhaustive()
    }
}

/// The file of an attachment page's key (`path#page=N`).
fn page_file(key: &NotePath) -> Option<NotePath> {
    key.as_str().rsplit_once("#page=").map(|(p, _)| NotePath::new(p))
}

impl Scope {
    /// The view of `ix` for `visible`: one call of it per note and per
    /// attachment, pure (no file is read).
    pub fn build(ix: &Index, visible: Visible) -> Self {
        let mut s = Self { visible, build: ix.build(), notes: DocStats::default(), pages: DocStats::default() };
        let ids: Vec<DocId> = (0..ix.fulltext().doc_slots()).filter_map(|i| u32::try_from(i).ok().map(DocId)).collect();
        s.patch_notes(ix, &ids);
        s.rebuild_pages(ix);
        s
    }

    /// The length of note document `id` if it is a member.
    fn note_len(&self, ix: &Index, id: DocId) -> Option<f32> {
        let ft = ix.fulltext();
        let path = ft.path_of(id)?;
        let rec = ix.get(path)?;
        (self.visible)(path, &rec.tags).then(|| ft.doc_len(id)).flatten()
    }

    /// Bring the note documents `ids` up to date (each changed, added or
    /// removed since the view last saw it).
    pub(crate) fn patch_notes(&mut self, ix: &Index, ids: &[DocId]) {
        for id in ids {
            let len = self.note_len(ix, *id);
            self.notes.set(*id, len);
        }
    }

    /// Count the attachment pages again (attachment text changes rarely).
    pub(crate) fn rebuild_pages(&mut self, ix: &Index) {
        let at = ix.attachment_fulltext();
        let mut pages = DocStats::default();
        for i in 0..at.doc_slots() {
            let id = DocId(u32::try_from(i).unwrap_or(u32::MAX));
            let member = at.path_of(id).and_then(page_file).is_some_and(|f| (self.visible)(&f, &[]));
            pages.set(id, at.doc_len(id).filter(|_| member));
        }
        self.pages = pages;
    }

    /// The index build these statistics belong to.
    pub const fn build_of(&self) -> u64 {
        self.build
    }

    /// The notes' statistics.
    pub const fn notes(&self) -> &DocStats {
        &self.notes
    }

    /// The attachment pages' statistics.
    pub const fn pages(&self) -> &DocStats {
        &self.pages
    }

    /// Whether the view holds every note and page of `ix`.
    pub fn hides_nothing(&self, ix: &Index) -> bool {
        self.notes.n == ix.fulltext().doc_count() && self.pages.n == ix.attachment_fulltext().doc_count()
    }

    /// Bytes this view holds on the heap (`docs/design/0.14-agent-index.md` G6).
    pub const fn heap_bytes(&self) -> usize {
        self.notes.heap_bytes() + self.pages.heap_bytes()
    }
}

/// How a search counts: over the whole index (the person), or over one
/// view (an agent).
#[derive(Debug, Clone, Copy)]
pub enum ScopeRead<'a> {
    Full,
    View(&'a Scope),
}

impl<'a> ScopeRead<'a> {
    pub(crate) const fn notes(self) -> Option<&'a DocStats> {
        match self {
            Self::Full => None,
            Self::View(s) => Some(&s.notes),
        }
    }

    pub(crate) const fn pages(self) -> Option<&'a DocStats> {
        match self {
            Self::Full => None,
            Self::View(s) => Some(&s.pages),
        }
    }

    /// Whether a note is in the view.
    pub fn has_note(self, ix: &Index, p: &NotePath) -> bool {
        match self {
            Self::Full => true,
            Self::View(s) => ix.doc_id(p).is_some_and(|id| s.notes.contains(id)),
        }
    }

    /// Whether an attachment file is in the view.
    pub fn has_file(self, p: &NotePath) -> bool {
        match self {
            Self::Full => true,
            Self::View(s) => (s.visible)(p, &[]),
        }
    }

    /// Whether meaning search may be used: the model is fitted on every
    /// note, so only a view that hides nothing may use it (0.14.0).
    pub fn meaning(self, ix: &Index) -> bool {
        match self {
            Self::Full => true,
            Self::View(s) => s.hides_nothing(ix),
        }
    }
}

impl Index {
    /// Keyword search through a view: [`Self::search`] as an index of only
    /// the view's notes gives it, scores included.
    pub fn search_in(&self, query: &str, limit: usize, scope: ScopeRead<'_>) -> Vec<SearchHit> {
        self.search_ranked(query, limit, scope.notes(), &|p| scope.has_note(self, p))
    }

    /// How rare a (folded, stemmed) word is among the view's notes; `None`
    /// when none of them uses it.
    pub fn idf_in(&self, term: &str, scope: ScopeRead<'_>) -> Option<f32> {
        self.fulltext.term_idf_in(term, scope.notes())
    }

    /// How many notes the view holds.
    pub fn len_in(&self, scope: ScopeRead<'_>) -> usize {
        scope.notes().map_or_else(|| self.len(), DocStats::n)
    }
}

/// One registered view.
struct Entry {
    key: ScopeKey,
    scope: Arc<Scope>,
    used: u64,
}

/// The views of one [`crate::IndexService`], beside its index.
#[derive(Default)]
pub(crate) struct Scopes {
    list: Vec<Entry>,
    clock: u64,
}

impl Scopes {
    /// The view for `key` if it belongs to `build`, marked used.
    pub(crate) fn get(&mut self, key: &ScopeKey, build: u64) -> Option<Arc<Scope>> {
        self.clock += 1;
        let clock = self.clock;
        let e = self.list.iter_mut().find(|e| e.key == *key && e.scope.build == build)?;
        e.used = clock;
        Some(Arc::clone(&e.scope))
    }

    /// Keep `scope` for `key` (replacing an older one), dropping the least
    /// recently used view past [`MAX_SCOPES`]. Another caller's view of the
    /// same build, put in meanwhile, wins: it is the same.
    pub(crate) fn insert(&mut self, key: ScopeKey, scope: Arc<Scope>) -> Arc<Scope> {
        self.clock += 1;
        let clock = self.clock;
        if let Some(e) = self.list.iter_mut().find(|e| e.key == key) {
            if e.scope.build != scope.build {
                e.scope = scope;
            }
            e.used = clock;
            return Arc::clone(&e.scope);
        }
        if self.list.len() >= MAX_SCOPES
            && let Some(i) = self.list.iter().enumerate().min_by_key(|(_, e)| e.used).map(|(i, _)| i)
        {
            self.list.swap_remove(i);
        }
        self.list.push(Entry { key, scope: Arc::clone(&scope), used: clock });
        scope
    }

    /// Patch every view of `ix`'s build for the note documents `ids` (and
    /// the attachment pages when they changed). Called by the index worker
    /// under its write lock: no reader holds a view then.
    pub(crate) fn patch(&mut self, ix: &Index, ids: &[DocId], pages: bool) {
        for e in &mut self.list {
            if e.scope.build != ix.build() {
                continue;
            }
            let s = Arc::make_mut(&mut e.scope);
            s.patch_notes(ix, ids);
            if pages {
                s.rebuild_pages(ix);
            }
        }
    }

    pub(crate) const fn len(&self) -> usize {
        self.list.len()
    }
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
