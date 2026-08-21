// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::super::FingerprintTreeMap;

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
