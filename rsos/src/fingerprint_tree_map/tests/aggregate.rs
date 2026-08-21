// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use rand::{Rng, SeedableRng};

use crate::aggregate::Aggregate;
use crate::fingerprint::{lift, Fingerprint};

use super::super::FingerprintTreeMap;

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
