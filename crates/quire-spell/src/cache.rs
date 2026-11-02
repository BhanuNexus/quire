// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small bounded cache of per-line results, so an editor can ask for the
//! misspellings of every visible line on every frame.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::ops::Range;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::LineContext;

/// Entries per generation; the cache holds at most twice this many lines.
const GENERATION_SIZE: usize = 2048;

/// Two-generation cache (an approximation of LRU): new results go into
/// `current`; when it fills up it becomes `previous` and the old `previous`
/// is dropped. Hits in `previous` are promoted.
#[derive(Debug, Default)]
pub(crate) struct LineCache {
    inner: Mutex<Inner>,
    hasher: RandomState,
}

#[derive(Debug, Default)]
struct Inner {
    /// Bumped whenever cached results become stale (word lists changed).
    epoch: u64,
    current: HashMap<u64, Entry>,
    previous: HashMap<u64, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    line: Box<str>,
    before: LineContext,
    after: LineContext,
    ranges: Vec<Range<usize>>,
}

impl LineCache {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn key(&self, line: &str, before: LineContext) -> u64 {
        self.hasher.hash_one((line, before))
    }

    /// Cached ranges and the context after `line`, if present.
    pub(crate) fn get(&self, line: &str, before: LineContext) -> Option<(Vec<Range<usize>>, LineContext)> {
        let key = self.key(line, before);
        let mut inner = self.lock();
        let matches = |e: &Entry| &*e.line == line && e.before == before;
        if let Some(e) = inner.current.get(&key).filter(|e| matches(e)) {
            return Some((e.ranges.clone(), e.after));
        }
        let e = inner.previous.remove(&key).filter(|e| matches(e))?;
        let hit = (e.ranges.clone(), e.after);
        inner.insert(key, e);
        Some(hit)
    }

    /// The current epoch; pass it back to [`insert`](Self::insert) so results
    /// computed before an invalidation are not stored.
    pub(crate) fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    pub(crate) fn insert(&self, line: &str, before: LineContext, after: LineContext, ranges: Vec<Range<usize>>, epoch: u64) {
        let key = self.key(line, before);
        let mut inner = self.lock();
        if inner.epoch == epoch {
            inner.insert(key, Entry { line: line.into(), before, after, ranges });
        }
    }

    pub(crate) fn clear(&self) {
        let mut inner = self.lock();
        inner.epoch = inner.epoch.wrapping_add(1);
        inner.current.clear();
        inner.previous.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        let inner = self.lock();
        inner.current.len() + inner.previous.len()
    }
}

impl Inner {
    fn insert(&mut self, key: u64, entry: Entry) {
        if self.current.len() >= GENERATION_SIZE && !self.current.contains_key(&key) {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(key, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_invalidated() {
        let cache = LineCache::default();
        let ctx = LineContext::new();
        let epoch = cache.epoch();
        for i in 0..GENERATION_SIZE * 3 {
            cache.insert(&format!("line {i}"), ctx, ctx, vec![0..4, 5..9], epoch);
        }
        assert!(cache.len() <= GENERATION_SIZE * 2);
        let last = format!("line {}", GENERATION_SIZE * 3 - 1);
        assert_eq!(cache.get(&last, ctx).map(|(r, _)| r), Some(vec![0..4, 5..9]));
        let mut other = ctx;
        other.advance("```");
        assert!(cache.get(&last, other).is_none(), "the context is part of the key");
        cache.clear();
        assert!(cache.get(&last, ctx).is_none());
        // A result computed before the clear is not stored.
        cache.insert("stale", ctx, ctx, vec![], epoch);
        assert!(cache.get("stale", ctx).is_none());
    }
}
