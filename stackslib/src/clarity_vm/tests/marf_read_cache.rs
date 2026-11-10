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

//! Tests for the open-tip read cache in `PersistentWritableMarfStore`.
//!
//! The cache must be invisible: every read returns what an uncached MARF lookup would, whatever
//! writes, rollbacks and tip changes happened since the key was first read.

use clarity::vm::database::{ClarityBackingStore, RollbackWrapper};
use stacks_common::types::chainstate::StacksBlockId;
use stacks_marf::ClarityMarfTrieId;

use crate::clarity_vm::clarity::ClarityMarfStoreTransaction;
use crate::clarity_vm::database::marf::MarfedKV;

/// Placeholder tip under which a new block's trie is built before it is committed.
fn open_tip() -> StacksBlockId {
    StacksBlockId([0xf0; 32])
}

fn block(n: u8) -> StacksBlockId {
    StacksBlockId([n; 32])
}

fn items(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Commit a block `target` on top of `parent` containing `pairs`.
fn commit_block(
    kv: &mut MarfedKV,
    parent: &StacksBlockId,
    target: &StacksBlockId,
    pairs: &[(&str, &str)],
) {
    let mut store = kv.begin(parent, &open_tip());
    store.put_all_data(items(pairs)).unwrap();
    store.commit_to_processed_block(target).unwrap();
}

fn read_committed(kv: &mut MarfedKV, at: &StacksBlockId, key: &str) -> Option<String> {
    kv.begin_read_only(Some(at)).get_data(key).unwrap()
}

/// `N` reads of one unchanged key at the open tip cost exactly one MARF lookup: the first read
/// misses and the other `N - 1` hit.
#[test]
fn repeated_reads_do_one_marf_lookup() {
    let mut kv = MarfedKV::temporary();
    commit_block(
        &mut kv,
        &StacksBlockId::sentinel(),
        &block(1),
        &[("k", "v1")],
    );

    let mut store = kv.begin(&block(1), &open_tip());
    for _ in 0..100 {
        assert_eq!(store.get_data("k").unwrap(), Some("v1".to_string()));
    }
    assert_eq!(store.read_cache_stats(), (99, 1));
    store.drop_current_trie();
}

/// A key that is absent is cached as absent, and becomes visible once it is written.
#[test]
fn absent_then_present() {
    let mut kv = MarfedKV::temporary();
    commit_block(
        &mut kv,
        &StacksBlockId::sentinel(),
        &block(1),
        &[("other", "x")],
    );

    let mut store = kv.begin(&block(1), &open_tip());
    assert_eq!(store.get_data("k").unwrap(), None);
    assert_eq!(store.get_data("k").unwrap(), None);
    assert_eq!(store.read_cache_stats(), (1, 1));

    store.put_all_data(items(&[("k", "v")])).unwrap();
    assert_eq!(store.get_data("k").unwrap(), Some("v".to_string()));
    store.drop_current_trie();
}

/// Overwriting a cached key is visible to the next read, repeatedly.
#[test]
fn overwrite_then_read() {
    let mut kv = MarfedKV::temporary();
    commit_block(
        &mut kv,
        &StacksBlockId::sentinel(),
        &block(1),
        &[("k", "v1")],
    );

    let mut store = kv.begin(&block(1), &open_tip());
    assert_eq!(store.get_data("k").unwrap(), Some("v1".to_string()));
    store.put_all_data(items(&[("k", "v2")])).unwrap();
    assert_eq!(store.get_data("k").unwrap(), Some("v2".to_string()));
    store.put_all_data(items(&[("k", "v3")])).unwrap();
    assert_eq!(store.get_data("k").unwrap(), Some("v3".to_string()));
    store.drop_current_trie();
}

/// Reads at a historical tip (as `at-block` does via `set_block_hash`) bypass the cache and see
/// the historical value, while open-tip reads keep seeing the newer value.
#[test]
fn historical_reads_bypass_cache() {
    let mut kv = MarfedKV::temporary();
    commit_block(
        &mut kv,
        &StacksBlockId::sentinel(),
        &block(1),
        &[("k", "v1")],
    );

    let mut store = kv.begin(&block(1), &open_tip());
    store.put_all_data(items(&[("k", "v2")])).unwrap();
    // warm the cache at the open tip
    assert_eq!(store.get_data("k").unwrap(), Some("v2".to_string()));
    let stats = store.read_cache_stats();

    let tip = store.set_block_hash(block(1)).unwrap();
    assert_eq!(tip, open_tip());
    assert_eq!(store.get_data("k").unwrap(), Some("v1".to_string()));
    assert_eq!(store.read_cache_stats(), stats);

    store.set_block_hash(open_tip()).unwrap();
    assert_eq!(store.get_data("k").unwrap(), Some("v2".to_string()));
    store.drop_current_trie();
}

/// A rolled-back Clarity write never reaches the store, so cached reads are unaffected, while a
/// committed one does.
#[test]
fn clarity_rollback_then_read() {
    let mut kv = MarfedKV::temporary();
    commit_block(
        &mut kv,
        &StacksBlockId::sentinel(),
        &block(1),
        &[("k", "v1")],
    );

    let mut store = kv.begin(&block(1), &open_tip());
    {
        let mut wrapper = RollbackWrapper::new(&mut store);
        wrapper.nest();
        assert_eq!(
            wrapper.get_data::<String>("k").unwrap(),
            Some("v1".to_string())
        );
        assert_eq!(wrapper.get_data::<String>("new").unwrap(), None);

        // rolled-back writes: one to an existing key, one to an absent key
        wrapper.nest();
        wrapper.put_data("k", "v2").unwrap();
        wrapper.put_data("new", "n").unwrap();
        assert_eq!(
            wrapper.get_data::<String>("k").unwrap(),
            Some("v2".to_string())
        );
        wrapper.rollback().unwrap();
        assert_eq!(
            wrapper.get_data::<String>("k").unwrap(),
            Some("v1".to_string())
        );
        assert_eq!(wrapper.get_data::<String>("new").unwrap(), None);

        // committed write
        wrapper.nest();
        wrapper.put_data("k", "v3").unwrap();
        wrapper.commit().unwrap();
        wrapper.commit().unwrap();
    }
    assert_eq!(store.get_data("k").unwrap(), Some("v3".to_string()));
    assert_eq!(store.get_data("new").unwrap(), None);
    store.drop_current_trie();
}

/// Dropping a trie discards its cache along with its writes: a fresh store on the same parent
/// sees the parent's value.
#[test]
fn dropped_block_does_not_leak_cache() {
    let mut kv = MarfedKV::temporary();
    commit_block(
        &mut kv,
        &StacksBlockId::sentinel(),
        &block(1),
        &[("k", "v1")],
    );

    let mut store = kv.begin(&block(1), &open_tip());
    assert_eq!(store.get_data("k").unwrap(), Some("v1".to_string()));
    store.put_all_data(items(&[("k", "v2")])).unwrap();
    assert_eq!(store.get_data("k").unwrap(), Some("v2".to_string()));
    store.drop_current_trie();

    let mut store = kv.begin(&block(1), &open_tip());
    assert_eq!(store.get_data("k").unwrap(), Some("v1".to_string()));
    assert_eq!(store.read_cache_stats(), (0, 1));
    store.drop_current_trie();
}

/// The cache never holds more than its capacity, and evicting entries never changes results.
#[test]
fn cache_is_bounded() {
    const CAP: usize = 8;
    const KEYS: usize = 3 * CAP + 1;

    let mut kv = MarfedKV::temporary();
    let keys: Vec<(String, String)> = (0..KEYS)
        .map(|i| (format!("key-{i}"), format!("value-{i}")))
        .collect();
    let pairs: Vec<(&str, &str)> = keys.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    commit_block(&mut kv, &StacksBlockId::sentinel(), &block(1), &pairs);

    let mut store = kv.begin(&block(1), &open_tip());
    store.set_read_cache_capacity(CAP);
    for _ in 0..3 {
        for (k, v) in keys.iter() {
            assert_eq!(store.get_data(k).unwrap().as_deref(), Some(v.as_str()));
            assert!(store.read_cache_len() <= CAP);
        }
    }
    store.drop_current_trie();
}

/// Reads interleaved with writes do not change what is committed: every key reads back as
/// expected afterwards, and the state root equals that of the same writes made without any reads.
#[test]
fn reads_do_not_change_committed_state() {
    let blocks: [&[(&str, &str)]; 3] = [
        &[("a", "1"), ("b", "2")],
        &[("b", "3"), ("c", "4")],
        &[("a", "5"), ("d", "6")],
    ];

    let mut with_reads = MarfedKV::temporary();
    let mut parent = StacksBlockId::sentinel();
    for (i, pairs) in blocks.iter().enumerate() {
        let target = block(i as u8 + 1);
        let mut store = with_reads.begin(&parent, &open_tip());
        for key in ["a", "b", "c", "d", "missing"] {
            store.get_data(key).unwrap();
        }
        for (k, v) in pairs.iter() {
            store.put_all_data(items(&[(k, v)])).unwrap();
            assert_eq!(store.get_data(k).unwrap().as_deref(), Some(*v));
        }
        store.commit_to_processed_block(&target).unwrap();
        parent = target;
    }

    let mut without_reads = MarfedKV::temporary();
    let mut parent = StacksBlockId::sentinel();
    for (i, pairs) in blocks.iter().enumerate() {
        let target = block(i as u8 + 1);
        commit_block(&mut without_reads, &parent, &target, pairs);
        parent = target;
    }

    let tip = block(3);
    let expected = [
        ("a", Some("5")),
        ("b", Some("3")),
        ("c", Some("4")),
        ("d", Some("6")),
        ("missing", None),
    ];
    for (key, value) in expected {
        assert_eq!(read_committed(&mut with_reads, &tip, key).as_deref(), value);
    }
    assert_eq!(
        with_reads.get_marf().get_root_hash_at(&tip).unwrap(),
        without_reads.get_marf().get_root_hash_at(&tip).unwrap()
    );
}
