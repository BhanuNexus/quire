// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small bounded least-recently-used cache, keyed by a 64-bit hash with the
//! full key stored alongside so a hash collision is a miss, never a wrong hit.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, PoisonError};

/// How many renders each cache keeps (math, diagrams).
pub(crate) const CAPACITY: usize = 512;

pub(crate) fn hash_of(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

struct Entry<K, V> {
    key: K,
    value: V,
    used: u64,
}

struct Lru<K, V> {
    entries: HashMap<u64, Entry<K, V>>,
    clock: u64,
}

/// A thread-safe LRU of at most [`CAPACITY`] entries.
pub(crate) struct Cache<K, V> {
    inner: Mutex<Option<Lru<K, V>>>,
}

impl<K: PartialEq, V: Clone> Cache<K, V> {
    pub(crate) const fn new() -> Self {
        Self { inner: Mutex::new(None) }
    }

    /// The cached value for `key` (whose hash is `hash`), marking it as used.
    pub(crate) fn get<Q>(&self, hash: u64, matches: Q) -> Option<V>
    where
        Q: FnOnce(&K) -> bool,
    {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let lru = guard.as_mut()?;
        lru.clock += 1;
        let clock = lru.clock;
        let entry = lru.entries.get_mut(&hash)?;
        if !matches(&entry.key) {
            return None;
        }
        entry.used = clock;
        Some(entry.value.clone())
    }

    /// Stores `value`, evicting the least recently used entry when full.
    pub(crate) fn insert(&self, hash: u64, key: K, value: V) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let lru = guard.get_or_insert_with(|| Lru { entries: HashMap::with_capacity(CAPACITY), clock: 0 });
        lru.clock += 1;
        if lru.entries.len() >= CAPACITY && !lru.entries.contains_key(&hash) {
            let oldest = lru.entries.iter().min_by_key(|(_, e)| e.used).map(|(h, _)| *h);
            if let Some(oldest) = oldest {
                lru.entries.remove(&oldest);
            }
        }
        lru.entries.insert(hash, Entry { key, value, used: lru.clock });
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map_or(0, |lru| lru.entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let cache: Cache<u32, u32> = Cache::new();
        for i in 0..CAPACITY as u32 {
            cache.insert(u64::from(i), i, i * 10);
        }
        // Touch 0 so 1 becomes the oldest.
        assert_eq!(cache.get(0, |k| *k == 0), Some(0));
        cache.insert(9999, 9999, 1);
        assert_eq!(cache.len(), CAPACITY);
        assert_eq!(cache.get(0, |k| *k == 0), Some(0));
        assert_eq!(cache.get(1, |k| *k == 1), None);
        assert_eq!(cache.get(9999, |k| *k == 9999), Some(1));
    }

    #[test]
    fn hash_collision_is_a_miss() {
        let cache: Cache<String, u32> = Cache::new();
        cache.insert(7, "a".into(), 1);
        assert_eq!(cache.get(7, |k| k == "b"), None);
        assert_eq!(cache.get(7, |k| k == "a"), Some(1));
    }
}
