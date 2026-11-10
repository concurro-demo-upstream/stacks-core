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

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::mem::size_of;

use stacks_common::types::chainstate::TrieHash;
use stacks_common::util::lru_cache::LruCache;

use crate::node::{TrieCowPtr, TrieNode256, TrieNodePatch, TrieNodeType, TriePtr};
use crate::{MarfTrieId, MAX_PATCH_DEPTH};

/// Cache MARF block hash/block ID lookups.
pub struct BlockHashCache<T: MarfTrieId> {
    /// Mapping between trie blob IDs (i.e. rowids) and the MarfTrieId of the trie.  Contents are
    /// never evicted, since the size of this map grows only at the rate of new Stacks blocks.
    block_hash_cache: HashMap<u32, T>,

    /// Mapping between trie blob hashes and their IDs
    block_id_cache: HashMap<T, u32>,
}

impl<T: MarfTrieId> BlockHashCache<T> {
    pub fn new() -> BlockHashCache<T> {
        BlockHashCache {
            block_hash_cache: HashMap::new(),
            block_id_cache: HashMap::new(),
        }
    }

    /// Get cached entry for a block hash, given its ID, or, if not
    ///  found, use `lookup` to get the corresponding block hash and
    ///  store it in the cache
    pub fn get_block_hash_caching<E, F: FnOnce(u32) -> Result<T, E>>(
        &mut self,
        id: u32,
        lookup: F,
    ) -> Result<&T, E> {
        match self.block_hash_cache.entry(id) {
            Entry::Occupied(occupied_entry) => Ok(occupied_entry.into_mut()),
            Entry::Vacant(vacant_entry) => {
                let block_hash = lookup(id)?;
                let block_hash_ref = vacant_entry.insert(block_hash.clone());
                self.block_id_cache.insert(block_hash, id);
                Ok(block_hash_ref)
            }
        }
    }

    /// Cache a block hash, given its ID
    pub fn store_block_hash(&mut self, block_id: u32, block_hash: T) {
        assert!(!self.block_hash_cache.contains_key(&block_id));
        self.block_id_cache.insert(block_hash.clone(), block_id);
        self.block_hash_cache.insert(block_id, block_hash);
    }

    /// Get an immutable reference to a block hash, given the ID
    pub fn ref_block_hash(&self, block_id: u32) -> Option<&T> {
        self.block_hash_cache.get(&block_id)
    }

    /// Get the block ID, given its hash
    pub fn load_block_id(&self, block_hash: &T) -> Option<u32> {
        self.block_id_cache.get(block_hash).copied()
    }
}

/// Memory budget, in bytes, for one [`PatchedNodeCache`].
///
/// The cache holds fully reconstructed nodes, whose size varies by orders of magnitude (a patched
/// `Node4` is a few hundred bytes; a patched `Node256` with retained patch diffs is ~20 KiB).  The
/// entry count is therefore derived from this budget and the *worst-case* entry size (see
/// [`PatchedNodeCache::CAPACITY`]), rather than guessed, so the budget is never exceeded.  16 MiB
/// is small next to the MARF's SQLite page cache and mmap, but large enough to hold the upper
/// levels of the trie along hot lookup paths.
pub const PATCHED_NODE_CACHE_BYTE_BUDGET: usize = 16 * 1024 * 1024;

/// Longest possible node path: its length is encoded in a single byte.
const MAX_NODE_PATH_BYTES: usize = 255;

/// Number of pointer slots in a patch diff, at most (see `TrieNodePatch::consensus_serialize`).
const MAX_PATCH_DIFF_PTRS: usize = 256;

/// Identifies one reconstructed node.  Both the block hash and the block ID (SQLite rowid) are
/// included: committed rows are immutable, but rowids can be reused after a rollback, so the
/// block hash keeps a reused rowid for a different block from ever matching.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PatchedNodeKey<T: MarfTrieId> {
    /// Hash of the trie the node was read from
    pub block_hash: T,
    /// Row ID of that trie in `marf_data`
    pub block_id: u32,
    /// Node type ID of the pointer that was read, with the backptr flag cleared
    pub node_id: u8,
    /// Offset of the node within the trie
    pub ptr: u64,
}

/// What is stored for each [`PatchedNodeKey`]: the reconstructed node, and its hash if one was
/// ever read along with it.  A read that did not request the hash stores `None`, never the zero
/// placeholder hash, so such an entry cannot serve a later read that needs the real hash.
type CachedNode = (TrieNodeType, Option<TrieHash>);

/// Counters describing [`PatchedNodeCache`] activity.  Reading them via
/// `PatchedNodeCache::take_stats` resets them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PatchedNodeCacheStats {
    /// Lookups served from the cache
    pub hits: u64,
    /// Lookups that were not served from the cache (including ones needing an absent hash)
    pub misses: u64,
    /// Nodes stored in the cache
    pub inserts: u64,
    /// Nodes rebuilt by following a patch chain, whether or not the result was cached
    pub reconstructions: u64,
}

/// Bounded, in-memory LRU cache of committed MARF nodes that were reconstructed from a patch
/// chain (a base node plus one or more `TrieNodePatch`es).  Reading such a node costs several
/// reads and an `apply_patches`; repeated lookups through shared parts of the trie hit the same
/// nodes again and again.
///
/// This is purely an in-memory memo of the storage read path.  It never changes what is written
/// to disk, and a lookup returns exactly what the uncached read would have returned.  That
/// depends on only ever caching committed, confirmed rows (see `TrieStorageConnection`'s read
/// path) and on dropping the whole cache whenever the rows it was filled from might not survive:
/// a rolled-back or abandoned transaction ([`Self::begin_tx`], [`Self::clear_if_tx_abandoned`]),
/// `format`, or a squash rewrite.  Entries are cloned in and out, so callers can freely mutate
/// the nodes they receive.
pub struct PatchedNodeCache<T: MarfTrieId> {
    /// The entries.  Allocated on first insert so that storages that never read a patched node
    /// (and read-only connections that are opened often) pay nothing.
    lru: Option<LruCache<PatchedNodeKey<T>, CachedNode>>,
    /// Set while an SQL transaction opened via `transaction()` has not been committed.  Nodes
    /// read inside it may come from rows that this transaction wrote, which a rollback undoes.
    tx_pending: bool,
    stats: PatchedNodeCacheStats,
    /// Maximum number of entries
    #[cfg(test)]
    capacity: usize,
    /// Whether the cache is in use at all
    #[cfg(test)]
    enabled: bool,
}

impl<T: MarfTrieId> Default for PatchedNodeCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: MarfTrieId> PatchedNodeCache<T> {
    /// Upper bound on the heap and inline bytes one cache entry can occupy.
    ///
    /// Counts the entry's slot in the LRU's node list (key, value, links), its slot in the LRU's
    /// hash map (the key is stored a second time; the factor of 3 covers hashbrown's control
    /// bytes and a table that is up to ~2.3x larger than the entry count), and the worst-case
    /// heap behind the value: a boxed `Node256`, a maximum-length path, and `MAX_PATCH_DEPTH`
    /// retained patches that each carry a full diff.  That comes to roughly 22 KiB.
    pub const WORST_CASE_ENTRY_BYTES: usize = {
        let key = size_of::<PatchedNodeKey<T>>();
        let lru_node = key + size_of::<CachedNode>() + 3 * size_of::<usize>();
        let map_slot = 3 * (key + size_of::<usize>() + 1);
        let node256_heap = size_of::<TrieNode256>();
        let path_heap = MAX_NODE_PATH_BYTES;
        let patch_heap = MAX_PATCH_DEPTH as usize
            * (size_of::<(u32, TriePtr, TrieNodePatch)>()
                + MAX_PATCH_DIFF_PTRS * size_of::<TriePtr>());
        let cowptr_heap = size_of::<TrieCowPtr>();
        lru_node + map_slot + node256_heap + path_heap + patch_heap + cowptr_heap
    };

    /// Maximum number of entries: the byte budget divided by the worst-case entry size, so the
    /// cache stays within [`PATCHED_NODE_CACHE_BYTE_BUDGET`] whatever it holds.  Typical entries
    /// (small nodes, short patches) are far smaller than the worst case.
    pub const CAPACITY: usize = PATCHED_NODE_CACHE_BYTE_BUDGET / Self::WORST_CASE_ENTRY_BYTES;

    pub fn new() -> PatchedNodeCache<T> {
        PatchedNodeCache {
            lru: None,
            tx_pending: false,
            stats: PatchedNodeCacheStats::default(),
            #[cfg(test)]
            capacity: Self::CAPACITY,
            #[cfg(test)]
            enabled: true,
        }
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.capacity
    }

    #[cfg(not(test))]
    fn capacity(&self) -> usize {
        Self::CAPACITY
    }

    #[cfg(test)]
    fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(not(test))]
    fn is_enabled(&self) -> bool {
        true
    }

    /// Test-only: use a different maximum entry count, dropping current entries.
    #[cfg(test)]
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.lru = None;
    }

    /// Test-only: turn the cache on or off, dropping current entries.
    #[cfg(test)]
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.lru = None;
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.lru.as_ref().map_or(0, |lru| lru.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a node.  If `read_hash` is false the returned hash is the all-zero placeholder, as
    /// for an uncached `read_hash = false` read.  If `read_hash` is true, only an entry that holds
    /// the real hash counts as a hit.  A corrupted LRU is discarded and reported as a miss.
    pub fn get(
        &mut self,
        key: &PatchedNodeKey<T>,
        read_hash: bool,
    ) -> Option<(TrieNodeType, TrieHash)> {
        if !self.is_enabled() {
            return None;
        }
        let found = match self.lru.as_mut().map(|lru| lru.get(key)) {
            Some(Ok(found)) => found,
            Some(Err(_)) => {
                self.clear();
                None
            }
            None => None,
        };
        let hit = match (found, read_hash) {
            // a no-hash read always gets the placeholder, even if a real hash is stored
            (Some((node, _)), false) => Some((node, TrieHash([0u8; 32]))),
            (Some((node, Some(hash))), true) => Some((node, hash)),
            _ => None,
        };
        if hit.is_some() {
            self.stats.hits += 1;
        } else {
            self.stats.misses += 1;
        }
        hit
    }

    /// Store a node, replacing any entry for `key` (e.g. to add the hash to one that had none).
    /// Pass `hash = None` if the read that produced `node` did not read the hash.
    pub fn insert(&mut self, key: PatchedNodeKey<T>, node: &TrieNodeType, hash: Option<TrieHash>) {
        if !self.is_enabled() {
            return;
        }
        let capacity = self.capacity();
        let lru = self.lru.get_or_insert_with(|| LruCache::new(capacity));
        // entries are never dirty, so an eviction is never reported back
        if lru.insert_clean(key, (node.clone(), hash)).is_err() {
            self.clear();
            return;
        }
        self.stats.inserts += 1;
    }

    /// Record that a node was rebuilt from a patch chain.
    pub fn note_reconstruction(&mut self) {
        self.stats.reconstructions += 1;
    }

    /// Drop all entries.
    pub fn clear(&mut self) {
        self.lru = None;
    }

    /// Note that an SQL transaction has begun.
    pub fn begin_tx(&mut self) {
        self.tx_pending = true;
    }

    /// Note that the SQL transaction is being committed, so what it wrote is now permanent.
    pub fn mark_committed(&mut self) {
        self.tx_pending = false;
    }

    /// Note that the SQL transaction is being rolled back: drop all entries, since they may have
    /// been read from rows the transaction wrote.
    pub fn abort_tx(&mut self) {
        self.clear();
        self.tx_pending = false;
    }

    /// If a transaction began and was never committed (it was rolled back or simply dropped),
    /// rows it wrote may be gone, along with any nodes cached from them: drop everything.
    pub fn clear_if_tx_abandoned(&mut self) {
        if self.tx_pending {
            self.abort_tx();
        }
    }

    /// Return the counters and reset them.
    pub fn take_stats(&mut self) -> PatchedNodeCacheStats {
        std::mem::take(&mut self.stats)
    }
}

#[cfg(test)]
mod tests {
    use stacks_common::types::chainstate::StacksBlockId;

    use super::*;
    use crate::node::{TrieNode4, TrieNodeID};

    fn key(ptr: u64) -> PatchedNodeKey<StacksBlockId> {
        PatchedNodeKey {
            block_hash: StacksBlockId([1; 32]),
            block_id: 7,
            node_id: 2,
            ptr,
        }
    }

    fn node(path_byte: u8) -> TrieNodeType {
        TrieNodeType::Node4(TrieNode4::new(&[path_byte]))
    }

    fn real_hash() -> TrieHash {
        TrieHash([0xab; 32])
    }

    /// The entry count must keep the cache within its byte budget, and the per-entry worst case
    /// must really be an upper bound: build the largest value we believe can be cached and
    /// compare.
    #[test]
    fn capacity_fits_byte_budget_and_worst_case_is_an_upper_bound() {
        const { assert!(PatchedNodeCache::<StacksBlockId>::CAPACITY > 0) };
        const {
            assert!(
                PatchedNodeCache::<StacksBlockId>::CAPACITY
                    * PatchedNodeCache::<StacksBlockId>::WORST_CASE_ENTRY_BYTES
                    <= PATCHED_NODE_CACHE_BYTE_BUDGET
            )
        };

        // a patched Node256 with a maximum-length path and MAX_PATCH_DEPTH full-size patches
        let mut big = TrieNode256::new(&[0u8; MAX_NODE_PATH_BYTES]);
        big.cowptr = Some(TrieCowPtr::new(StacksBlockId([2; 32]), TriePtr::default()));
        for _ in 0..MAX_PATCH_DEPTH {
            big.patches.push((
                1,
                TriePtr::default(),
                TrieNodePatch {
                    ptr: TriePtr::default(),
                    ptr_diff: vec![TriePtr::default(); MAX_PATCH_DIFF_PTRS],
                },
            ));
        }
        let heap = size_of::<TrieNode256>()
            + big.path.capacity()
            + big.patches.capacity() * size_of::<(u32, TriePtr, TrieNodePatch)>()
            + big
                .patches
                .iter()
                .map(|(_, _, p)| p.ptr_diff.capacity() * size_of::<TriePtr>())
                .sum::<usize>();
        let entry = size_of::<PatchedNodeKey<StacksBlockId>>() * 2
            + size_of::<CachedNode>()
            + heap
            + size_of::<TrieCowPtr>();
        assert!(
            entry <= PatchedNodeCache::<StacksBlockId>::WORST_CASE_ENTRY_BYTES,
            "worst case entry is {entry} bytes, more than the assumed bound"
        );
    }

    /// A `read_hash = false` insert stores no hash, so it can serve only `read_hash = false`
    /// reads (with the zero placeholder); a later insert with the real hash upgrades it.
    #[test]
    fn hash_handling() {
        let mut cache = PatchedNodeCache::<StacksBlockId>::new();
        cache.insert(key(1), &node(1), None);

        let (_, h) = cache.get(&key(1), false).expect("hit without hash");
        assert_eq!(h, TrieHash([0u8; 32]), "placeholder hash must stay zero");
        assert!(
            cache.get(&key(1), true).is_none(),
            "an entry without a real hash must not serve a read that needs it"
        );

        cache.insert(key(1), &node(1), Some(real_hash()));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&key(1), true).unwrap().1, real_hash());
        let (_, h) = cache.get(&key(1), false).expect("hit without hash");
        assert_eq!(
            h,
            TrieHash([0u8; 32]),
            "no-hash hit must stay a placeholder"
        );

        let stats = cache.take_stats();
        assert_eq!(stats.inserts, 2);
        assert_eq!(stats.hits, 3);
        assert_eq!(stats.misses, 1);
        assert_eq!(cache.take_stats(), PatchedNodeCacheStats::default());
    }

    /// Mutating a node returned from the cache must not affect later lookups.
    #[test]
    fn lookups_return_independent_copies() {
        let mut cache = PatchedNodeCache::<StacksBlockId>::new();
        cache.insert(key(1), &node(1), Some(real_hash()));

        let (mut got, _) = cache.get(&key(1), true).unwrap();
        got.ptrs_mut()[0] = TriePtr::new(TrieNodeID::Leaf as u8, 9, 99);
        assert_ne!(got.ptrs()[0], TriePtr::default());

        let (again, _) = cache.get(&key(1), true).unwrap();
        assert_eq!(again.ptrs()[0], TriePtr::default());
        assert_eq!(again.path_bytes(), &vec![1u8]);
    }

    /// Keys differing in any component never alias.
    #[test]
    fn distinct_keys_do_not_alias() {
        let mut cache = PatchedNodeCache::<StacksBlockId>::new();
        cache.insert(key(1), &node(1), None);

        let mut other_hash = key(1);
        other_hash.block_hash = StacksBlockId([9; 32]);
        let mut other_id = key(1);
        other_id.block_id = 8;
        let mut other_node_id = key(1);
        other_node_id.node_id = 3;
        for k in [key(2), other_hash, other_id, other_node_id] {
            assert!(cache.get(&k, false).is_none());
        }
        assert!(cache.get(&key(1), false).is_some());
    }

    /// The entry count stays within the capacity; the least recently used entry goes first.
    #[test]
    fn evicts_least_recently_used_beyond_capacity() {
        let mut cache = PatchedNodeCache::<StacksBlockId>::new();
        cache.set_capacity(2);
        cache.insert(key(1), &node(1), None);
        cache.insert(key(2), &node(2), None);
        assert!(cache.get(&key(1), false).is_some()); // key 2 is now the oldest
        cache.insert(key(3), &node(3), None);

        assert_eq!(cache.len(), 2);
        assert!(cache.get(&key(2), false).is_none());
        assert!(cache.get(&key(1), false).is_some());
        assert!(cache.get(&key(3), false).is_some());
    }

    /// Entries filled during a transaction survive a commit but not an abort or an abandonment.
    #[test]
    fn transaction_lifecycle() {
        let mut cache = PatchedNodeCache::<StacksBlockId>::new();

        // committed: kept
        cache.begin_tx();
        cache.insert(key(1), &node(1), None);
        cache.mark_committed();
        cache.clear_if_tx_abandoned();
        assert_eq!(cache.len(), 1);

        // rolled back: dropped
        cache.begin_tx();
        cache.insert(key(2), &node(2), None);
        cache.abort_tx();
        assert!(cache.is_empty());

        // dropped without either: dropped by the next user of the storage, only once
        cache.begin_tx();
        cache.insert(key(3), &node(3), None);
        cache.clear_if_tx_abandoned();
        assert!(cache.is_empty());
        cache.insert(key(4), &node(4), None);
        cache.clear_if_tx_abandoned();
        assert_eq!(cache.len(), 1);
    }

    /// A disabled cache neither serves nor stores.
    #[test]
    fn disabled_cache_is_inert() {
        let mut cache = PatchedNodeCache::<StacksBlockId>::new();
        cache.set_enabled(false);
        cache.insert(key(1), &node(1), Some(real_hash()));
        assert!(cache.get(&key(1), true).is_none());
        assert!(cache.is_empty());
        assert_eq!(cache.take_stats().inserts, 0);
    }
}
