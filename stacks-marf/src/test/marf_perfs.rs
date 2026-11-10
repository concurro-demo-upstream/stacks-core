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

//! MARF performance tests intended to be run manually for analysis.

use std::fs;
use std::time::Instant;

use stacks_common::types::chainstate::{BlockHeaderHash, StacksBlockId, TrieHash};
use stacks_common::util::get_epoch_time_ms;
use stacks_common::util::hash::to_hex;

use crate::marf::{MARFOpenOpts, MarfConnection, MARF};
use crate::storage::TrieFileStorage;
use crate::test::{merkle_test_marf, opts};
use crate::{ClarityMarfTrieId, MARFValue, TrieLeaf};

// insert a random sequence of 1024 * 1024 keys.  Every 4096 inserts, fork.
// Use file storage, and use batching.
// Used mainly for performance analysis.
#[test]
#[ignore]
fn marf_insert_random_1048576_4096_file_storage() {
    let path = "/tmp/rust_marf_insert_random_1048576_4096_file_storage".to_string();
    if fs::metadata(&path).is_ok() {
        fs::remove_dir_all(&path).unwrap();
    };
    let marf_opts = MARFOpenOpts::default();
    let f = TrieFileStorage::open(&path, marf_opts).unwrap();
    let mut m = MARF::from_storage(f);

    let mut block_header = BlockHeaderHash::sentinel();

    let mut seed = TrieHash::EMPTY.as_bytes().to_vec();
    let mut start_time = get_epoch_time_ms();
    let mut end_time = 0;
    let mut block_start_time = start_time;
    let mut prev_block_header = block_header.clone();

    let mut i: u64 = 1;
    let num_iterations = 1024 * 1024;
    let block_size = 4096;

    while i <= num_iterations {
        let mut keys = vec![];
        let mut values = vec![];

        let i0 = (i & 0xff000000) >> 24;
        let i1 = (i & 0x00ff0000) >> 16;
        let i2 = (i & 0x0000ff00) >> 8;
        let i3 = i & 0x000000ff;

        prev_block_header = block_header.clone();
        block_header = BlockHeaderHash::from_bytes(&[
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            i0 as u8, i1 as u8, i2 as u8, i3 as u8,
        ])
        .unwrap();

        for _ in 0..block_size {
            let i0 = (i & 0xff000000) >> 24;
            let i1 = (i & 0x00ff0000) >> 16;
            let i2 = (i & 0x0000ff00) >> 8;
            let i3 = i & 0x000000ff;

            let path = TrieHash::from_data(&seed[..]).as_bytes()[0..32].to_vec();
            seed = path.clone();

            let key = to_hex(&path);
            let value = to_hex(&[
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, i0 as u8, i1 as u8, i2 as u8, i3 as u8,
            ]);

            keys.push(key);
            values.push(value);
            i += 1;
        }

        block_start_time = get_epoch_time_ms();
        m.begin(&prev_block_header, &block_header).unwrap();

        start_time = get_epoch_time_ms();

        let values = values
            .into_iter()
            .map(|x| MARFValue::from_value(&x))
            .collect();

        m.insert_batch(&keys, values).unwrap();
        end_time = get_epoch_time_ms();

        let flush_start_time = get_epoch_time_ms();
        m.commit().unwrap();
        let flush_end_time = get_epoch_time_ms();

        eprintln!(
            "Inserted {} in {} (1 insert = {} ms).  Processed {} keys in {} ms (flush = {} ms)",
            i,
            end_time - start_time,
            ((end_time - start_time) as f64) / (block_size as f64),
            block_size,
            flush_end_time - block_start_time,
            flush_end_time - flush_start_time
        );
    }

    i = 1;
    seed = TrieHash::EMPTY.as_bytes().to_vec();

    while i <= num_iterations {
        let mut keys = vec![];
        let mut values = vec![];

        for _ in 0..block_size {
            let i0 = (i & 0xff000000) >> 24;
            let i1 = (i & 0x00ff0000) >> 16;
            let i2 = (i & 0x0000ff00) >> 8;
            let i3 = i & 0x000000ff;

            let path = TrieHash::from_data(&seed[..]).as_bytes()[0..32].to_vec();
            seed = path.clone();

            let key = to_hex(&path);
            let value = to_hex(&[
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, i0 as u8, i1 as u8, i2 as u8, i3 as u8,
            ]);

            keys.push(key);
            values.push(value);
            i += 1;
        }

        start_time = get_epoch_time_ms();

        for j in 0..block_size {
            let read_value = m.get(&block_header, &keys[j]).unwrap().unwrap();
            assert_eq!(read_value, MARFValue::from_value(&values[j]));
        }

        end_time = get_epoch_time_ms();

        eprintln!(
            "Got {} in {} (1 get = {} ms)",
            i,
            end_time - start_time,
            ((end_time - start_time) as f64) / (block_size as f64)
        );
    }
}

// Test reads specifically on existing test data.
#[test]
#[ignore]
fn marf_read_random_1048576_4096_file_storage() {
    let do_merkle_check = std::env::var("TEST_MARF_PERFS_MERKLE_PROOFS") == Ok("1".to_string());

    for marf_opts in opts::ALL_OPTS.clone().into_iter() {
        test_debug!("With {:?}", &marf_opts);
        let path = "/tmp/rust_marf_insert_random_1048576_4096_file_storage".to_string();
        if fs::metadata(&path).is_err() {
            eprintln!("Run the marf_insert_random_1048576_4096_file_storage test first");
            return;
        };
        let marf_opts = MARFOpenOpts::default();
        let mut f_store = TrieFileStorage::new_memory(marf_opts).unwrap();
        let mut f = f_store.connection();

        let block_header = BlockHeaderHash::from_bytes(&[
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0xf0, 0xff, 0xff,
        ])
        .unwrap();
        f.open_block(&block_header).unwrap();

        let mut seed = TrieHash::EMPTY.as_bytes().to_vec();
        let mut start_time = 0;

        start_time = get_epoch_time_ms();
        for i in 0..1048576 {
            // can read them all back
            let i0 = (i & 0xff0000) >> 12;
            let i1 = (i & 0x00ff00) >> 8;
            let i2 = i & 0x0000ff;

            let path = TrieHash::from_data(&seed[..]).as_bytes()[0..32].to_vec();
            seed = path.clone();

            let triepath = TrieHash::from_bytes(&path[..]).unwrap();
            let value = TrieLeaf::new(
                &[],
                &[
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, i0 as u8, i1 as u8, i2 as u8,
                ],
            );

            let read_value = MARF::get_path(
                &mut f,
                &block_header,
                &TrieHash::from_bytes(&path[..]).unwrap(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(read_value.data.to_vec(), value.data.to_vec());

            // can make a merkle proof to each one
            if do_merkle_check {
                merkle_test_marf(&mut f, &block_header, &path, &value.data.to_vec(), None);
            }
            if i % 128 == 0 {
                let end_time = get_epoch_time_ms();
                let (read_count, write_count) = f.stats();
                let (node_reads, backptr_reads, node_writes) = f.node_stats();
                let (leaf_reads, leaf_writes) = f.leaf_stats();
                debug!(
                    "Got {} in {} (1 get = {} ms).  Read = {}, Write = {}, Node Reads = {}, Node Writes = {}, Backptr Reads = {}, Leaf Reads = {}, Leaf Writes = {}",
                    i,
                    end_time - start_time,
                    ((end_time - start_time) as f64) / 128.0,
                    read_count,
                    write_count,
                    node_reads,
                    node_writes,
                    backptr_reads,
                    leaf_reads,
                    leaf_writes
                );

                start_time = get_epoch_time_ms();
            }
        }
    }
}

/// Compare repeated lookups with the patched-node cache off and on.
///
/// Builds a compressed MARF (the only kind with patch nodes) with a fork, from seeded pseudo-random
/// keys, then repeatedly looks keys up at the tip, at historical tips and at the fork's tip.  The
/// same lookups run against two identical storages, one with the cache disabled.  Reports wall
/// time, nodes read, nodes rebuilt from patch chains and the cache hit rate, and checks that both
/// storages return the same values.  Run with:
///
/// `cargo test -p stacks-marf --release --lib marf_perfs::patched_node_cache -- --ignored --nocapture`
#[test]
#[ignore]
fn patched_node_cache_off_vs_on() {
    const NUM_KEYS: u64 = 20_000;
    const REWRITES_PER_BLOCK: u64 = 400;
    const NUM_BLOCKS: u8 = 12;
    const FORK_FROM: u8 = 6;
    const FORK_BLOCKS: u8 = 4;
    const LOOKUPS_PER_TIP: u64 = 20_000;
    const PASSES: usize = 3;

    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }
    fn path_for(i: u64) -> TrieHash {
        TrieHash::from_data(&i.to_be_bytes())
    }
    fn blk(n: u8) -> StacksBlockId {
        StacksBlockId([n; 32])
    }

    // (block, parent) in creation order: a chain, then a fork off the middle of it
    let mut plan: Vec<(StacksBlockId, StacksBlockId)> = vec![(blk(1), StacksBlockId::sentinel())];
    for n in 2..=NUM_BLOCKS {
        plan.push((blk(n), blk(n - 1)));
    }
    for n in 0..FORK_BLOCKS {
        let parent = if n == 0 {
            blk(FORK_FROM)
        } else {
            blk(100 + n - 1)
        };
        plan.push((blk(100 + n), parent));
    }
    let tips = [
        blk(NUM_BLOCKS),
        blk(9),
        blk(FORK_FROM),
        blk(100 + FORK_BLOCKS - 1),
    ];

    let mut results = vec![];
    for cache_enabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("marf.sqlite");
        let storage = TrieFileStorage::<StacksBlockId>::open(
            db_path.to_str().unwrap(),
            MARFOpenOpts::default().with_compression(true),
        )
        .unwrap();
        let mut marf = MARF::from_storage(storage);
        marf.borrow_storage_backend()
            .node_cache_mut()
            .set_enabled(cache_enabled);

        let mut seed = 0x9E3779B97F4A7C15u64;
        for (n, (b, parent)) in plan.iter().enumerate() {
            marf.begin(parent, b).unwrap();
            let count = if n == 0 { NUM_KEYS } else { REWRITES_PER_BLOCK };
            for j in 0..count {
                let i = if n == 0 {
                    j
                } else {
                    next(&mut seed) % NUM_KEYS
                };
                let mut data = [n as u8; 40];
                data[..8].copy_from_slice(&i.to_be_bytes());
                marf.insert_raw(path_for(i), TrieLeaf::new(&[], &data))
                    .unwrap();
            }
            marf.commit_to(b).unwrap();
        }

        // identical lookups for both runs
        let mut seed = 0xD1B54A32D192ED03u64;
        marf.borrow_storage_backend().node_cache_stats();
        marf.borrow_storage_backend().node_stats();
        let mut checksum = 0u64;
        let start = Instant::now();
        for _ in 0..PASSES {
            for tip in tips.iter() {
                for _ in 0..LOOKUPS_PER_TIP {
                    let i = next(&mut seed) % NUM_KEYS;
                    let leaf =
                        MARF::get_path(&mut marf.borrow_storage_backend(), tip, &path_for(i))
                            .unwrap()
                            .unwrap();
                    checksum =
                        checksum.wrapping_add(leaf.data.0.iter().map(|b| *b as u64).sum::<u64>());
                }
            }
        }
        let elapsed = start.elapsed();
        let stats = marf.borrow_storage_backend().node_cache_stats();
        let (node_reads, backptr_reads, _) = marf.borrow_storage_backend().node_stats();
        let lookups = (PASSES * tips.len()) as u64 * LOOKUPS_PER_TIP;
        let hit_rate = if stats.hits + stats.misses > 0 {
            100.0 * stats.hits as f64 / (stats.hits + stats.misses) as f64
        } else {
            0.0
        };
        eprintln!(
            "cache {}: {lookups} lookups in {elapsed:?} ({:.2} us/lookup); node reads {}, backptr reads {}, \
             patch reconstructions {}, cache hits {} misses {} (hit rate {hit_rate:.1}%), inserts {}",
            if cache_enabled { "on " } else { "off" },
            elapsed.as_secs_f64() * 1e6 / lookups as f64,
            node_reads,
            backptr_reads,
            stats.reconstructions,
            stats.hits,
            stats.misses,
            stats.inserts,
        );
        results.push((checksum, stats, elapsed));
    }

    assert_eq!(
        results[0].0, results[1].0,
        "cached and uncached lookups disagree"
    );
    assert!(results[1].1.reconstructions <= results[0].1.reconstructions);
    eprintln!(
        "reconstructions: {} -> {}; time: {:?} -> {:?}",
        results[0].1.reconstructions, results[1].1.reconstructions, results[0].2, results[1].2
    );
}
