#![cfg(reconcile_internal_testing)]

use std::hash::Hash;
use std::ops::Bound;

use reconcile::hrtree::HRTree;
use reconcile::testing::{diff_round, range_hash, start_diff, DiffRange};

/// Run the full diff exchange between two trees, returning `(local_owes, remote_owes)`: the
/// ranges `local` must send to `remote` and vice-versa. Drives the `pub(crate)` anti-entropy
/// protocol through the gated `reconcile::testing` seam.
pub fn diff<K, V>(
    local: &HRTree<K, V>,
    remote: &HRTree<K, V>,
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

pub fn reconcile<K, V>(local: &mut HRTree<K, V>, remote: &mut HRTree<K, V>)
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    let (diff_ranges1, diff_ranges2) = diff(local, remote);
    for diff in diff_ranges1 {
        for (k, v) in local.get_range(&diff) {
            remote.insert(k.clone(), v.clone());
        }
    }
    for diff in diff_ranges2 {
        for (k, v) in remote.get_range(&diff) {
            local.insert(k.clone(), v.clone());
        }
    }
}

#[test]
fn test_compare() {
    let tree1 = HRTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Everyone!")]);
    let tree2 = HRTree::from_iter([(75, "Everyone!"), (50, "Hello"), (25, "World!")]);
    let tree3 = HRTree::from_iter([(75, "Everyone!"), (25, "World!"), (50, "Hello")]);
    let tree4 = HRTree::from_iter([(75, "Everyone!"), (25, "World!"), (40, "Hello")]);
    let tree5 = HRTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Goodbye!")]);

    assert_eq!(range_hash(&tree1, &..), range_hash(&tree1, &..));
    assert_eq!(range_hash(&tree1, &..), range_hash(&tree2, &..));
    assert_eq!(range_hash(&tree1, &..), range_hash(&tree3, &..));
    assert_ne!(range_hash(&tree1, &..), range_hash(&tree4, &..));
    assert_ne!(range_hash(&tree1, &..), range_hash(&tree5, &..));

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
        tree1.get_range(&..).collect::<Vec<_>>(),
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
// crafted `HashSegment`s (whose `hash`/`size` fields are deliberately set to collide) straight
// into `diff_round`. Because `HashSegment`'s fields are `pub(crate)`, those tests live as unit
// tests in `src/proto.rs` (`nonempty_zero_hash_vs_empty_is_not_in_sync` and friends), next to
// the algorithm they guard, rather than here.
