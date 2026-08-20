// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ops::RangeBounds;

use rand::{seq::SliceRandom, Rng, SeedableRng};

use crate::aggregate::Aggregate;
use crate::fingerprint::{lift, Fingerprint};

use super::FingerprintTreeMap;

#[test]
fn test_simple() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut tree: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
    for _ in 1..=100 {
        tree.insert(rng.gen(), rng.gen());
        tree.check_invariants();
    }
}

#[test]
fn first_and_last_key_value() {
    let mut tree: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
    assert_eq!(tree.first_key_value(), None);
    assert_eq!(tree.last_key_value(), None);

    tree.insert(5, 50);
    assert_eq!(tree.first_key_value(), Some((&5, &50)));
    assert_eq!(tree.last_key_value(), Some((&5, &50)));

    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    for _ in 1..=200 {
        let key: i32 = rng.gen();
        tree.insert(key, key.wrapping_mul(2));
    }
    tree.check_invariants();
    let min_key = *tree.select(0);
    let max_key = *tree.select(tree.len() - 1);
    assert_eq!(
        tree.first_key_value(),
        Some((&min_key, &min_key.wrapping_mul(2)))
    );
    assert_eq!(
        tree.last_key_value(),
        Some((&max_key, &max_key.wrapping_mul(2)))
    );
}

#[test]
fn test_aggregate() {
    // empty
    let mut tree = FingerprintTreeMap::new();
    assert_eq!(tree.aggregate(..), Aggregate::ZERO);
    tree.check_invariants();

    // 1 value
    tree.insert(50, "Hello");
    tree.check_invariants();
    let agg1 = tree.aggregate(..);
    assert_eq!(agg1.size(), 1);
    // Fingerprints, not aggregates: differing sizes would make `!=` trivial.
    assert_ne!(agg1.fingerprint(), Fingerprint::ZERO);

    // 2 values
    tree.insert(25, "World!");
    tree.check_invariants();
    let agg2 = tree.aggregate(..);
    assert_eq!(agg2.size(), 2);
    assert_ne!(agg2.fingerprint(), Fingerprint::ZERO);
    assert_ne!(agg2.fingerprint(), agg1.fingerprint());

    // 3 values
    tree.insert(75, "Everyone!");
    tree.check_invariants();
    let agg3 = tree.aggregate(..);
    assert_eq!(agg3.size(), 3);
    assert_ne!(agg3.fingerprint(), Fingerprint::ZERO);
    assert_ne!(agg3.fingerprint(), agg1.fingerprint());
    assert_ne!(agg3.fingerprint(), agg2.fingerprint());

    tree.remove(&75);
    tree.check_invariants();
    assert_eq!(tree.aggregate(..), agg2);
}

/// `aggregate`'s cached-subtree fast path only applies once a query's bound falls strictly
/// outside a node's own key range (`fingerprint_tree_map::query`'s `lower_bound`/`upper_bound`
/// comparisons) -- a boundary a handful of hand-picked ranges can easily miss. Exhaustively
/// pairing every key with every other key on a deep enough tree (100 keys, several B-tree
/// levels at `B == 6`) forces every separator to sit at both a range start and a range end at
/// least once.
#[test]
fn aggregate_matches_brute_force_for_every_boundary_pair() {
    let entries: Vec<(u32, u32)> = (0..100).map(|k| (k, k * 7)).collect();
    let tree: FingerprintTreeMap<u32, u32> = entries.iter().copied().collect();
    tree.check_invariants();

    for lo in 0..=100u32 {
        for hi in lo..=100u32 {
            let range = lo..hi;
            let expected = entries
                .iter()
                .filter(|(k, _)| range.contains(k))
                .fold(Aggregate::ZERO, |acc, (k, v)| {
                    acc + Aggregate::new(1, lift(k, v))
                });
            assert_eq!(tree.aggregate(range.clone()), expected, "range {range:?}");
        }
    }
}

#[test]
fn contains_key_tracks_presence() {
    let mut tree = FingerprintTreeMap::new();
    assert!(!tree.contains_key(&1));
    tree.insert(1, "a");
    assert!(tree.contains_key(&1));
    assert!(!tree.contains_key(&2));
    tree.remove(&1);
    assert!(!tree.contains_key(&1));
}

#[test]
fn clear_empties_the_tree_and_preserves_invariants() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut tree: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
    for _ in 1..=50 {
        tree.insert(rng.gen(), rng.gen());
    }
    tree.check_invariants();
    assert!(!tree.is_empty());

    tree.clear();

    assert_eq!(tree.len(), 0);
    assert!(tree.is_empty());
    assert_eq!(tree.aggregate(..), Aggregate::ZERO);
    tree.check_invariants();

    tree.insert(1, 2);
    assert_eq!(tree.get(&1), Some(&2));
    tree.check_invariants();
}

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

    fn truncate_rightmost_leaf<K, V>(node: &mut super::Node<K, V>) {
        if let Some(children) = node.children.as_mut() {
            truncate_rightmost_leaf(children.last_mut().unwrap());
        } else {
            while node.keys.len() >= super::MIN_CAPACITY {
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

/// Ranges built from runtime bounds must compile: a borrowed range would be `E0716`.
#[test]
fn ranges_can_be_built_from_runtime_bounds() {
    let tree: FingerprintTreeMap<u64, u64> = (0..100).map(|k| (k, k * 3)).collect();

    let lo = *tree.select(10);
    let hi = *tree.select(20);

    let keys: Vec<u64> = tree.range(lo..hi).map(|(k, _)| *k).collect();
    assert_eq!(keys, (10..20).collect::<Vec<u64>>());
    assert_eq!(tree.aggregate(lo..hi).size(), 10);

    let bounds = (std::ops::Bound::Included(lo), std::ops::Bound::Excluded(hi));
    assert_eq!(tree.range(bounds).count(), 10);
    assert_eq!(tree.aggregate(bounds).size(), 10);

    assert_eq!(tree.range((lo + 1)..(hi - 1)).count(), 8);
}

#[test]
fn item_range_trait_stack_is_usable_through_the_reexport() {
    // Nameable via `rsos::ItemRange`, not only `rsos::fingerprint_tree_map::ItemRange` (#291).
    use crate::ItemRange;

    let tree: FingerprintTreeMap<u64, u64> = (0..30).map(|k| (k, k * 3)).collect();
    let range: ItemRange<'_, u64, u64, _> = tree.range(10..20);

    assert_eq!(range.len(), 10, "ExactSizeIterator::len");
    assert_eq!(range.size_hint(), (10, Some(10)));

    let cloned = range.clone();
    assert_eq!(
        cloned.map(|(k, _)| *k).collect::<Vec<_>>(),
        (10..20).collect::<Vec<u64>>(),
        "Clone must preserve remaining traversal state"
    );

    let debugged = format!("{:?}", tree.range(10..20));
    for k in 10..20 {
        assert!(
            debugged.contains(&k.to_string()),
            "Debug output {debugged:?} missing key {k}"
        );
    }

    // A fused iterator keeps returning `None` after exhaustion.
    let mut exhausted = tree.range(25..25);
    assert_eq!(exhausted.next(), None);
    assert_eq!(exhausted.next(), None);
    assert_eq!(exhausted.size_hint(), (0, Some(0)));
}

#[test]
fn item_range_size_hint_matches_partial_traversal() {
    let tree: FingerprintTreeMap<u64, u64> = (0..50).map(|k| (k, k)).collect();
    let mut range = tree.range(5..45);
    assert_eq!(range.size_hint(), (40, Some(40)));

    for expected_remaining in (0..40).rev() {
        range.next();
        assert_eq!(
            range.size_hint(),
            (expected_remaining, Some(expected_remaining))
        );
    }
    assert_eq!(range.next(), None);
}

#[test]
fn equality_is_content_not_shape() {
    let ascending: FingerprintTreeMap<u64, u64> = (0..200).map(|k| (k, k * 7)).collect();
    let mut descending = FingerprintTreeMap::new();
    for k in (0..200).rev() {
        descending.insert(k, k * 7);
    }
    assert_eq!(ascending, descending);
    assert_eq!(ascending.aggregate(..), descending.aggregate(..));
}

#[test]
fn equality_sees_values_and_cardinality() {
    let base: FingerprintTreeMap<u64, u64> = (0..50).map(|k| (k, k)).collect();

    let mut different_value = base.clone();
    different_value.insert(17, 999);
    assert_ne!(base, different_value);

    let mut fewer = base.clone();
    fewer.remove(&17);
    assert_ne!(base, fewer);

    fewer.insert(17, 17);
    assert_eq!(base, fewer);

    let empty_a: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
    let empty_b: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
    assert_eq!(empty_a, empty_b);
    assert_ne!(base, empty_a);
}

/// `==` must read the whole [`Aggregate`], not the fingerprint alone.
#[test]
fn equality_reads_the_whole_aggregate_not_just_the_fingerprint() {
    let a: FingerprintTreeMap<u64, u64> = (0..10).map(|k| (k, k)).collect();
    let b: FingerprintTreeMap<u64, u64> = (0..10).map(|k| (k, k)).collect();
    assert_eq!(a, b);
    assert_eq!(a.aggregate(..), b.aggregate(..));

    let fp = a.aggregate(..).fingerprint();
    assert_ne!(Aggregate::new(10, fp), Aggregate::new(9, fp));
}

#[test]
fn big_test() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut tree1 = FingerprintTreeMap::new();
    let mut key_values = Vec::new();

    let mut expected = Aggregate::ZERO;

    // add some
    for _ in 0..1000 {
        let key: u64 = rng.gen();
        let value: u64 = rng.gen();
        let old = tree1.insert(key, value);
        assert!(old.is_none());
        tree1.check_invariants();
        expected += Aggregate::new(1, lift(&key, &value));
        assert_eq!(tree1.aggregate(..), expected);
        key_values.push((key, value));
    }

    assert_eq!(tree1.get(&rng.gen()), None);
    assert_eq!(tree1.get(&key_values[0].0), Some(&key_values[0].1));

    // test get_mut
    tree1.with_mut(&rng.gen(), |v| assert_eq!(v, None));
    let key: u64 = rng.gen::<u64>();
    let value1: u64 = rng.gen();
    let value2: u64 = rng.gen();
    tree1.insert(key, value1);
    tree1.with_mut(&key, |v| *v.unwrap() = value2);
    tree1.check_invariants();
    expected += Aggregate::new(1, lift(&key, &value2));
    key_values.push((key, value2));

    // in the tree, the items should now be sorted
    key_values.sort();

    let tree2 = FingerprintTreeMap::from_iter(key_values.iter().copied());
    assert_eq!(tree1, tree2);

    // check for partial ranges
    let mid = key_values[key_values.len() / 2].0;
    assert_ne!(
        tree1.aggregate(mid..).fingerprint(),
        tree1.aggregate(..).fingerprint()
    );
    assert_ne!(
        tree1.aggregate(..mid).fingerprint(),
        tree1.aggregate(..).fingerprint()
    );
    // `⊗` over a partition of the key space reproduces the whole.
    assert_eq!(
        tree1.aggregate(..mid) + tree1.aggregate(mid..),
        tree1.aggregate(..)
    );

    for _ in 0..100 {
        let index = rng.gen::<usize>() % key_values.len();
        let key = key_values[index].0;
        assert_eq!(*tree1.select(index), key);
        assert_eq!(tree1.position(&key), Some(index));
        assert_eq!(tree1.rank(&key), index);
    }
    assert_eq!(tree1.rank(&0), 0);
    assert_eq!(tree1.rank(&u64::MAX), tree1.len());

    // test range
    let from_index = rng.gen_range(0..key_values.len());
    let to_index = rng.gen_range(from_index..key_values.len());
    let from_key = tree1.select(from_index);
    let to_key = tree1.select(to_index);
    fn test_range<
        R: RangeBounds<u64>,
        SI: std::slice::SliceIndex<[(u64, u64)], Output = [(u64, u64)]>,
    >(
        key_values: &[(u64, u64)],
        tree: &FingerprintTreeMap<u64, u64>,
        range: R,
        slice_index: SI,
    ) {
        assert_eq!(
            tree.range(range).map(|(k, v)| (*k, *v)).collect::<Vec<_>>(),
            key_values[slice_index]
        );
    }
    test_range(&key_values, &tree1, from_key..to_key, from_index..to_index);
    test_range(
        &key_values,
        &tree1,
        from_key..=to_key,
        from_index..=to_index,
    );
    test_range(&key_values, &tree1, ..to_key, ..to_index);
    test_range(&key_values, &tree1, ..=to_key, ..=to_index);
    test_range(&key_values, &tree1, from_key.., from_index..);
    test_range(&key_values, &tree1, .., ..);

    // remove everything one-by-one
    key_values.shuffle(&mut rng);
    for (key, value) in key_values {
        let value2 = tree1.remove(&key);
        tree1.check_invariants();
        assert_eq!(value2, Some(value));
        expected = Aggregate::new(
            expected.size() - 1,
            expected.fingerprint() - lift(&key, &value),
        );
        assert_eq!(tree1.aggregate(..), expected);
    }
}

/// Both halves of the aggregate must agree with an independent walk of `range`.
#[test]
fn aggregate_count_matches_range_count() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut tree: FingerprintTreeMap<u32, u32> = FingerprintTreeMap::new();
    let mut keys = Vec::new();
    for _ in 0..75 {
        let key: u32 = rng.gen();
        let value: u32 = rng.gen();
        tree.insert(key, value);
        keys.push(key);
    }
    keys.sort_unstable();
    tree.check_invariants();

    let check = |range: &dyn Fn() -> (std::ops::Bound<u32>, std::ops::Bound<u32>)| {
        let range = range();
        let aggregate = tree.aggregate(range);
        assert_eq!(
            aggregate.size(),
            tree.range(range).count(),
            "aggregate size disagrees with range().count() for {range:?}"
        );
        assert_eq!(
            aggregate.is_empty(),
            tree.range(range).next().is_none(),
            "aggregate is_empty disagrees with range() for {range:?}"
        );
        let expected_fingerprint = tree
            .range(range)
            .fold(Fingerprint::ZERO, |acc, (k, v)| acc + lift(k, v));
        assert_eq!(
            aggregate.fingerprint(),
            expected_fingerprint,
            "aggregate fingerprint disagrees with the summed element lifts for {range:?}"
        );
    };

    // empty range: nothing between a key and itself, excluded
    let mid = keys[keys.len() / 2];
    check(&|| {
        (
            std::ops::Bound::Included(mid),
            std::ops::Bound::Excluded(mid),
        )
    });
    // full range
    check(&|| (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded));
    // partial ranges
    let lo = keys[keys.len() / 4];
    let hi = keys[3 * keys.len() / 4];
    check(&|| (std::ops::Bound::Included(lo), std::ops::Bound::Excluded(hi)));
    check(&|| (std::ops::Bound::Excluded(lo), std::ops::Bound::Included(hi)));
    check(&|| (std::ops::Bound::Unbounded, std::ops::Bound::Excluded(hi)));
    check(&|| (std::ops::Bound::Included(lo), std::ops::Bound::Unbounded));
    // an empty tree
    let empty: FingerprintTreeMap<u32, u32> = FingerprintTreeMap::new();
    assert_eq!(empty.aggregate(..), Aggregate::ZERO);
}
