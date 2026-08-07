use std::hash::Hash;
use std::ops::Bound;

use rbsr::{diff_round, start_diff, DiffRange};
use rsos::FingerprintTreeMap;

/// Run the full diff exchange between two trees, returning `(local_owes, remote_owes)`: the
/// ranges `local` must send to `remote` and vice-versa. Drives the anti-entropy algorithm
/// directly on the standalone `rbsr` crate, where these items are genuinely public — no
/// `reconcile::testing` detour needed.
pub fn diff<K, V>(
    local: &FingerprintTreeMap<K, V>,
    remote: &FingerprintTreeMap<K, V>,
) -> (Vec<DiffRange<K>>, Vec<DiffRange<K>>)
where
    K: Clone + Hash + Ord,
    V: Hash,
{
    let mut local_diff_ranges = Vec::new();
    let mut remote_diff_ranges = Vec::new();
    let mut local_segments = start_diff(local);
    let mut remote_segments = Vec::new();
    while !local_segments.is_empty() {
        diff_round(
            remote,
            std::mem::take(&mut local_segments),
            &mut remote_segments,
            &mut remote_diff_ranges,
        );
        diff_round(
            local,
            std::mem::take(&mut remote_segments),
            &mut local_segments,
            &mut local_diff_ranges,
        );
    }
    (local_diff_ranges, remote_diff_ranges)
}

pub fn reconcile<K, V>(local: &mut FingerprintTreeMap<K, V>, remote: &mut FingerprintTreeMap<K, V>)
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    let (diff_ranges1, diff_ranges2) = diff(local, remote);
    for diff in diff_ranges1 {
        for (k, v) in local.range(&diff) {
            remote.insert(k.clone(), v.clone());
        }
    }
    for diff in diff_ranges2 {
        for (k, v) in remote.range(&diff) {
            local.insert(k.clone(), v.clone());
        }
    }
}

#[test]
fn test_compare() {
    let tree1 = FingerprintTreeMap::from_iter([(25, "World!"), (50, "Hello"), (75, "Everyone!")]);
    let tree2 = FingerprintTreeMap::from_iter([(75, "Everyone!"), (50, "Hello"), (25, "World!")]);
    let tree3 = FingerprintTreeMap::from_iter([(75, "Everyone!"), (25, "World!"), (50, "Hello")]);
    let tree4 = FingerprintTreeMap::from_iter([(75, "Everyone!"), (25, "World!"), (40, "Hello")]);
    let tree5 = FingerprintTreeMap::from_iter([(25, "World!"), (50, "Hello"), (75, "Goodbye!")]);

    assert_eq!(tree1.aggregate(&..), tree1.aggregate(&..));
    assert_eq!(tree1.aggregate(&..), tree2.aggregate(&..));
    assert_eq!(tree1.aggregate(&..), tree3.aggregate(&..));
    assert_ne!(tree1.aggregate(&..), tree4.aggregate(&..));
    assert_ne!(tree1.aggregate(&..), tree5.aggregate(&..));

    assert_eq!(tree1, tree1);
    assert_eq!(tree1, tree2);
    assert_eq!(tree1, tree3);
    assert_ne!(tree1, tree4);
    assert_ne!(tree1, tree5);

    assert_eq!(diff(&tree1, &tree1), (vec![], vec![]));
    assert_eq!(diff(&tree1, &tree2), (vec![], vec![]));
    assert_eq!(diff(&tree1, &tree3), (vec![], vec![]));
    assert_eq!(
        diff(&tree1, &tree4),
        (
            vec![(Bound::Included(40), Bound::Excluded(75))],
            vec![(Bound::Included(40), Bound::Excluded(75))],
        ),
    );
    assert_eq!(
        diff(&tree1, &tree5),
        (
            vec![(Bound::Included(75), Bound::Unbounded)],
            vec![(Bound::Included(75), Bound::Unbounded)],
        ),
    );

    let mut tree1 = tree1;
    let mut tree4 = tree4;
    reconcile(&mut tree1, &mut tree4);
    assert_eq!(tree1, tree4);
    assert_eq!(
        tree1.range(&..).collect::<Vec<_>>(),
        [
            (&25, &"World!"),
            (&40, &"Hello"),
            (&50, &"Hello"),
            (&75, &"Everyone!")
        ]
    )
}

// The size-not-hash regression tests — a *non-empty* range that
// fingerprints to `ZERO`, and equal fingerprints over different-sized ranges — require feeding
// crafted `RangeAggregate`s (whose bundled `Aggregate` is deliberately set to collide) straight
// into `diff_round`. Because `RangeAggregate`'s fields are private, those tests live as unit
// tests in `rbsr/src/diff.rs` (`nonempty_zero_fingerprint_vs_empty_is_not_in_sync` and friends),
// next to the algorithm they guard, rather than here.
