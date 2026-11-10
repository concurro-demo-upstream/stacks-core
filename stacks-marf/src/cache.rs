// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2026 Stacks Open Internet Foundation
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

use std::collections::HashMap;

use crate::MarfTrieId;

/// Default maximum number of block-identity pairs held by a [`BlockHashCache`].  Each cached pair
/// costs on the order of 150 bytes (two hash map entries plus a ring slot), so this bounds a
/// cache to roughly 10 MB.  It is sized so that the block IDs reached by reads over a large
/// window of recent blocks (including skip-list back-pointer targets) stay resident, since a
/// miss costs one indexed SQLite query.
pub const BLOCK_HASH_CACHE_CAPACITY: usize = 65_536;

/// Cache of MARF block hash/block ID lookups, with a bounded size.
///
/// Each cached entry is a pair (block ID, block hash).  Both `by_id` and `by_hash` hold exactly
/// the same set of pairs, and every slot of `ring` names an ID that is in `by_id`; pairs are
/// inserted and evicted in lock-step.
///
/// Eviction uses the CLOCK (second-chance) policy.  Looking an entry up via
/// `get_block_hash_caching` or `load_block_id` sets its referenced bit.  When the cache is full
/// and a new pair must be inserted, `hand` sweeps the ring, clearing referenced bits as it goes,
/// and evicts the first entry whose bit is clear.  Consequently, as long as the set of distinct
/// IDs that is being read fits within the capacity, nothing is ever evicted and all repeated
/// reads hit; scans over more than `capacity` IDs miss, as with any bounded cache.
///
/// Hits are O(1).  Inserts are amortized O(1), but a single insert can sweep up to `capacity`
/// slots if every entry is referenced.  A miss is never incorrect, since the cache only
/// memoizes the `marf_data` table; it costs one database query.
pub struct BlockHashCache<T: MarfTrieId> {
    /// Mapping between trie blob IDs (i.e. rowids) and the MarfTrieId of the trie, along with the
    /// index of the entry's slot in `ring`.
    by_id: HashMap<u32, (T, usize)>,

    /// Mapping between trie blob hashes and their IDs
    by_hash: HashMap<T, u32>,

    /// Eviction ring.  Each slot holds a cached block ID and its referenced bit.  Grows up to
    /// `capacity` slots, after which slots are reused.
    ring: Vec<(u32, bool)>,

    /// Next slot of `ring` that the CLOCK sweep will examine
    hand: usize,

    /// Maximum number of pairs in the cache.  Always at least 1.
    capacity: usize,

    /// Number of pairs inserted, i.e. of lookups that were not served from the cache and that
    /// found a row.  Failed lookups (such as the sentinel block, which has no row) are not
    /// cached and not counted.  Tests and benchmarks only.
    #[cfg(test)]
    misses: u64,
}

impl<T: MarfTrieId> BlockHashCache<T> {
    /// Make a cache with the default capacity, `BLOCK_HASH_CACHE_CAPACITY`.
    pub fn new() -> BlockHashCache<T> {
        Self::with_capacity(BLOCK_HASH_CACHE_CAPACITY)
    }

    /// Make a cache that holds at most `capacity` pairs (at least 1).  Memory is not
    /// preallocated.
    pub fn with_capacity(capacity: usize) -> BlockHashCache<T> {
        BlockHashCache {
            by_id: HashMap::new(),
            by_hash: HashMap::new(),
            ring: Vec::new(),
            hand: 0,
            capacity: capacity.max(1),
            #[cfg(test)]
            misses: 0,
        }
    }

    /// Insert a pair whose ID is not yet cached, evicting another pair if the cache is full.
    fn insert(&mut self, block_id: u32, block_hash: T) {
        #[cfg(test)]
        {
            self.misses += 1;
        }
        let slot = if self.ring.len() < self.capacity {
            self.ring.push((block_id, false));
            self.ring.len() - 1
        } else {
            let slot = self.evict();
            self.ring[slot] = (block_id, false);
            slot
        };
        self.by_hash.insert(block_hash.clone(), block_id);
        self.by_id.insert(block_id, (block_hash, slot));
    }

    /// Run the CLOCK sweep on a full ring: evict the first unreferenced entry and return its
    /// (now free) slot.  The reverse mapping is removed only if it still names the evicted ID, so
    /// that a hash cached under another ID is not dropped.
    fn evict(&mut self) -> usize {
        loop {
            let slot = self.hand;
            self.hand = (self.hand + 1) % self.ring.len();
            let (block_id, referenced) = &mut self.ring[slot];
            if *referenced {
                *referenced = false;
                continue;
            }
            let block_id = *block_id;
            if let Some((block_hash, _)) = self.by_id.remove(&block_id) {
                if self.by_hash.get(&block_hash) == Some(&block_id) {
                    self.by_hash.remove(&block_hash);
                }
            }
            return slot;
        }
    }

    /// Get cached entry for a block hash, given its ID, or, if not
    ///  found, use `lookup` to get the corresponding block hash and
    ///  store it in the cache.  Errors from `lookup` are returned and nothing is cached.
    pub fn get_block_hash_caching<E, F: FnOnce(u32) -> Result<T, E>>(
        &mut self,
        id: u32,
        lookup: F,
    ) -> Result<&T, E> {
        if let Some(&(_, slot)) = self.by_id.get(&id) {
            self.ring[slot].1 = true;
        } else {
            let block_hash = lookup(id)?;
            self.insert(id, block_hash);
        }
        Ok(&self.by_id[&id].0)
    }

    /// Cache a block hash, given its ID.  `block_id` must not be cached already.
    pub fn store_block_hash(&mut self, block_id: u32, block_hash: T) {
        assert!(!self.by_id.contains_key(&block_id));
        self.insert(block_id, block_hash);
    }

    /// Get an immutable reference to a block hash, given the ID.  Does not count as a use for
    /// eviction purposes.
    pub fn ref_block_hash(&self, block_id: u32) -> Option<&T> {
        self.by_id.get(&block_id).map(|(block_hash, _)| block_hash)
    }

    /// Get the block ID, given its hash.  A hit counts as a use for eviction purposes.
    pub fn load_block_id(&mut self, block_hash: &T) -> Option<u32> {
        let block_id = *self.by_hash.get(block_hash)?;
        if let Some(&(_, slot)) = self.by_id.get(&block_id) {
            self.ring[slot].1 = true;
        }
        Some(block_id)
    }

    /// Number of cached pairs
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the cache holds no pairs
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Number of pairs inserted since this cache was made (see `misses`)
    #[cfg(test)]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Check that the maps and the ring describe the same set of pairs, and that the capacity is
    /// respected.  Panics otherwise.
    #[cfg(test)]
    pub fn assert_consistent(&self) {
        assert!(self.ring.len() <= self.capacity);
        assert_eq!(self.ring.len(), self.by_id.len());
        assert_eq!(self.by_hash.len(), self.by_id.len());
        for (slot, (block_id, _)) in self.ring.iter().enumerate() {
            let (block_hash, ring_slot) = self.by_id.get(block_id).expect("ring id not in by_id");
            assert_eq!(*ring_slot, slot);
            assert_eq!(self.by_hash.get(block_hash), Some(block_id));
        }
    }
}
