// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::fingerprint::{lift, Fingerprint};

use super::super::{FingerprintTreeMap, Node, MIN_CAPACITY};

/// `check_invariants` only proves anything if it actually rejects a broken tree: tamper with a
/// leaf's cached fingerprint directly (through the crate-internal `Node` fields tests.rs shares
/// with the rest of `fingerprint_tree_map`) and require a panic.
#[test]
fn check_invariants_panics_on_a_corrupted_fingerprint_cache() {
    let mut tree: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
    tree.insert(1, 10);
    tree.insert(2, 20);
    tree.check_invariants();

    tree.root.fingerprints[0] += lift(&999u64, &999u64);

    let panicked =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tree.check_invariants())).is_err();
    assert!(
        panicked,
        "check_invariants must reject a tampered fingerprint cache"
    );
}

/// The minimum-occupancy check only applies once `min` or `max` is `Some` (i.e. not at the true
/// root) -- along the rightmost spine, `max` stays `None` all the way down and only `min` is
/// `Some`, so this specifically exercises the `||`, not the `&&` a node could be mutated to.
///
/// Repairing every cached [`Aggregate`] back up to the root after truncating the leaf isolates
/// that one invariant: left alone, the (also real, but different) stale-subtree-aggregate check
/// would panic first regardless of `||` vs `&&`, and the test would stop telling them apart.
#[test]
fn check_invariants_panics_on_an_underfull_non_root_node_on_the_rightmost_spine() {
    let mut tree: FingerprintTreeMap<u64, u64> = (0..200).map(|k| (k, k)).collect();
    tree.check_invariants();

    fn truncate_rightmost_leaf<K, V>(node: &mut Node<K, V>) {
        if let Some(children) = node.children.as_mut() {
            truncate_rightmost_leaf(children.last_mut().unwrap());
        } else {
            while node.keys.len() >= MIN_CAPACITY {
                node.keys.pop();
                node.values.pop();
                node.fingerprints.pop();
            }
        }
        node.refresh_aggregate();
    }
    truncate_rightmost_leaf(tree.root.as_mut());

    let panicked =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tree.check_invariants())).is_err();
    assert!(
        panicked,
        "check_invariants must reject an under-full non-root node"
    );
}

#[test]
fn retain_drops_non_matching_and_preserves_invariants() {
    let mut tree: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
    for k in 0..30 {
        tree.insert(k, k * 10);
    }
    tree.check_invariants();

    tree.retain(|k, _| k % 2 == 0);
    tree.check_invariants();

    assert_eq!(tree.len(), 15);
    for k in 0..30 {
        if k % 2 == 0 {
            assert_eq!(tree.get(&k), Some(&(k * 10)));
        } else {
            assert_eq!(tree.get(&k), None);
        }
    }

    tree.retain(|_, v| *v < 100);
    tree.check_invariants();
    assert!(tree.iter().all(|(_, v)| *v < 100));
}

#[test]
fn with_mut_keeps_aggregates_consistent_when_the_callback_panics() {
    let mut tree: FingerprintTreeMap<u64, u64> = (0..50).map(|k| (k, k)).collect();
    let before = tree.aggregate(..);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tree.with_mut(&17, |v| {
            *v.expect("17 is present") = 999;
            panic!("callback blew up mid-mutation");
        })
    }));
    assert!(outcome.is_err(), "the panic must still propagate");

    assert_eq!(tree.get(&17), Some(&999));

    // The caches must describe what is actually stored.
    tree.check_invariants();
    let after = tree.aggregate(..);
    assert_ne!(after, before, "the summary must have moved with the value");
    assert_eq!(after.size(), 50, "no element was added or removed");

    tree.insert(200, 200);
    tree.check_invariants();
    assert_eq!(tree.len(), 51);
}

#[test]
fn with_mut_passes_the_callback_result_back() {
    let mut tree: FingerprintTreeMap<u64, u64> = (0..10).map(|k| (k, k)).collect();

    let doubled = tree.with_mut(&3, |v| {
        let v = v.expect("3 is present");
        *v *= 2;
        *v
    });
    assert_eq!(doubled, 6);
    assert_eq!(tree.get(&3), Some(&6));
    tree.check_invariants();

    assert!(tree.with_mut(&999, |v| v.is_none()));
    tree.check_invariants();
}

/// `check_invariants` must actually detect a broken tree, not just run without asserting
/// anything: corrupt the cached per-element fingerprint directly (private-field access, same
/// module) and confirm it panics rather than silently accepting the mismatch.
#[test]
#[should_panic(expected = "per-element fingerprint cache invalid")]
fn check_invariants_catches_a_corrupted_fingerprint_cache() {
    let mut map: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
    map.insert(1, 10);
    // Combining with a nonzero fingerprint always changes the value (group addition), so
    // this is guaranteed to no longer match `lift(&1, &10)`.
    map.root.fingerprints[0] = map.root.fingerprints[0].combine(Fingerprint([1, 0, 0, 0]));
    map.check_invariants();
}

/// The minimum-occupancy invariant applies to every **non-root** node — shrink a leaf below
/// [`MIN_CAPACITY`] directly (bypassing the safe `remove` path, which would rebalance to
/// preserve it) and confirm `check_invariants` catches it.
#[test]
#[should_panic(expected = "minimum node size invariant violated")]
fn check_invariants_catches_an_undersized_non_root_node() {
    let mut map: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
    for i in 0..200 {
        map.insert(i, i);
    }
    map.check_invariants(); // sanity: valid before corruption

    fn shrink_a_leaf<K, V>(node: &mut Node<K, V>) {
        if let Some(children) = node.children.as_mut() {
            shrink_a_leaf(&mut children[0]);
        } else {
            while node.keys.len() >= MIN_CAPACITY {
                node.keys.pop();
                node.values.pop();
                node.fingerprints.pop();
            }
        }
    }
    shrink_a_leaf(&mut map.root);

    map.check_invariants();
}
