// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The term interner of a keyword index (`docs/design/0.14-index-startup.md` §3.2).
//!
//! Every distinct (folded, stemmed) word gets a [`TermId`] once; postings,
//! each document's term list and each record's content terms hold ids, not
//! strings. A term no document uses any more keeps its id until the index
//! is built again (its posting list is empty), so an id never changes meaning
//! while an index lives.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::Arc;

use crate::tokenize::FastHash;

/// A term of one keyword index. Only meaningful with the index that gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermId(pub u32);

impl TermId {
    /// The id as an index into a table by term.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Terms and their ids, with a sorted view for prefix search.
#[derive(Debug, Default)]
pub(crate) struct Terms {
    by_id: Vec<Arc<str>>,
    /// Lookup while interning: one hash lookup per distinct term of a note.
    ids: HashMap<Arc<str>, TermId, FastHash>,
    /// The same terms in byte order, for search-as-you-type.
    sorted: BTreeMap<Arc<str>, TermId>,
}

impl Terms {
    /// The id of `term`, given a new one if it has none.
    pub(crate) fn intern(&mut self, term: &str) -> TermId {
        if let Some(&id) = self.ids.get(term) {
            return id;
        }
        let id = TermId(u32::try_from(self.by_id.len()).expect("fewer than 2^32 distinct terms"));
        let key: Arc<str> = Arc::from(term);
        self.by_id.push(Arc::clone(&key));
        self.ids.insert(Arc::clone(&key), id);
        self.sorted.insert(key, id);
        id
    }

    /// The id of `term`, if it was ever interned.
    pub(crate) fn get(&self, term: &str) -> Option<TermId> {
        self.ids.get(term).copied()
    }

    /// The text of `id`.
    pub(crate) fn name(&self, id: TermId) -> &str {
        self.by_id.get(id.index()).map_or("", |s| s)
    }

    /// Every term starting with `prefix`, in byte order.
    pub(crate) fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, TermId)> + 'a {
        self.sorted
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(t, _)| t.starts_with(prefix))
            .map(|(t, id)| (&**t, *id))
    }

    /// How many ids were given.
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_prefixes_sorted() {
        let mut t = Terms::default();
        let rope = t.intern("rope");
        assert_eq!(t.intern("ropes"), TermId(1));
        assert_eq!(t.intern("rope"), rope);
        t.intern("apple");
        assert_eq!(t.get("rope"), Some(rope));
        assert_eq!(t.get("pear"), None);
        assert_eq!(t.name(rope), "rope");
        let p: Vec<&str> = t.with_prefix("rop").map(|(s, _)| s).collect();
        assert_eq!(p, ["rope", "ropes"]);
        assert_eq!(t.len(), 3);
    }
}
