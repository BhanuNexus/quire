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
///
/// The third argument says whether it may read the vault (Fable's review,
/// S3). A view asks with `true` on the caller's thread, where it must
/// answer; the index's worker asks with `false` under its write lock, where
/// `None` ("that needs a storage call") leaves the document hidden until the
/// next caller's own predicate decides it.
pub type Visible = Arc<dyn Fn(&NotePath, &[String], bool) -> Option<bool> + Send + Sync>;

/// A [`Visible`] that never needs the vault.
pub fn visible(f: impl Fn(&NotePath, &[String]) -> bool + Send + Sync + 'static) -> Visible {
    Arc::new(move |p: &NotePath, tags: &[String], _: bool| Some(f(p, tags)))
}

/// What a view is known by: the caller's visibility fingerprint.
pub type ScopeKey = [u8; 32];

/// Views with keyword statistics kept at most (`docs/design/0.14-agent-index.md` §3.6).
pub const MAX_SCOPES: usize = 32;

/// What a view knows of one document id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Slot {
    /// No document has this id.
    #[default]
    Free,
    /// A document the caller may not read.
    Hidden,
    /// A document the caller may read.
    Member,
}

/// Which documents of one keyword index a view holds, and their totals.
///
/// Membership is kept apart from length (Fable's review, S2): whatever a
/// document's length, a member counts.
#[derive(Debug, Clone, Default)]
pub struct DocStats {
    /// By [`DocId`].
    slots: Vec<Slot>,
    /// By [`DocId`]: a member's length, 0 for any other document.
    lens: Vec<f32>,
    n: usize,
    /// Documents the caller may not read.
    hidden: usize,
    /// Sums of whole numbers: exact in `f64`, whatever the order.
    total_len: f64,
}

impl DocStats {
    /// How many documents are members.
    pub const fn n(&self) -> usize {
        self.n
    }

    /// How many documents are not members.
    pub const fn hidden(&self) -> usize {
        self.hidden
    }

    /// The members' total length.
    pub const fn total_len(&self) -> f64 {
        self.total_len
    }

    /// Whether `id` is a member.
    pub fn contains(&self, id: DocId) -> bool {
        self.slots.get(id.index()) == Some(&Slot::Member)
    }

    /// Bytes this holds on the heap.
    pub const fn heap_bytes(&self) -> usize {
        self.lens.capacity() * size_of::<f32>() + self.slots.capacity() * size_of::<Slot>()
    }

    /// Set what `id` is: no document, a hidden one, or a member of length `len`.
    fn set(&mut self, id: DocId, slot: Slot, len: f32) {
        let i = id.index();
        if i >= self.slots.len() {
            if slot == Slot::Free {
                return;
            }
            self.slots.resize(i + 1, Slot::Free);
            self.lens.resize(i + 1, 0.0);
        }
        match self.slots[i] {
            Slot::Member => {
                self.n -= 1;
                self.total_len -= f64::from(self.lens[i]);
            }
            Slot::Hidden => self.hidden -= 1,
            Slot::Free => {}
        }
        self.slots[i] = slot;
        self.lens[i] = if slot == Slot::Member { len } else { 0.0 };
        match slot {
            Slot::Member => {
                self.n += 1;
                self.total_len += f64::from(len);
            }
            Slot::Hidden => self.hidden += 1,
            Slot::Free => {}
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
    /// The number of meaning-model fits started when this view last hid a
    /// document: only a model whose fit started later can be free of it
    /// (Fable's review, S1). `None`: it never hid one.
    hid_at: Option<u64>,
    /// Note documents the worker could not decide without the vault (hidden
    /// until then), and whether an attachment page was one.
    unsure: Vec<DocId>,
    unsure_pages: bool,
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
        Self::build_at(ix, visible, 0)
    }

    /// [`Self::build`], with `fits` meaning-model fits started so far.
    pub(crate) fn build_at(ix: &Index, visible: Visible, fits: u64) -> Self {
        let mut s = Self {
            visible,
            build: ix.build(),
            notes: DocStats::default(),
            pages: DocStats::default(),
            hid_at: None,
            unsure: Vec::new(),
            unsure_pages: false,
        };
        let ids: Vec<DocId> = (0..ix.fulltext().doc_slots()).filter_map(|i| u32::try_from(i).ok().map(DocId)).collect();
        s.patch_notes(ix, &ids, true);
        s.rebuild_pages(ix, true);
        s.saw(false, fits);
        s
    }

    /// Take `visible`, a newer caller's predicate for the same key, and
    /// decide with it what the worker left undecided. On the caller's thread.
    fn refresh(&mut self, ix: &Index, visible: Visible, fits: u64) {
        self.visible = visible;
        if self.unsure.is_empty() && !self.unsure_pages {
            return;
        }
        let hid = !self.hides_nothing();
        let ids = std::mem::take(&mut self.unsure);
        self.patch_notes(ix, &ids, true);
        if self.unsure_pages {
            self.rebuild_pages(ix, true);
        }
        self.saw(hid, fits);
    }

    /// Whether anything waits for a caller's predicate.
    pub const fn undecided(&self) -> bool {
        !self.unsure.is_empty() || self.unsure_pages
    }

    /// Note when the view hides a document, before (`hid`) or after a change,
    /// with `fits` meaning-model fits started so far.
    fn saw(&mut self, hid: bool, fits: u64) {
        if hid || !self.hides_nothing() {
            self.hid_at = Some(fits);
        }
    }

    /// Whether a meaning model whose fit was number `fit` may serve this
    /// view: it hides nothing, and the fit started after it last hid a
    /// document.
    pub(crate) fn meaning_from(&self, fit: u64) -> bool {
        self.hides_nothing() && self.hid_at.is_none_or(|h| fit > h)
    }

    /// What note document `id` is to this view and its length; `None` when
    /// that needs the vault and `storage` is false.
    fn note_slot(&self, ix: &Index, id: DocId, storage: bool) -> Option<(Slot, f32)> {
        let ft = ix.fulltext();
        let (Some(path), Some(len)) = (ft.path_of(id), ft.doc_len(id)) else { return Some((Slot::Free, 0.0)) };
        let tags = ix.get(path).map_or(&[][..], |r| r.tags.as_slice());
        Some(if (self.visible)(path, tags, storage)? { (Slot::Member, len) } else { (Slot::Hidden, 0.0) })
    }

    /// Bring the note documents `ids` up to date (each changed, added or
    /// removed since the view last saw it). `storage`: the predicate may
    /// read the vault (not under the index's write lock).
    pub(crate) fn patch_notes(&mut self, ix: &Index, ids: &[DocId], storage: bool) {
        for id in ids {
            self.unsure.retain(|u| u != id);
            if let Some((slot, len)) = self.note_slot(ix, *id, storage) {
                self.notes.set(*id, slot, len);
            } else {
                self.notes.set(*id, Slot::Hidden, 0.0);
                self.unsure.push(*id);
            }
        }
    }

    /// Count the attachment pages again (attachment text changes rarely).
    pub(crate) fn rebuild_pages(&mut self, ix: &Index, storage: bool) {
        let at = ix.attachment_fulltext();
        let mut pages = DocStats::default();
        self.unsure_pages = false;
        for i in 0..at.doc_slots() {
            let id = DocId(u32::try_from(i).unwrap_or(u32::MAX));
            let (Some(file), Some(len)) = (at.path_of(id).and_then(page_file), at.doc_len(id)) else { continue };
            match (self.visible)(&file, &[], storage) {
                Some(true) => pages.set(id, Slot::Member, len),
                Some(false) => pages.set(id, Slot::Hidden, 0.0),
                None => {
                    pages.set(id, Slot::Hidden, 0.0);
                    self.unsure_pages = true;
                }
            }
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
    pub const fn hides_nothing(&self) -> bool {
        self.notes.hidden == 0 && self.pages.hidden == 0
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
            Self::View(s) => (s.visible)(p, &[], true).unwrap_or(false),
        }
    }

    /// Whether meaning search may be used: the model is fitted on every
    /// note, so only a view that hides nothing may use it (0.14.0). A
    /// service's [`crate::Scoped`] also asks that the model was fitted after
    /// the view last hid a note.
    pub const fn meaning(self, _ix: &Index) -> bool {
        match self {
            Self::Full => true,
            Self::View(s) => s.hides_nothing(),
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
    /// The view for `key` if it belongs to `ix`'s build, marked used, with
    /// `visible` (this caller's predicate) in place of the one it had, and
    /// what the worker left undecided decided by it (Fable's review, S3).
    /// `fits`: meaning-model fits started so far.
    pub(crate) fn get(&mut self, key: &ScopeKey, ix: &Index, visible: &Visible, fits: u64) -> Option<Arc<Scope>> {
        self.clock += 1;
        let clock = self.clock;
        let e = self.list.iter_mut().find(|e| e.key == *key && e.scope.build == ix.build())?;
        e.used = clock;
        // A copy only while a reader holds the view: two tables of a few bytes a note.
        Arc::make_mut(&mut e.scope).refresh(ix, Arc::clone(visible), fits);
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
                // What the view hid before the load may still be in the meaning model.
                let hid_at = e.scope.hid_at.max(scope.hid_at);
                let mut scope = scope;
                Arc::make_mut(&mut scope).hid_at = hid_at;
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
    /// `fits`: meaning-model fits started so far.
    pub(crate) fn patch(&mut self, ix: &Index, ids: &[DocId], pages: bool, fits: u64) {
        for e in &mut self.list {
            if e.scope.build != ix.build() {
                continue;
            }
            let s = Arc::make_mut(&mut e.scope);
            let hid = !s.hides_nothing();
            // No storage call under the write lock: what needs one waits for the next caller.
            s.patch_notes(ix, ids, false);
            if pages {
                s.rebuild_pages(ix, false);
            }
            s.saw(hid, fits);
        }
    }

    pub(crate) const fn len(&self) -> usize {
        self.list.len()
    }
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod tests;
