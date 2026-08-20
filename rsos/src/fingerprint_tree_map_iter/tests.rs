// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use rand::{Rng, SeedableRng};

use crate::fingerprint_tree_map::FingerprintTreeMap;
use once_cell::sync::Lazy;

const TREE_SIZE: usize = 1000;

static BASE_ITEMS: Lazy<Vec<(u64, u64)>> = Lazy::new(|| {
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    (0..TREE_SIZE)
        .map(|i| (i as u64, rng.gen::<u64>()))
        .collect()
});

fn make_tree() -> FingerprintTreeMap<u64, u64> {
    FingerprintTreeMap::from_iter(BASE_ITEMS.clone())
}

#[test]
fn test_into_iter() {
    let tree = make_tree();
    assert_eq!(
        tree.clone().into_iter().collect::<Vec<_>>(),
        BASE_ITEMS.clone()
    );
}

#[test]
fn test_iter() {
    let tree = make_tree();
    assert_eq!(
        tree.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(),
        BASE_ITEMS.clone()
    );
}

#[test]
fn test_iter_mut() {
    let mut tree = make_tree();
    let collected: Vec<_> = tree.iter_mut().map(|(_, v)| *v).collect();
    let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
    tree.check_invariants();
    assert_eq!(collected, expected);
}

#[test]
fn test_iter_mut_modify() {
    let mut tree = make_tree();

    let num = rand::random::<usize>().rem_euclid(TREE_SIZE);
    let (key, value) = BASE_ITEMS[num];
    let mut expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
    expected[num] = value;

    for (k, v) in tree.iter_mut() {
        if *k == key {
            *v = value;
        }
    }
    let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
    assert_eq!(collected, expected);
}

#[test]
fn test_into_values() {
    let values: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
    let tree = make_tree();
    assert_eq!(tree.clone().into_values().collect::<Vec<_>>(), values);
}

#[test]
fn test_values() {
    let values: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
    let tree = make_tree();
    assert_eq!(tree.values().copied().collect::<Vec<_>>(), values);
}

#[test]
fn test_values_mut() {
    let mut tree = make_tree();
    let collected: Vec<_> = tree.values_mut().map(|v| *v).collect();
    let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
    tree.check_invariants();
    assert_eq!(collected, expected);
}

#[test]
fn test_values_mut_modify() {
    let mut tree = make_tree();

    let num = rand::random::<usize>().rem_euclid(TREE_SIZE);
    let (_, value) = BASE_ITEMS[num];
    let mut expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
    expected[num] = value;

    for (n, v) in tree.values_mut().enumerate() {
        if n == num {
            *v = value;
        }
    }
    let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
    assert_eq!(collected, expected);
}

#[test]
fn test_with_mut_maintains_invariants() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(99);
    let mut tree = make_tree();
    tree.check_invariants();

    let aggregate_before = tree.aggregate(..);

    for _ in 0..20 {
        let idx = rng.gen_range(0..TREE_SIZE);
        let (key, _) = BASE_ITEMS[idx];
        let new_value: u64 = rng.gen();
        tree.with_mut(&key, |v| *v.unwrap() = new_value);
        tree.check_invariants();
    }

    // `with_mut` overwrites in place: the fingerprint moves, the count must not.
    let aggregate_after = tree.aggregate(..);
    assert_ne!(
        aggregate_before.fingerprint(),
        aggregate_after.fingerprint(),
        "tree fingerprint unchanged after with_mut mutations — fingerprints not updated"
    );
    assert_eq!(
        aggregate_before.size(),
        aggregate_after.size(),
        "with_mut changed the element count"
    );

    let mid = BASE_ITEMS[TREE_SIZE / 2].0;
    assert_eq!(
        tree.aggregate(..mid) + tree.aggregate(mid..),
        tree.aggregate(..),
        "partial-range aggregates do not compose into the global aggregate"
    );
    assert_eq!(tree.aggregate(..), aggregate_after);
}

#[test]
fn test_into_keys() {
    let keys: Vec<_> = BASE_ITEMS.iter().map(|&(k, _)| k).collect();
    let tree = make_tree();
    assert_eq!(tree.clone().into_keys().collect::<Vec<_>>(), keys);
}

#[test]
fn test_keys() {
    let keys: Vec<_> = BASE_ITEMS.iter().map(|&(k, _)| k).collect();
    let tree = make_tree();
    assert_eq!(tree.keys().copied().collect::<Vec<_>>(), keys);
}

#[test]
fn test_all_iterators_empty() {
    let empty: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
    // immutable
    assert_eq!(empty.iter().next(), None);
    // consuming
    assert!(empty.clone().into_iter().next().is_none());
    assert!(empty.clone().into_keys().next().is_none());
    assert!(empty.clone().into_values().next().is_none());
    // shared
    assert!(empty.keys().next().is_none());
    assert!(empty.values().next().is_none());
    let mut empty_mut = empty.clone();
    assert!(empty_mut.iter_mut().next().is_none());
    assert!(empty_mut.values_mut().next().is_none());
    empty_mut.check_invariants();
}

#[test]
fn test_all_iterators_single_leaf() {
    let mut single = FingerprintTreeMap::new();
    single.insert(42, 99);
    single.check_invariants();
    // immutable
    assert_eq!(single.iter().collect::<Vec<_>>(), vec![(&42, &99)]);
    // consuming
    assert_eq!(
        single.clone().into_iter().collect::<Vec<_>>(),
        vec![(42, 99)]
    );
    assert_eq!(single.clone().into_keys().collect::<Vec<_>>(), vec![42]);
    assert_eq!(single.clone().into_values().collect::<Vec<_>>(), vec![99]);
    // shared
    assert_eq!(single.keys().copied().collect::<Vec<_>>(), vec![42]);
    assert_eq!(single.values().copied().collect::<Vec<_>>(), vec![99]);
    single.with_mut(&42, |v| *v.unwrap() += 1);
    single.check_invariants();
    assert_eq!(single.iter().collect::<Vec<_>>(), vec![(&42, &100)]);
    single.with_mut(&42, |v| *v.unwrap() *= 2);
    single.check_invariants();
    assert_eq!(single.values().copied().collect::<Vec<_>>(), vec![200]);
}

#[test]
fn test_len_and_size_hint_exact() {
    let tree = make_tree();
    macro_rules! assert_exact_len {
        ($mk:expr) => {{
            let mut it = $mk;
            let mut remaining = TREE_SIZE;
            assert_eq!(it.len(), remaining);
            assert_eq!(it.size_hint(), (remaining, Some(remaining)));
            while it.next().is_some() {
                remaining -= 1;
                assert_eq!(it.len(), remaining);
                assert_eq!(it.size_hint(), (remaining, Some(remaining)));
            }
            assert_eq!(it.len(), 0);
        }};
    }
    assert_exact_len!(tree.iter());
    assert_exact_len!(tree.keys());
    assert_exact_len!(tree.values());
    assert_exact_len!(tree.clone().into_iter());
    assert_exact_len!(tree.clone().into_keys());
    assert_exact_len!(tree.clone().into_values());
}

#[test]
fn test_len_and_size_hint_exact_empty() {
    let empty: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
    assert_eq!(empty.iter().len(), 0);
    assert_eq!(empty.iter().size_hint(), (0, Some(0)));
    assert_eq!(empty.keys().len(), 0);
    assert_eq!(empty.values().len(), 0);
    assert_eq!(empty.clone().into_iter().len(), 0);
    assert_eq!(empty.clone().into_keys().len(), 0);
    assert_eq!(empty.into_values().len(), 0);
}

#[test]
fn test_fused_after_exhaustion() {
    let tree = make_tree();
    macro_rules! assert_fused {
        ($mk:expr) => {{
            let mut it = $mk;
            while it.next().is_some() {}
            assert!(it.next().is_none());
            assert!(it.next().is_none());
            assert!(it.next().is_none());
        }};
    }
    assert_fused!(tree.iter());
    assert_fused!(tree.keys());
    assert_fused!(tree.values());
    assert_fused!(tree.clone().into_iter());
    assert_fused!(tree.clone().into_keys());
    assert_fused!(tree.clone().into_values());

    let empty: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
    assert_fused!(empty.iter());
    assert_fused!(empty.clone().into_iter());
}

#[test]
fn test_clone_and_debug() {
    let tree = make_tree();

    let iter = tree.iter();
    let cloned: Vec<_> = iter.clone().collect();
    assert_eq!(cloned, iter.collect::<Vec<_>>());

    let into_iter = tree.clone().into_iter();
    let cloned_into: Vec<_> = into_iter.clone().collect();
    assert_eq!(cloned_into, into_iter.collect::<Vec<_>>());

    assert_eq!(
        format!("{:?}", tree.iter()),
        format!(
            "{:?}",
            BASE_ITEMS.iter().map(|(k, v)| (k, v)).collect::<Vec<_>>()
        )
    );
    assert_eq!(
        format!("{:?}", tree.keys()),
        format!(
            "{:?}",
            BASE_ITEMS.iter().map(|(k, _)| k).collect::<Vec<_>>()
        )
    );
    assert_eq!(
        format!("{:?}", tree.values()),
        format!(
            "{:?}",
            BASE_ITEMS.iter().map(|(_, v)| v).collect::<Vec<_>>()
        )
    );
    assert_eq!(
        format!("{:?}", tree.clone().into_iter()),
        format!("{:?}", BASE_ITEMS.clone())
    );
    assert_eq!(
        format!("{:?}", tree.clone().into_keys()),
        format!(
            "{:?}",
            BASE_ITEMS.iter().map(|&(k, _)| k).collect::<Vec<_>>()
        )
    );
    assert_eq!(
        format!("{:?}", tree.into_values()),
        format!(
            "{:?}",
            BASE_ITEMS.iter().map(|&(_, v)| v).collect::<Vec<_>>()
        )
    );
}

#[test]
fn test_standard_adapters() {
    let tree = make_tree();

    let peeked: Vec<_> = {
        let mut it = tree.iter().peekable();
        let mut out = Vec::new();
        while let Some(&(&k, &v)) = it.peek() {
            out.push((k, v));
            it.next();
        }
        out
    };
    assert_eq!(peeked, BASE_ITEMS.clone());

    let taken: Vec<_> = tree
        .iter()
        .take(10)
        .map(|(&k, &v)| (k, v))
        .collect::<Vec<_>>();
    assert_eq!(taken, BASE_ITEMS[..10].to_vec());

    let skipped: Vec<_> = tree
        .iter()
        .skip(TREE_SIZE - 10)
        .map(|(&k, &v)| (k, v))
        .collect::<Vec<_>>();
    assert_eq!(skipped, BASE_ITEMS[TREE_SIZE - 10..].to_vec());

    let mid = tree
        .iter()
        .skip(100)
        .take(10)
        .map(|(&k, &v)| (k, v))
        .collect::<Vec<_>>();
    assert_eq!(mid, BASE_ITEMS[100..110].to_vec());

    assert_eq!(tree.iter().count(), TREE_SIZE);
    let mut preallocated: Vec<_> = Vec::with_capacity(tree.iter().len());
    preallocated.extend(tree.iter().map(|(&k, &v)| (k, v)));
    assert_eq!(preallocated, BASE_ITEMS.clone());
}
