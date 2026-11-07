// Copyright (C) 2025-2026 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::fmt::Display;

use hashbrown::HashMap;

/// Cached data and its outstanding save obligation, independent of recency.
struct Entry<K, V> {
    /// Key also stored in the cache index.
    key: K,
    /// Latest cached value.
    value: V,
    /// Whether this value still needs saving.
    dirty: bool,
}

impl<K, V: Copy> Entry<K, V> {
    /// Overwrite the value and its save obligation, including clearing it for clean inserts.
    fn overwrite(&mut self, value: V, dirty: bool) {
        self.value = value;
        self.dirty = dirty;
    }

    /// Reuse this slot, returning the displaced data only when it needs saving.
    fn replace(&mut self, key: K, value: V, dirty: bool) -> Option<(K, V)> {
        let old = std::mem::replace(self, Self { key, value, dirty });
        old.dirty.then_some((old.key, old.value))
    }
}

/// Missing neighbour; never exposed by recency operations as a slot index.
const NIL: usize = usize::MAX;

/// Compact neighbours of one cache slot.
struct Links {
    /// More recently used slot, or NIL at the head.
    prev: usize,
    /// Less recently used slot, or NIL at the tail.
    next: usize,
}

/// Recency ordering of slot indices, independent of keys, values and dirty state.
struct RecencyList {
    /// Parallel to the cache's entries; slots are appended then reused.
    links: Vec<Links>,
    /// Most recently used slot, or NIL when empty.
    head: usize,
    /// Least recently used slot, or NIL when empty.
    tail: usize,
}

impl RecencyList {
    /// Allocate ordering storage for the cache's fixed capacity.
    fn new(capacity: usize) -> Self {
        Self {
            links: Vec::with_capacity(capacity),
            head: NIL,
            tail: NIL,
        }
    }

    /// Detach an existing slot, repairing both neighbours and endpoints.
    fn detach(&mut self, slot: usize) -> Result<(), LruCacheCorrupted> {
        let Links { prev, next } = *self.links.get(slot).ok_or(LruCacheCorrupted)?;
        if slot == self.tail {
            self.tail = prev;
        } else {
            self.links.get_mut(next).ok_or(LruCacheCorrupted)?.prev = prev;
        }
        if slot == self.head {
            self.head = next;
        } else {
            self.links.get_mut(prev).ok_or(LruCacheCorrupted)?.next = next;
        }
        Ok(())
    }

    /// Link a newly appended or detached slot at the most recent end.
    fn push_front(&mut self, slot: usize) -> Result<(), LruCacheCorrupted> {
        if slot == self.links.len() {
            self.links.push(Links {
                prev: NIL,
                next: NIL,
            });
        }
        let links = self.links.get_mut(slot).ok_or(LruCacheCorrupted)?;
        links.prev = NIL;
        links.next = self.head;
        if self.head == NIL {
            self.tail = slot;
        } else {
            self.links.get_mut(self.head).ok_or(LruCacheCorrupted)?.prev = slot;
        }
        self.head = slot;
        Ok(())
    }

    /// Promote an existing slot without changing its storage position.
    fn move_to_front(&mut self, slot: usize) -> Result<(), LruCacheCorrupted> {
        if slot == self.head {
            return Ok(());
        }
        self.detach(slot)?;
        self.push_front(slot)
    }

    /// Detach the least recent slot for reuse, or return None when empty.
    fn pop_back(&mut self) -> Result<Option<usize>, LruCacheCorrupted> {
        if self.tail == NIL {
            return Ok(None);
        }
        let slot = self.tail;
        self.detach(slot)?;
        Ok(Some(slot))
    }

    /// Visit slots from most to least recent, reporting an invalid index once.
    fn iter(&self) -> impl Iterator<Item = Result<usize, LruCacheCorrupted>> + '_ {
        let mut current = self.head;
        std::iter::from_fn(move || {
            if current == NIL {
                return None;
            }
            let slot = current;
            match self.links.get(slot) {
                Some(links) => {
                    current = links.next;
                    Some(Ok(slot))
                }
                None => {
                    current = NIL;
                    Some(Err(LruCacheCorrupted))
                }
            }
        })
    }

    /// Render a missing neighbour using the cache's historical display sentinel.
    fn display_slot(slot: usize, capacity: usize) -> usize {
        if slot == NIL {
            capacity
        } else {
            slot
        }
    }
}

/// Cache bookkeeping is invalid; discard the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LruCacheCorrupted;

impl std::fmt::Display for LruCacheCorrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LRU cache is in a corrupted state")
    }
}

impl std::error::Error for LruCacheCorrupted {}

/// Failure to flush cached data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushError<E> {
    /// Cache bookkeeping is invalid.
    LruCacheCorrupted,
    /// The save callback failed.
    FlushError(E),
}

impl<E> From<E> for FlushError<E> {
    fn from(e: E) -> Self {
        FlushError::FlushError(e)
    }
}

/// LRU cache with dirty eviction reporting and slot-ordered flushing.
pub struct LruCache<K, V> {
    /// Maximum number of retained entries.
    capacity: usize,
    /// Map from keys to stable storage slots.
    index: HashMap<K, usize>,
    /// Cached data in insertion-slot order, also used for flush ordering.
    entries: Vec<Entry<K, V>>,
    /// Most-to-least recent ordering of the storage slots.
    recency: RecencyList,
}

impl<K: Display, V: Display> Display for LruCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let display = |slot| RecencyList::display_slot(slot, self.capacity);
        writeln!(
            f,
            "LruCache (capacity={}, head={}, tail={})",
            self.capacity,
            display(self.recency.head),
            display(self.recency.tail)
        )?;
        for slot in self.recency.iter() {
            let Some((entry, links)) = slot
                .ok()
                .and_then(|slot| Some((self.entries.get(slot)?, self.recency.links.get(slot)?)))
            else {
                writeln!(f, "  <invalid>")?;
                break;
            };
            writeln!(
                f,
                "  {}={} ({}) [prev={}, next={}]",
                entry.key,
                entry.value,
                if entry.dirty { "dirty" } else { "clean" },
                display(links.prev),
                display(links.next)
            )?;
        }
        Ok(())
    }
}

impl<K: Eq + std::hash::Hash + Clone, V: Copy> LruCache<K, V> {
    /// Create a new LRU cache with the given capacity (> 0)
    pub fn new(mut capacity: usize) -> Self {
        if capacity == 0 {
            error!("Capacity must be greater than 0. Defaulting to 1024.");
            capacity = 1024;
        }

        LruCache {
            capacity,
            index: HashMap::new(),
            entries: Vec::with_capacity(capacity),
            recency: RecencyList::new(capacity),
        }
    }

    /// Get the value for the given key
    /// Returns an error iff the cache is corrupted and should be discarded
    pub fn get(&mut self, key: &K) -> Result<Option<V>, LruCacheCorrupted> {
        if let Some(&index) = self.index.get(key) {
            self.recency.move_to_front(index)?;
            let node = self.entries.get(index).ok_or(LruCacheCorrupted)?;
            Ok(Some(node.value))
        } else {
            Ok(None)
        }
    }

    /// Insert a key-value pair into the cache, marking it as dirty.
    /// Returns an error iff the cache is corrupted and should be discarded
    /// Returns `Ok(Some((K, V)))` if a dirty value was evicted.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<(K, V)>, LruCacheCorrupted> {
        self.insert_with_dirty(key, value, true)
    }

    /// Insert a key-value pair into the cache, marking it as clean.
    /// Returns an error iff the cache is corrupted and should be discarded
    /// Returns `Ok(Some((K, V)))` if a dirty value was evicted.
    pub fn insert_clean(&mut self, key: K, value: V) -> Result<Option<(K, V)>, LruCacheCorrupted> {
        self.insert_with_dirty(key, value, false)
    }

    /// Insert a key-value pair into the cache
    /// Returns an error iff the cache is corrupted and should be discarded
    /// Returns `Ok(Some((K, V)))` if a dirty value was evicted.
    pub fn insert_with_dirty(
        &mut self,
        key: K,
        value: V,
        dirty: bool,
    ) -> Result<Option<(K, V)>, LruCacheCorrupted> {
        if let Some(&slot) = self.index.get(&key) {
            self.entries
                .get_mut(slot)
                .ok_or(LruCacheCorrupted)?
                .overwrite(value, dirty);
            self.recency.move_to_front(slot)?;
            return Ok(None);
        }

        let (slot, evicted) = if self.index.len() == self.capacity {
            let slot = self.recency.pop_back()?.ok_or(LruCacheCorrupted)?;
            let entry = self.entries.get_mut(slot).ok_or(LruCacheCorrupted)?;
            self.index.remove(&entry.key);
            (slot, entry.replace(key.clone(), value, dirty))
        } else {
            let slot = self.entries.len();
            self.entries.push(Entry {
                key: key.clone(),
                value,
                dirty,
            });
            (slot, None)
        };
        self.index.insert(key, slot);
        self.recency.push_front(slot)?;
        Ok(evicted)
    }

    /// Flush all dirty values in the cache, calling the given function, `f`,
    /// for each dirty value.
    /// Visits storage slots in order, marking each entry clean only after success.
    /// Stops at the first callback error; the failed and later entries remain dirty.
    pub fn flush<E>(
        &mut self,
        mut f: impl FnMut(&K, V) -> Result<(), E>,
    ) -> Result<(), FlushError<E>> {
        for node in self.entries.iter_mut().filter(|n| n.dirty) {
            f(&node.key, node.value)?;
            node.dirty = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_cache() {
        let mut cache = LruCache::new(2);

        cache.insert(1, 1).expect("cache corrupted");
        cache.insert(2, 2).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), Some(1));
        cache.insert(3, 3).expect("cache corrupted");
        assert_eq!(cache.get(&2).expect("cache corrupted"), None);
        cache.insert(4, 4).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), None);
        assert_eq!(cache.get(&3).expect("cache corrupted"), Some(3));
        assert_eq!(cache.get(&4).expect("cache corrupted"), Some(4));
    }

    #[test]
    fn test_lru_cache_update() {
        let mut cache = LruCache::new(2);

        cache.insert(1, 1).expect("cache corrupted");
        cache.insert(2, 2).expect("cache corrupted");
        cache.insert(1, 10).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), Some(10));
        cache.insert(3, 3).expect("cache corrupted");
        assert_eq!(cache.get(&2).expect("cache corrupted"), None);
        cache.insert(2, 4).expect("cache corrupted");
        assert_eq!(cache.get(&2).expect("cache corrupted"), Some(4));
        assert_eq!(cache.get(&3).expect("cache corrupted"), Some(3));
    }

    #[test]
    fn test_lru_cache_evicted() {
        let mut cache = LruCache::new(2);

        assert!(cache.insert(1, 1).expect("cache corrupted").is_none());
        assert!(cache.insert(2, 2).expect("cache corrupted").is_none());
        let evicted = cache
            .insert(3, 3)
            .expect("cache corrupted")
            .expect("expected an eviction");
        assert_eq!(evicted, (1, 1));
    }

    #[test]
    fn test_lru_cache_flush() {
        let mut cache = LruCache::new(2);

        cache.insert(1, 1).expect("cache corrupted");

        let mut flushed = Vec::new();
        cache
            .flush(|k, v| {
                flushed.push((*k, v));
                Ok::<(), ()>(())
            })
            .expect("cache corrupted or flush failed");

        assert_eq!(flushed, vec![(1, 1)]);

        cache.insert(1, 3).expect("cache corrupted");
        cache.insert(2, 2).expect("cache corrupted");

        let mut flushed = Vec::new();
        cache
            .flush(|k, v| {
                flushed.push((*k, v));
                Ok::<(), ()>(())
            })
            .expect("cache corrupted or flush failed");

        flushed.sort();
        assert_eq!(flushed, vec![(1, 3), (2, 2)]);
    }

    #[test]
    fn test_lru_cache_evict_clean() {
        let mut cache = LruCache::new(2);

        assert!(cache
            .insert_with_dirty(0, 0, false)
            .expect("cache corrupted")
            .is_none());
        assert!(cache
            .insert_with_dirty(1, 1, false)
            .expect("cache corrupted")
            .is_none());
        assert!(cache
            .insert_with_dirty(2, 2, true)
            .expect("cache corrupted")
            .is_none());
        assert!(cache
            .insert_with_dirty(3, 3, true)
            .expect("cache corrupted")
            .is_none());

        let mut flushed = Vec::new();
        cache
            .flush(|k, v| {
                flushed.push((*k, v));
                Ok::<(), ()>(())
            })
            .expect("cache corrupted or flush failed");

        flushed.sort();
        assert_eq!(flushed, [(2, 2), (3, 3)]);
    }

    #[test]
    fn test_lru_cache_capacity_one() {
        let mut cache = LruCache::new(1);

        cache.insert(1, 1).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), Some(1));

        cache.insert(2, 2).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), None);
        assert_eq!(cache.get(&2).expect("cache corrupted"), Some(2));
    }

    #[test]
    fn test_lru_cache_capacity_one_update() {
        let mut cache = LruCache::new(1);

        cache.insert(1, 1).expect("cache corrupted");
        cache.insert(1, 2).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), Some(2));

        cache.insert(2, 3).expect("cache corrupted");
        assert_eq!(cache.get(&1).expect("cache corrupted"), None);
        assert_eq!(cache.get(&2).expect("cache corrupted"), Some(3));
    }

    #[test]
    fn test_lru_cache_capacity_one_eviction() {
        let mut cache = LruCache::new(1);

        assert!(cache.insert(1, 1).expect("cache corrupted").is_none());
        let evicted = cache
            .insert(2, 2)
            .expect("cache corrupted")
            .expect("expected eviction");
        assert_eq!(evicted, (1, 1));
    }

    #[test]
    fn test_lru_cache_capacity_one_flush() {
        let mut cache = LruCache::new(1);

        cache.insert(1, 1).expect("cache corrupted");

        let mut flushed = Vec::new();
        cache
            .flush(|k, v| {
                flushed.push((*k, v));
                Ok::<(), ()>(())
            })
            .expect("cache corrupted or flush failed");

        assert_eq!(flushed, vec![(1, 1)]);

        cache.insert(2, 2).expect("cache corrupted");

        let mut flushed = Vec::new();
        cache
            .flush(|k, v| {
                flushed.push((*k, v));
                Ok::<(), ()>(())
            })
            .expect("cache corrupted or flush failed");

        assert_eq!(flushed, vec![(2, 2)]);
    }

    #[test]
    fn pins_flush_order_and_failure() {
        let mut cache = LruCache::new(3);
        for k in 1..=3 {
            cache.insert(k, k).unwrap();
        }
        cache.get(&1).unwrap();
        assert_eq!(cache.insert(4, 4).unwrap(), Some((2, 2)));
        let mut calls = vec![];
        assert_eq!(
            cache.flush(|k, _| {
                calls.push(*k);
                if calls.len() == 2 {
                    Err(())
                } else {
                    Ok(())
                }
            }),
            Err(FlushError::FlushError(()))
        );
        assert_eq!(calls, [1, 4]);
        let mut remaining = vec![];
        cache
            .flush(|k, v| {
                remaining.push((*k, v));
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(remaining, [(4, 4), (3, 3)]);
    }

    #[test]
    fn pins_clean_overwrite_and_latest_eviction() {
        let mut cache = LruCache::new(1);
        cache.insert(1, 1).unwrap();
        cache.insert_clean(1, 2).unwrap();
        cache
            .flush(|_, _| -> Result<(), ()> { panic!("clean entry saved") })
            .unwrap();
        assert_eq!(cache.insert(2, 3).unwrap(), None);
        cache.insert(2, 4).unwrap();
        assert_eq!(cache.insert_clean(3, 5).unwrap(), Some((2, 4)));
        assert_eq!(cache.insert(4, 6).unwrap(), None);
    }

    #[test]
    fn pins_zero_capacity_and_display() {
        let mut fallback = LruCache::new(0);
        for k in 0..1024 {
            assert_eq!(fallback.insert(k, k).unwrap(), None);
        }
        assert_eq!(fallback.insert(1024, 1024).unwrap(), Some((0, 0)));
        let mut cache = LruCache::new(2);
        assert_eq!(cache.to_string(), "LruCache (capacity=2, head=2, tail=2)\n");
        cache.insert(1, 10).unwrap();
        cache.insert_clean(2, 20).unwrap();
        assert_eq!(cache.to_string(), "LruCache (capacity=2, head=1, tail=0)\n  2=20 (clean) [prev=2, next=0]\n  1=10 (dirty) [prev=1, next=2]\n");
    }

    #[test]
    fn compact_links_layout() {
        assert_eq!(
            std::mem::size_of::<Links>(),
            2 * std::mem::size_of::<usize>()
        );
    }

    /// Independent reference entry, deliberately unrelated to production storage.
    struct ModelEntry {
        /// Model key.
        key: u32,
        /// Latest model value.
        value: u32,
        /// Outstanding model save obligation.
        dirty: bool,
    }

    /// Simple vector reference model, oldest entry first.
    pub struct SimpleLRU {
        /// Entries from least to most recent.
        cache: Vec<ModelEntry>,
        /// Maximum retained entries.
        capacity: usize,
    }

    impl SimpleLRU {
        /// Start an empty model with the given positive capacity.
        pub fn new(capacity: usize) -> Self {
            Self {
                cache: Vec::new(),
                capacity,
            }
        }

        /// Promote or append an entry, returning only dirty evictions.
        pub fn insert(&mut self, key: u32, value: u32, dirty: bool) -> Option<(u32, u32)> {
            let mut evicted = None;
            if let Some(pos) = self.cache.iter().position(|x| x.key == key) {
                self.cache.remove(pos);
            } else if self.cache.len() == self.capacity {
                let old = self.cache.remove(0);
                if old.dirty {
                    evicted = Some((old.key, old.value));
                }
            }
            self.cache.push(ModelEntry { key, value, dirty });
            evicted
        }

        /// Read and promote a matching entry.
        pub fn get(&mut self, key: u32) -> Option<u32> {
            let pos = self.cache.iter().position(|x| x.key == key)?;
            let entry = self.cache.remove(pos);
            let value = entry.value;
            self.cache.push(entry);
            Some(value)
        }

        /// Mirror exactly the keys successfully saved by the cache.
        pub fn flush_keys(&mut self, keys: &[u32]) {
            for entry in &mut self.cache {
                if keys.contains(&entry.key) {
                    entry.dirty = false;
                }
            }
        }

        /// Snapshot the complete expected recency and dirty state.
        pub fn entries_mru_to_lru(&self) -> Vec<(u32, u32, bool)> {
            self.cache
                .iter()
                .rev()
                .map(|e| (e.key, e.value, e.dirty))
                .collect()
        }

        /// Return the outstanding saves without imposing a flush order.
        pub fn dirty_pairs(&self) -> Vec<(u32, u32)> {
            let mut pairs: Vec<_> = self
                .cache
                .iter()
                .filter(|e| e.dirty)
                .map(|e| (e.key, e.value))
                .collect();
            pairs.sort();
            pairs
        }
    }
}

#[cfg(test)]
mod property_tests {
    use pinny::tag;
    use proptest::prelude::*;

    use super::tests::SimpleLRU;
    use super::*;

    #[derive(Debug, Clone)]
    enum CacheOp {
        Insert(u32, u32),
        Get(u32),
        InsertClean(u32, u32),
        Flush,
        FlushFailingAfter(usize),
    }

    prop_compose! {
        fn arbitrary_op()(op_type in 0..5, key in 0..8u32, value in 0..1000u32, n in 0..11usize) -> CacheOp {
            match op_type {
                0 => CacheOp::Insert(key, value),
                1 => CacheOp::Get(key),
                2 => CacheOp::InsertClean(key, value),
                3 => CacheOp::Flush,
                _ => CacheOp::FlushFailingAfter(n),
            }
        }
    }

    /// Include singleton caches, small eviction-heavy caches and the original size ten.
    fn capacity() -> impl Strategy<Value = usize> {
        prop_oneof![1..=5usize, Just(10)]
    }

    /// Compare successful saves with the model, then mirror only those saves.
    fn flush(cache: &mut LruCache<u32, u32>, model: &mut SimpleLRU, limit: usize) {
        let expected = model.dirty_pairs();
        let mut saved = vec![];
        let result = cache.flush(|k, v| {
            if saved.len() == limit {
                return Err(());
            }
            saved.push((*k, v));
            Ok(())
        });
        assert_eq!(
            result,
            if expected.len() > limit {
                Err(FlushError::FlushError(()))
            } else {
                Ok(())
            }
        );
        for pair in &saved {
            assert!(expected.contains(pair));
        }
        if result.is_ok() {
            let mut sorted = saved.clone();
            sorted.sort();
            assert_eq!(sorted, expected);
        }
        model.flush_keys(&saved.iter().map(|(k, _)| *k).collect::<Vec<_>>());
    }

    /// Compare operation results and full state, including before failed-flush recovery.
    fn apply(cache: &mut LruCache<u32, u32>, model: &mut SimpleLRU, op: CacheOp) {
        match op {
            CacheOp::Insert(k, v) => {
                assert_eq!(cache.insert(k, v).unwrap(), model.insert(k, v, true))
            }
            CacheOp::InsertClean(k, v) => {
                assert_eq!(cache.insert_clean(k, v).unwrap(), model.insert(k, v, false))
            }
            CacheOp::Get(k) => assert_eq!(cache.get(&k).unwrap(), model.get(k)),
            CacheOp::Flush => flush(cache, model, usize::MAX),
            CacheOp::FlushFailingAfter(n) => {
                flush(cache, model, n);
                assert_eq!(entries(cache), model.entries_mru_to_lru());
                // Recovery must save exactly the outstanding set after partial success.
                flush(cache, model, usize::MAX);
            }
        }
        assert_eq!(entries(cache), model.entries_mru_to_lru());
    }

    /// Snapshot production state with a traversal bound to detect cycles.
    fn entries(cache: &LruCache<u32, u32>) -> Vec<(u32, u32, bool)> {
        cache
            .recency
            .iter()
            .take(cache.entries.len() + 1)
            .map(|slot| {
                let entry = &cache.entries[slot.unwrap()];
                (entry.key, entry.value, entry.dirty)
            })
            .collect()
    }

    proptest! {
        #[tag(t_prop)]
        #[test]
        fn doesnt_crash_with_random_operations(cap in capacity(), ops in prop::collection::vec(arbitrary_op(), 1..1000)) {
            let mut cache = LruCache::new(cap);
            let mut model = SimpleLRU::new(cap);
            for op in ops { apply(&mut cache, &mut model, op); }
        }

        #[tag(t_prop)]
        #[test]
        fn maintains_size_invariant(cap in capacity(), ops in prop::collection::vec(arbitrary_op(), 1..1000)) {
            let mut cache = LruCache::new(cap);
            let mut model = SimpleLRU::new(cap);
            for op in ops {
                apply(&mut cache, &mut model, op);
                prop_assert!(cache.index.len() <= cap);
                prop_assert_eq!(cache.index.len(), cache.entries.len());
                prop_assert_eq!(cache.entries.len(), cache.recency.links.len());
            }
        }

        #[tag(t_prop)]
        #[test]
        fn maintains_linked_list_integrity(cap in capacity(), ops in prop::collection::vec(arbitrary_op(), 1..1000)) {
            let mut cache = LruCache::new(cap);
            let mut model = SimpleLRU::new(cap);
            for op in ops {
                apply(&mut cache, &mut model, op);
                let mut prev = NIL;
                let mut count = 0;
                for slot in cache.recency.iter().take(cache.entries.len() + 1) {
                    let slot = slot.unwrap();
                    prop_assert_eq!(cache.recency.links[slot].prev, prev);
                    prop_assert_eq!(cache.index.get(&cache.entries[slot].key), Some(&slot));
                    prev = slot;
                    count += 1;
                }
                prop_assert_eq!(count, cache.entries.len());
                prop_assert_eq!(prev, cache.recency.tail);
            }
        }

        #[tag(t_prop)]
        #[test]
        fn maintains_lru_correctness(cap in capacity(), ops in prop::collection::vec(arbitrary_op(), 1..1000)) {
            let mut cache = LruCache::new(cap);
            let mut model = SimpleLRU::new(cap);
            for op in ops {
                apply(&mut cache, &mut model, op);
                prop_assert_eq!(entries(&cache), model.entries_mru_to_lru());
            }
        }
    }
}
