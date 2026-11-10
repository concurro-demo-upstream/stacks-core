// Copyright (C) 2026 Stacks Open Internet Foundation
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

//! Tests for the patched-node cache (`PatchedNodeCache`): reads through the cache must always
//! match reads with the cache disabled, whatever forks, tips, rollbacks and reopens happen in
//! between.
//!
//! Each test builds a compressed MARF (the only kind that contains patch nodes) next to a
//! "plain" twin built identically with the cache disabled, and compares the two.

use std::io::Read;

use stacks_common::types::chainstate::StacksBlockId;
use tempfile::TempDir;

use super::*;
use crate::cache::PatchedNodeCacheStats;
use crate::node::{is_backptr, TrieNodeID};
use crate::*;

/// Number of keys inserted by the first block
const NUM_KEYS: u64 = 300;

/// Compressed storage variants: blobs in SQLite, and blobs in the external file.
fn compressed_opts() -> Vec<MARFOpenOpts> {
    vec![opts::OPTS_DEF_COMP.clone(), opts::OPTS_DEF_EXT_COMP.clone()]
}

fn block(n: u8) -> StacksBlockId {
    StacksBlockId([n; 32])
}

fn path_for(i: u64) -> TrieHash {
    TrieHash::from_data(&i.to_be_bytes())
}

fn leaf_for(i: u64, round: u8) -> TrieLeaf {
    let mut data = [round; 40];
    data[..8].copy_from_slice(&i.to_be_bytes());
    TrieLeaf::new(&[], &data)
}

/// The blocks of the test chain: (block, parent, rewritten key divisor, round).  The first block
/// inserts every key; each later block rewrites the keys divisible by its divisor.  Blocks 1-5 form
/// a chain deep enough to reach `MAX_PATCH_DEPTH`, and 6-7 fork off block 2.
const CHAIN: &[(u8, u8, u64, u8)] = &[
    (1, 0, 1, 0),
    (2, 1, 5, 1),
    (3, 2, 3, 2),
    (4, 3, 7, 3),
    (5, 4, 11, 4),
    (6, 2, 4, 5),
    (7, 6, 6, 6),
];

/// Block hashes of [`CHAIN`], in order
fn chain_tips() -> Vec<StacksBlockId> {
    CHAIN.iter().map(|(b, ..)| block(*b)).collect()
}

fn parent_of(parent: u8) -> StacksBlockId {
    if parent == 0 {
        StacksBlockId::sentinel()
    } else {
        block(parent)
    }
}

/// Write `blk` on top of `parent`, rewriting the keys in `indexes` with `round`'s values.
fn write_block(
    marf: &mut MARF<StacksBlockId>,
    parent: &StacksBlockId,
    blk: &StacksBlockId,
    indexes: impl Iterator<Item = u64>,
    round: u8,
) {
    marf.begin(parent, blk).unwrap();
    for i in indexes {
        marf.insert_raw(path_for(i), leaf_for(i, round)).unwrap();
    }
    marf.commit_to(blk).unwrap();
}

/// Open the storage at `dir` and, if `cache_enabled` is false, turn its patched-node cache off.
fn open_marf(dir: &TempDir, opts: &MARFOpenOpts, cache_enabled: bool) -> MARF<StacksBlockId> {
    let path = dir.path().join("marf.sqlite");
    let storage = TrieFileStorage::open(path.to_str().unwrap(), opts.clone()).unwrap();
    let mut marf = MARF::from_storage(storage);
    marf.borrow_storage_backend()
        .node_cache_mut()
        .set_enabled(cache_enabled);
    marf
}

/// Build [`CHAIN`] in a fresh directory.
fn build_chain(opts: &MARFOpenOpts, cache_enabled: bool) -> (TempDir, MARF<StacksBlockId>) {
    let dir = tempfile::tempdir().unwrap();
    let mut marf = open_marf(&dir, opts, cache_enabled);
    for (b, parent, divisor, round) in CHAIN {
        let keys = (0..NUM_KEYS).filter(|i| i % divisor == 0);
        write_block(&mut marf, &parent_of(*parent), &block(*b), keys, *round);
    }
    (dir, marf)
}

/// Everything observable about a lookup: the value, and the proof, which exercises the whole
/// path including hashes read along it.
fn read_one(
    marf: &mut MARF<StacksBlockId>,
    tip: &StacksBlockId,
    i: u64,
) -> Option<(MARFValue, String)> {
    marf.get_with_proof_from_hash(tip, &path_for(i))
        .unwrap()
        .map(|(value, proof)| (value, format!("{proof:?}")))
}

fn cache_stats(marf: &mut MARF<StacksBlockId>) -> PatchedNodeCacheStats {
    marf.borrow_storage_backend().node_cache_stats()
}

fn cache_len(marf: &mut MARF<StacksBlockId>) -> usize {
    marf.borrow_storage_backend().node_cache_mut().len()
}

/// Deterministic shuffle of every (tip index, key) pair, so that reads interleave across forks
/// and historical tips.
fn interleaved_reads(num_tips: usize) -> Vec<(usize, u64)> {
    let mut pairs: Vec<(usize, u64)> = (0..num_tips)
        .flat_map(|t| (0..NUM_KEYS).map(move |k| (t, k)))
        .collect();
    let mut state = 0x2545F4914F6CDD1Du64;
    for i in (1..pairs.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        pairs.swap(i, (state % (i as u64 + 1)) as usize);
    }
    pairs
}

/// Read every key at every tip, in interleaved order, from both MARFs; they must agree on
/// everything.
fn assert_same_reads(
    cached: &mut MARF<StacksBlockId>,
    plain: &mut MARF<StacksBlockId>,
    tips: &[StacksBlockId],
) {
    for (t, k) in interleaved_reads(tips.len()) {
        assert_eq!(
            read_one(cached, &tips[t], k),
            read_one(plain, &tips[t], k),
            "tip {} key {k}",
            t + 1
        );
    }
    for tip in tips {
        assert_eq!(
            cached.get_root_hash_at(tip).unwrap(),
            plain.get_root_hash_at(tip).unwrap()
        );
    }
}

/// The full serialized trie blob of `blk`, however the MARF stores it.
fn trie_blob_bytes(dir: &TempDir, marf: &MARF<StacksBlockId>, blk: &StacksBlockId) -> Vec<u8> {
    let conn = marf.sqlite_conn();
    let id = trie_sql::get_block_identifier(conn, blk).unwrap();
    let blobs_path = dir.path().join("marf.sqlite.blobs");
    if blobs_path.exists() {
        let (offset, length) = trie_sql::get_external_trie_offset_length(conn, id).unwrap();
        let all = std::fs::read(&blobs_path).unwrap();
        all[offset as usize..(offset + length) as usize].to_vec()
    } else {
        let mut bytes = vec![];
        trie_sql::open_trie_blob_readonly(conn, id)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }
}

/// Find a node stored in the open block's own trie that was reconstructed from patches.
fn find_patched_ptr(conn: &mut TrieStorageConnection<StacksBlockId>) -> Option<TriePtr> {
    let mut frontier = vec![conn.root_trieptr()];
    while let Some(ptr) = frontier.pop() {
        let node = conn.read_nodetype_nohash(&ptr).unwrap();
        if !node.get_patches().is_empty() {
            return Some(ptr);
        }
        for child in node.ptrs() {
            if !child.is_empty() && !is_backptr(child.id()) && child.id() != TrieNodeID::Leaf as u8
            {
                frontier.push(*child);
            }
        }
    }
    None
}

/// Reads at every fork tip and historical tip, interleaved, match a cache-disabled storage, and
/// the cache is actually used: it hits, and rebuilds fewer nodes.
#[test]
fn reads_match_uncached_across_forks_and_tips() {
    for opts in compressed_opts() {
        let (_d1, mut cached) = build_chain(&opts, true);
        let (_d2, mut plain) = build_chain(&opts, false);
        let tips = chain_tips();

        cache_stats(&mut cached);
        cache_stats(&mut plain);
        assert_same_reads(&mut cached, &mut plain, &tips);

        let cs = cache_stats(&mut cached);
        let ps = cache_stats(&mut plain);
        assert!(cs.hits > 0, "cache never hit: {cs:?}");
        assert_eq!(ps.hits + ps.inserts, 0, "disabled cache was used: {ps:?}");
        assert!(ps.reconstructions > 0, "chain has no patched nodes: {ps:?}");
        assert!(
            cs.reconstructions < ps.reconstructions,
            "cache did not save reconstructions: {cs:?} vs {ps:?}"
        );
        assert!(cache_len(&mut cached) > 0);
    }
}

/// Warm the cache through a patched parent, then commit a child whose sealing creates patches
/// against that parent: roots, proofs and the child's serialized blob must be byte-identical to
/// a cache-disabled twin given the same operations.
#[test]
fn child_commit_after_warm_parent_is_identical() {
    for opts in compressed_opts() {
        let (d1, mut cached) = build_chain(&opts, true);
        let (d2, mut plain) = build_chain(&opts, false);
        let tips = chain_tips();

        // warm the cache with lookups through the parent tips
        for tip in [block(5), block(7)] {
            for k in 0..NUM_KEYS {
                assert_eq!(
                    read_one(&mut cached, &tip, k),
                    read_one(&mut plain, &tip, k)
                );
            }
        }
        assert!(cache_len(&mut cached) > 0, "cache was not warmed");
        assert!(cache_stats(&mut cached).hits > 0);

        // extend the parent, rewriting some keys and adding new ones
        let child = block(0x40);
        for marf in [&mut cached, &mut plain] {
            let keys = (0..NUM_KEYS + 20).filter(|i| i % 2 == 0 || *i >= NUM_KEYS);
            write_block(marf, &block(5), &child, keys, 9);
        }

        assert_eq!(
            trie_blob_bytes(&d1, &cached, &child),
            trie_blob_bytes(&d2, &plain, &child),
            "child trie blob differs"
        );
        let mut all_tips = tips;
        all_tips.push(child);
        assert_same_reads(&mut cached, &mut plain, &all_tips);
    }
}

/// A node read without its hash and then with it returns the real hash, and cached hits keep
/// returning it.
#[test]
fn hash_is_never_a_placeholder_after_cached_no_hash_read() {
    for opts in compressed_opts() {
        let (_d1, mut cached) = build_chain(&opts, true);
        let (_d2, mut plain) = build_chain(&opts, false);
        let tip = block(5);

        let mut plain_conn = plain.borrow_storage_backend();
        plain_conn.open_block(&tip).unwrap();
        let ptr = find_patched_ptr(&mut plain_conn).expect("no patched node in tip trie");
        let (plain_node, plain_hash) = plain_conn.read_nodetype(&ptr).unwrap();
        assert_ne!(plain_hash, TrieHash([0u8; 32]));

        let mut conn = cached.borrow_storage_backend();
        conn.open_block(&tip).unwrap();
        conn.node_cache_mut().clear();
        conn.node_cache_stats();

        conn.read_nodetype_nohash(&ptr).unwrap();
        let stats = conn.node_cache_stats();
        assert_eq!((stats.misses, stats.inserts), (1, 1));

        // the cached entry has no hash, so this must go to storage and upgrade the entry
        let (node, hash) = conn.read_nodetype(&ptr).unwrap();
        assert_eq!(hash, plain_hash);
        assert_eq!(node.ptrs(), plain_node.ptrs());
        let stats = conn.node_cache_stats();
        assert_eq!((stats.hits, stats.misses, stats.inserts), (0, 1, 1));

        // now both flavors of read are served from the cache
        let (_, hash) = conn.read_nodetype(&ptr).unwrap();
        assert_eq!(hash, plain_hash);
        conn.read_nodetype_nohash(&ptr).unwrap();
        let stats = conn.node_cache_stats();
        assert_eq!((stats.hits, stats.misses), (2, 0));
    }
}

/// A transaction that flushed a block and read it (filling the cache from uncommitted rows) and
/// was then dropped without committing leaves nothing in the cache, and every read still matches
/// the twin that never saw the abandoned block.
///
/// (The storage's block hash cache does not support reusing the rolled back rowid within the
/// same `TrieFileStorage`, so that case is not exercised here.)
#[test]
fn abandoned_transaction_does_not_leave_stale_nodes() {
    for opts in compressed_opts() {
        let (_d1, mut cached) = build_chain(&opts, true);
        let (_d2, mut plain) = build_chain(&opts, false);
        let abandoned = block(0x50);

        // write the abandoned block's contents and flush them inside a transaction
        cached.begin(&block(5), &abandoned).unwrap();
        for i in (0..NUM_KEYS).filter(|i| i % 2 == 1) {
            cached.insert_raw(path_for(i), leaf_for(i, 7)).unwrap();
        }
        {
            let mut tx = cached.begin_tx().unwrap();
            tx.storage.flush().unwrap();
            for k in 0..NUM_KEYS {
                MARF::get_path(&mut tx.storage, &abandoned, &path_for(k)).unwrap();
            }
            assert!(!tx.storage.node_cache_mut().is_empty());
            // dropped here without commit: SQLite rolls the flush back
        }
        assert_eq!(
            cache_len(&mut cached),
            0,
            "stale nodes survived the rollback"
        );
        cached.drop_current();

        assert_same_reads(&mut cached, &mut plain, &chain_tips());
    }
}

/// Same, for a transaction that is explicitly rolled back with `drop_current`.
#[test]
fn rolled_back_trie_does_not_leave_stale_nodes() {
    for opts in compressed_opts() {
        let (_d1, mut cached) = build_chain(&opts, true);
        let (_d2, mut plain) = build_chain(&opts, false);
        let dropped = block(0x60);

        cached.begin(&block(5), &dropped).unwrap();
        for i in (0..NUM_KEYS).filter(|i| i % 3 == 1) {
            cached.insert_raw(path_for(i), leaf_for(i, 7)).unwrap();
        }
        {
            let mut tx = cached.begin_tx().unwrap();
            tx.storage.flush().unwrap();
            for k in 0..NUM_KEYS {
                MARF::get_path(&mut tx.storage, &dropped, &path_for(k)).unwrap();
            }
            assert!(!tx.storage.node_cache_mut().is_empty());
            tx.drop_current();
        }
        assert_eq!(
            cache_len(&mut cached),
            0,
            "stale nodes survived drop_current"
        );

        assert_same_reads(&mut cached, &mut plain, &chain_tips());
    }
}

/// An unconfirmed storage never caches, even when reading patched confirmed nodes, and rewriting
/// unconfirmed state is always visible.
#[test]
fn unconfirmed_storage_never_caches() {
    for opts in compressed_opts() {
        let (dir, confirmed) = build_chain(&opts, true);
        drop(confirmed);

        let path = dir.path().join("marf.sqlite");
        let storage =
            TrieFileStorage::<StacksBlockId>::open_unconfirmed(path.to_str().unwrap(), opts)
                .unwrap();
        let mut marf = MARF::from_storage(storage);

        let confirmed_before: Vec<_> = (0..NUM_KEYS)
            .map(|i| {
                MARF::get_path(&mut marf.borrow_storage_backend(), &block(5), &path_for(i))
                    .unwrap()
                    .unwrap()
                    .data
            })
            .collect();

        for round in [1u8, 2u8] {
            // (re)write the unconfirmed trie atop the confirmed tip, replacing its values
            let unconfirmed_tip = marf.begin_unconfirmed(&block(5)).unwrap();
            for i in 0..NUM_KEYS {
                marf.insert_raw(path_for(i), leaf_for(i, 100 + round))
                    .unwrap();
            }
            marf.commit().unwrap();

            for i in 0..NUM_KEYS {
                let leaf = MARF::get_path(
                    &mut marf.borrow_storage_backend(),
                    &unconfirmed_tip,
                    &path_for(i),
                )
                .unwrap()
                .unwrap();
                assert_eq!(leaf.data, leaf_for(i, 100 + round).data);
                // and the confirmed tip is untouched
                let leaf =
                    MARF::get_path(&mut marf.borrow_storage_backend(), &block(5), &path_for(i))
                        .unwrap()
                        .unwrap();
                assert_eq!(leaf.data, confirmed_before[i as usize]);
            }
        }

        let stats = cache_stats(&mut marf);
        assert_eq!((stats.hits, stats.inserts), (0, 0), "{stats:?}");
        assert_eq!(cache_len(&mut marf), 0);
    }
}

/// Every way of reopening a storage starts with an empty cache, and reads match the original.
#[test]
fn reopened_storage_starts_empty_and_matches() {
    for opts in compressed_opts() {
        let (_d, mut cached) = build_chain(&opts, true);
        let tips = chain_tips();

        // fill the original's cache
        for k in 0..NUM_KEYS {
            read_one(&mut cached, &block(5), k);
        }
        assert!(cache_len(&mut cached) > 0);

        let mut ro = cached.reopen_readonly().unwrap();
        assert_eq!(cache_len(&mut ro), 0);

        let mut ro_storage = TrieFileStorage::<StacksBlockId>::reopen_readonly(
            &cached.reopen_storage_readonly().unwrap(),
        )
        .unwrap();
        assert!(ro_storage.connection().node_cache_mut().is_empty());

        {
            let mut reopened = cached.reopen_connection().unwrap();
            let mut conn = reopened.connection();
            assert!(conn.node_cache_mut().is_empty());
            conn.open_block(&block(5)).unwrap();
            let ptr = find_patched_ptr(&mut conn).expect("no patched node");
            let (node, hash) = conn.read_nodetype(&ptr).unwrap();
            drop(conn);
            let mut conn = cached.borrow_storage_backend();
            conn.open_block(&block(5)).unwrap();
            let (orig_node, orig_hash) = conn.read_nodetype(&ptr).unwrap();
            assert_eq!(hash, orig_hash);
            assert_eq!(node.ptrs(), orig_node.ptrs());
        }

        for (t, k) in interleaved_reads(tips.len()) {
            let path = path_for(k);
            let from_ro =
                MARF::get_path(&mut ro.borrow_storage_backend(), &tips[t], &path).unwrap();
            let from_orig =
                MARF::get_path(&mut cached.borrow_storage_backend(), &tips[t], &path).unwrap();
            assert_eq!(from_ro.map(|l| l.data), from_orig.map(|l| l.data));
        }
        for tip in &tips {
            assert_eq!(
                ro.get_root_hash_at(tip).unwrap(),
                cached.get_root_hash_at(tip).unwrap()
            );
        }
    }
}

/// With a tiny capacity the entry count stays within it, and evicted nodes read back correctly.
#[test]
fn small_capacity_stays_bounded_and_correct() {
    for opts in compressed_opts() {
        let (_d1, mut cached) = build_chain(&opts, true);
        let (_d2, mut plain) = build_chain(&opts, false);
        cached
            .borrow_storage_backend()
            .node_cache_mut()
            .set_capacity(3);

        for (t, k) in interleaved_reads(CHAIN.len()) {
            let tip = &chain_tips()[t];
            assert_eq!(read_one(&mut cached, tip, k), read_one(&mut plain, tip, k));
            assert!(cache_len(&mut cached) <= 3);
        }
        let stats = cache_stats(&mut cached);
        assert!(stats.inserts > 3, "no evictions were exercised: {stats:?}");
    }
}
