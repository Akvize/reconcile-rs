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
use crate::fingerprint::lift;

use super::super::FingerprintTreeMap;

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

/// A `String`-keyed map's point-query methods accept `&str` directly (`K: Borrow<Q>`), the
/// same shape `BTreeMap` offers — without this, every lookup on a string-keyed map would have
/// to allocate an owned `String` just to satisfy `&K`.
#[test]
fn borrowed_key_lookup_matches_owned_key_lookup() {
    let mut map: FingerprintTreeMap<String, u32> = FingerprintTreeMap::new();
    map.insert("a".to_string(), 1);
    map.insert("b".to_string(), 2);
    map.insert("c".to_string(), 3);

    assert_eq!(map.get("b"), Some(&2));
    assert!(map.contains_key("b"));
    assert_eq!(map.position("b"), Some(1));
    assert_eq!(map.rank("b"), 1);
    assert_eq!(map.rank("ba"), 2);

    assert_eq!(map.remove("b"), Some(2));
    assert_eq!(map.get("b"), None);
    assert!(!map.contains_key("b"));
    map.check_invariants();
}

/// `or_default` inserts `V::default()` on a vacant entry, and leaves an occupied one alone;
/// `and_modify` mutates only an occupied entry, in-place, and re-lifts the fingerprint.
#[test]
fn entry_or_default_and_and_modify() {
    let mut map: FingerprintTreeMap<&str, u32> = FingerprintTreeMap::new();

    // Vacant: or_default inserts the type's default.
    assert_eq!(*map.entry("a").or_default(), 0);
    assert_eq!(map.get("a"), Some(&0));

    // Occupied: and_modify runs, or_default does not overwrite.
    map.insert("b", 5);
    let before = map.aggregate(..);
    assert_eq!(*map.entry("b").and_modify(|v| *v += 1).or_default(), 6);
    assert_eq!(map.get("b"), Some(&6));
    assert_ne!(before.fingerprint(), map.aggregate(..).fingerprint());

    // Vacant: and_modify is a no-op, or_default still inserts.
    assert_eq!(*map.entry("c").and_modify(|v| *v += 100).or_default(), 0);
    assert_eq!(map.get("c"), Some(&0));

    map.check_invariants();
}

/// `Debug` must actually render the map's contents, not just satisfy the trait.
#[test]
fn debug_format_shows_every_entry() {
    let mut map: FingerprintTreeMap<i32, &str> = FingerprintTreeMap::new();
    map.insert(1, "a");
    map.insert(2, "b");
    let rendered = format!("{map:?}");
    assert!(rendered.contains('1') && rendered.contains("\"a\""));
    assert!(rendered.contains('2') && rendered.contains("\"b\""));
}
