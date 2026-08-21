// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Property 1: equivalence to a `BTreeMap` oracle, with every invariant holding after every
//! mutation.

use std::collections::BTreeMap;
use std::ops::Bound;

use proptest::prelude::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use rsos::{lift, Fingerprint, FingerprintTreeMap};

#[derive(Clone, Debug)]
enum Op {
    Insert(u8, u16),
    Remove(u8),
    Get(u8),
    ContainsKey(u8),
    /// Keep entries whose value is below this threshold — exercises `retain` with both a
    /// no-op (`u16::MAX`, keeps everything) and an aggressive cutoff (drops most entries).
    Retain(u16),
    Clear,
    /// Mutate the value at a key in place through `with_mut`, the hash-safe mutation path.
    WithMut(u8, u16),
    /// The same, with a callback that mutates and then panics: the caught panic must leave every
    /// cached aggregate describing what is actually stored, interleaved with splits and merges.
    WithMutPanicking(u8, u16),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    // Small key space so `remove`/`get` hit often; `Clear`/`Retain` weighted low so a non-trivial
    // tree still builds between them.
    prop_oneof![
        6 => (any::<u8>(), any::<u16>()).prop_map(|(k, v)| Op::Insert(k, v)),
        6 => any::<u8>().prop_map(Op::Remove),
        6 => any::<u8>().prop_map(Op::Get),
        6 => any::<u8>().prop_map(Op::ContainsKey),
        1 => any::<u16>().prop_map(Op::Retain),
        1 => Just(Op::Clear),
        4 => (any::<u8>(), any::<u16>()).prop_map(|(k, v)| Op::WithMut(k, v)),
        2 => (any::<u8>(), any::<u16>()).prop_map(|(k, v)| Op::WithMutPanicking(k, v)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn fingerprint_tree_map_matches_btreemap_oracle(ops in prop::collection::vec(op_strategy(), 0..400)) {
        let mut tree: FingerprintTreeMap<u8, u16> = FingerprintTreeMap::new();
        let mut oracle: BTreeMap<u8, u16> = BTreeMap::new();

        for op in ops {
            match op {
                Op::Insert(k, v) => {
                    prop_assert_eq!(tree.insert(k, v), oracle.insert(k, v));
                }
                Op::Remove(k) => {
                    prop_assert_eq!(tree.remove(&k), oracle.remove(&k));
                }
                Op::Get(k) => {
                    prop_assert_eq!(tree.get(&k), oracle.get(&k));
                }
                Op::ContainsKey(k) => {
                    prop_assert_eq!(tree.contains_key(&k), oracle.contains_key(&k));
                }
                Op::Retain(threshold) => {
                    tree.retain(|_, v| *v < threshold);
                    oracle.retain(|_, v| *v < threshold);
                }
                Op::Clear => {
                    tree.clear();
                    oracle.clear();
                }
                Op::WithMut(k, v) => {
                    let present = tree.with_mut(&k, |slot| match slot {
                        Some(value) => {
                            *value = v;
                            true
                        }
                        None => false,
                    });
                    match oracle.get_mut(&k) {
                        Some(value) => {
                            *value = v;
                            prop_assert!(present, "with_mut saw {k} as absent, oracle has it");
                        }
                        None => prop_assert!(!present, "with_mut saw {k} as present, oracle has not"),
                    }
                }
                Op::WithMutPanicking(k, v) => {
                    // The callback mutates, then unwinds. `with_mut`'s repair runs from a `Drop`
                    // guard, so the aggregates must end up describing the *mutated* value — which
                    // is why the oracle applies the same write.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tree.with_mut(&k, |slot| {
                            if let Some(value) = slot {
                                *value = v;
                                panic!("deliberate panic inside with_mut");
                            }
                        })
                    }));
                    if let Some(value) = oracle.get_mut(&k) {
                        *value = v;
                        prop_assert!(outcome.is_err(), "the callback's panic must propagate");
                    } else {
                        // Absent key: the callback never panics, and nothing was mutated.
                        prop_assert!(outcome.is_ok());
                    }
                }
            }
            // The core safety net: structural, ordering, height, size and hash
            // caches must all hold after *every* mutation. Panics here would be
            // the rebalancing bugs the issue is worried about.
            tree.check_invariants();
            prop_assert_eq!(tree.len(), oracle.len());
        }

        // Full-range iteration yields the exact sorted oracle contents.
        let got: Vec<(u8, u16)> = tree.range(..).map(|(k, v)| (*k, *v)).collect();
        let want: Vec<(u8, u16)> = oracle.iter().map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(got, want);

        // The cumulated fingerprint equals the sum of the per-element lifts
        // (and is order-independent), matching the diff protocol's fingerprint.
        let expected_fingerprint = oracle
            .iter()
            .fold(Fingerprint::ZERO, |acc, (k, v)| acc + lift(k, v));
        prop_assert_eq!(tree.aggregate(..).fingerprint(), expected_fingerprint);
    }

    #[test]
    fn fingerprint_tree_map_range_queries_match_oracle(
        entries in prop::collection::vec((any::<u8>(), any::<u16>()), 0..200),
        lo in any::<u8>(),
        hi in any::<u8>(),
    ) {
        let mut tree: FingerprintTreeMap<u8, u16> = FingerprintTreeMap::new();
        let mut oracle: BTreeMap<u8, u16> = BTreeMap::new();
        for (k, v) in entries {
            tree.insert(k, v);
            oracle.insert(k, v);
        }
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let range = (Bound::Included(lo), Bound::Excluded(hi));

        let got: Vec<(u8, u16)> = tree.range(range).map(|(k, v)| (*k, *v)).collect();
        let want: Vec<(u8, u16)> = oracle.range(range).map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(&got, &want);

        // Range fingerprint is consistent with iterating the same range.
        let expected = want
            .iter()
            .fold(Fingerprint::ZERO, |acc, (k, v)| acc + lift(k, v));
        prop_assert_eq!(tree.aggregate(range).fingerprint(), expected);
    }

    /// `==` compares content, never tree shape. The size half of the aggregate is only reachable
    /// on `Aggregate` itself, asserted in `rsos`'s unit tests.
    #[test]
    fn fingerprint_tree_map_equality_is_content_not_shape(
        entries in prop::collection::vec((any::<u8>(), any::<u16>()), 0..200),
        seed in any::<u64>(),
    ) {
        // Later inserts overwrite, so the oracle defines the actual content.
        let mut oracle: BTreeMap<u8, u16> = BTreeMap::new();
        for (k, v) in &entries {
            oracle.insert(*k, *v);
        }

        let ascending: FingerprintTreeMap<u8, u16> = oracle.iter().map(|(k, v)| (*k, *v)).collect();
        let mut shuffled: Vec<(u8, u16)> = oracle.iter().map(|(k, v)| (*k, *v)).collect();
        shuffled.shuffle(&mut StdRng::seed_from_u64(seed));
        let mut arbitrary_order = FingerprintTreeMap::new();
        for (k, v) in shuffled {
            arbitrary_order.insert(k, v);
        }

        // Same elements, different insertion orders and in general different node layouts.
        prop_assert_eq!(&ascending, &arbitrary_order);

        // Any single-element difference is visible, in either half.
        if let Some((&k, &v)) = oracle.iter().next() {
            let mut one_fewer = arbitrary_order.clone();
            one_fewer.remove(&k);
            prop_assert_ne!(&ascending, &one_fewer);

            let mut one_changed = arbitrary_order.clone();
            one_changed.insert(k, v.wrapping_add(1));
            prop_assert_ne!(&ascending, &one_changed);
        }
    }

    /// A retrying/reordering transport can deliver a record the store already holds a second
    /// time. `insert`'s existing-key branch (`rsos/src/fingerprint_tree_map.rs:438-446`) applies
    /// a signed `new_fp - old_fp` delta to the cached aggregate rather than blindly combining the
    /// new lift in, so re-delivering an unchanged `(key, value)` pair must contribute a zero
    /// delta — the aggregate is bit-for-bit unchanged, not merely numerically close.
    #[test]
    fn duplicate_delivery_of_an_already_held_record_leaves_the_aggregate_unchanged(
        entries in prop::collection::vec((any::<u8>(), any::<u16>()), 1..200),
        pick in any::<usize>(),
    ) {
        let mut tree: FingerprintTreeMap<u8, u16> = FingerprintTreeMap::new();
        for (k, v) in &entries {
            tree.insert(*k, *v);
        }
        let keys: Vec<u8> = tree.range(..).map(|(k, _)| *k).collect();
        let key = keys[pick % keys.len()];
        let value = *tree.get(&key).unwrap();

        let before = tree.aggregate(..);
        let replaced = tree.insert(key, value);
        let after = tree.aggregate(..);

        prop_assert_eq!(replaced, Some(value), "the key must already have been present");
        prop_assert_eq!(
            before, after,
            "re-delivering an unchanged record must not move the aggregate"
        );
        tree.check_invariants();
    }
}
