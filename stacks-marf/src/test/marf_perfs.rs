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

use stacks_common::types::chainstate::{BlockHeaderHash, TrieHash};
use stacks_common::util::get_epoch_time_ms;
use stacks_common::util::hash::to_hex;

use crate::cache::BLOCK_HASH_CACHE_CAPACITY;
use crate::marf::{MARFOpenOpts, MarfConnection, MARF};
use crate::storage::TrieFileStorage;
use crate::test::{merkle_test_marf, opts, ForkedHistory};
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

/// Read a benchmark parameter from the environment
fn bhc_param(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Small deterministic generator, so that every configuration replays the same reads
struct BhcRng(u64);

impl BhcRng {
    fn next(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

/// Outcome of replaying the benchmark reads against one cache configuration
struct BhcRun {
    /// Time per `get`, in microseconds, on the first and the second replay
    get_us: [f64; 2],
    /// Time per `get_block_at_height`, in microseconds, on the first and the second replay
    height_us: [f64; 2],
    /// Block hash cache misses during the first and the second replay
    misses: [u64; 2],
    /// Pairs in the cache after both replays
    cache_len: usize,
}

/// Replay `reads` (tip, block, key index) and `heights` (tip, height) twice against a fresh
/// read-only handle on the MARF at `path`, whose block hash cache holds at most `capacity` pairs
/// (the default capacity if `None`), and check every answer against `history`.
fn bhc_run(
    path: &str,
    history: &ForkedHistory,
    capacity: Option<usize>,
    reads: &[(usize, usize, usize)],
    heights: &[(usize, usize)],
) -> BhcRun {
    let mut storage = TrieFileStorage::open_readonly(path, MARFOpenOpts::default()).unwrap();
    if let Some(capacity) = capacity {
        storage.set_block_hash_cache_capacity(capacity);
    }
    let mut marf = MARF::from_storage(storage);
    let mut run = BhcRun {
        get_us: [0.0; 2],
        height_us: [0.0; 2],
        misses: [0; 2],
        cache_len: 0,
    };
    let mut misses_before = 0;
    for pass in 0..2 {
        let start = Instant::now();
        for (tip, block, k) in reads {
            let value = MARF::get_by_key(
                &mut marf.borrow_storage_backend(),
                &history.blocks[*tip],
                &history.key(*block, *k),
            )
            .unwrap();
            assert_eq!(value, Some(history.value(*block, *k)));
        }
        if !reads.is_empty() {
            run.get_us[pass] = start.elapsed().as_secs_f64() * 1e6 / reads.len() as f64;
        }

        let start = Instant::now();
        for (tip, height) in heights {
            let bhh = MARF::get_block_at_height(
                &mut marf.borrow_storage_backend(),
                *height as u32,
                &history.blocks[*tip],
            )
            .unwrap();
            let expected = history.ancestor(*tip, history.chain_len(*tip) - 1 - *height);
            assert_eq!(bhh, Some(history.blocks[expected].clone()));
        }
        if !heights.is_empty() {
            run.height_us[pass] = start.elapsed().as_secs_f64() * 1e6 / heights.len() as f64;
        }

        let conn = marf.borrow_storage_backend();
        let misses = conn.block_hash_cache_misses();
        run.misses[pass] = misses - misses_before;
        misses_before = misses;
    }
    let conn = marf.borrow_storage_backend();
    conn.assert_block_hash_cache_consistent();
    run.cache_len = conn.block_hash_cache_len();
    run
}

fn bhc_report(label: &str, capacity: Option<usize>, run: &BhcRun) {
    info!(
        "BHC {label}: capacity={capacity:?} get_us={:.1}/{:.1} height_us={:.1}/{:.1} misses={}/{} cache_len={}",
        run.get_us[0],
        run.get_us[1],
        run.height_us[0],
        run.height_us[1],
        run.misses[0],
        run.misses[1],
        run.cache_len,
    );
}

/// Measure what bounding `BlockHashCache` costs MARF reads.  Each configuration replays the same
/// seeded reads twice on a fresh handle (first replay: cold cache; second replay: warm cache) and
/// reports microseconds per read and cache misses.
///
/// Parameters (environment): `BHC_BLOCKS` main chain length (default 2048), `BHC_READS` number of
/// gets (default 50000), `BHC_KEYS` keys per block (default 16), `BHC_LINEAR=1` for a single
/// chain with no fork and no height lookups, `BHC_WINDOW` how many of the most recent blocks the
/// linear run reads from (default 60000).
///
/// Default mode has a main chain plus a fork of half its length from its midpoint, reads that
/// alternate between the two forks at seeded-random historical tips, and checks that when the
/// working set fits the capacity the second replay has no misses.  Linear mode (use with `BHC_BLOCKS=70000`) measures the distinct
/// IDs that the reads touch, checks that they fit the default capacity, and then checks that the
/// second replay at the default capacity has no misses.
#[test]
#[ignore]
fn marf_block_hash_cache_read_bench() {
    let blocks = bhc_param("BHC_BLOCKS", 2048);
    let num_reads = bhc_param("BHC_READS", 50_000);
    let keys_per_block = bhc_param("BHC_KEYS", 16);
    let linear = bhc_param("BHC_LINEAR", 0) == 1;
    let window = bhc_param("BHC_WINDOW", 60_000).min(blocks);

    let history = if linear {
        ForkedHistory::new(blocks, 0, 0, keys_per_block)
    } else {
        ForkedHistory::new(blocks, blocks / 2, blocks / 2, keys_per_block)
    };
    let total = history.blocks.len();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marf").to_str().unwrap().to_string();
    let start = Instant::now();
    history.build(&path, MARFOpenOpts::default());
    info!(
        "BHC built {total} blocks x {keys_per_block} keys in {:.1}s",
        start.elapsed().as_secs_f64()
    );

    let mut rng = BhcRng(0x9e3779b97f4a7c15);
    // Alternate between the forks, reading from a seeded-random historical tip on each
    let fork_len = total - blocks;
    let random_tip = |rng: &mut BhcRng, i: usize| {
        if i % 2 == 1 && fork_len > 0 {
            blocks + rng.next(fork_len)
        } else {
            rng.next(blocks)
        }
    };
    let mut reads = vec![];
    for i in 0..num_reads {
        let (tip, depth) = if linear {
            // tips and keys in the most recent `window` blocks
            let lo = blocks - window;
            let tip = lo + rng.next(window);
            (tip, rng.next(tip - lo + 1))
        } else {
            let tip = random_tip(&mut rng, i);
            (tip, rng.next(history.chain_len(tip)))
        };
        reads.push((tip, history.ancestor(tip, depth), rng.next(keys_per_block)));
    }
    let mut heights = vec![];
    if !linear {
        for i in 0..2000 {
            let tip = random_tip(&mut rng, i);
            heights.push((tip, rng.next(history.chain_len(tip))));
        }
    }

    if linear {
        let unbounded = bhc_run(&path, &history, Some(total + 16), &reads, &heights);
        bhc_report("unbounded", Some(total + 16), &unbounded);
        let distinct = unbounded.cache_len;
        info!("BHC distinct block IDs touched: {distinct}");
        assert!(distinct <= BLOCK_HASH_CACHE_CAPACITY);

        let default = bhc_run(&path, &history, None, &reads, &heights);
        bhc_report("default", None, &default);
        assert!(default.cache_len <= BLOCK_HASH_CACHE_CAPACITY);
        assert_eq!(default.misses[1], 0);
    } else {
        let configs = [
            ("unbounded", Some(total + 16)),
            ("default", None),
            ("total", Some(total)),
            ("half", Some(total / 2)),
            ("16", Some(16)),
        ];
        for (label, capacity) in configs {
            let run = bhc_run(&path, &history, capacity, &reads, &heights);
            bhc_report(label, capacity, &run);
            if matches!(label, "unbounded" | "default" | "total") {
                // the working set fits, so nothing was evicted
                assert_eq!(run.misses[1], 0, "{label}");
            }
        }
    }
}
