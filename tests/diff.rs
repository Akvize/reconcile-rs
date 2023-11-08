use std::hash::Hash;
use std::ops::Bound;

use reconcile::diff::{DiffRange, Diffable, HashRangeQueryable, HashSegment};
use reconcile::hrtree::HRTree;

pub fn diff<K, D: Diffable<ComparisonItem = HashSegment<K>, DifferenceItem = DiffRange<K>>>(
    local: &D,
    remote: &D,
) -> (Vec<DiffRange<K>>, Vec<DiffRange<K>>) {
    let mut local_diff_ranges = Vec::new();
    let mut remote_diff_ranges = Vec::new();
    let mut local_segments = local.start_diff();
    let mut remote_segments = Vec::new();
    while !local_segments.is_empty() {
        remote.diff_round(
            local_segments.drain(..).collect(),
            &mut remote_segments,
            &mut remote_diff_ranges,
        );
        local.diff_round(
            remote_segments.drain(..).collect(),
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

    assert_eq!(tree1.hash(&..), tree1.hash(&..));
    assert_eq!(tree1.hash(&..), tree2.hash(&..));
    assert_eq!(tree1.hash(&..), tree3.hash(&..));
    assert_ne!(tree1.hash(&..), tree4.hash(&..));
    assert_ne!(tree1.hash(&..), tree5.hash(&..));

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

    /*
    let range = tree1.get_range(&(Bound::Included(40), Bound::Excluded(50)));
    assert_eq!(range.collect::<Vec<_>>(), vec![]);
    let range = tree1.get_range(&(Bound::Included(50), Bound::Excluded(75)));
    assert_eq!(range.collect::<Vec<_>>(), vec![(&50, &"Hello")]);
    let range = tree4.get_range(&(Bound::Included(40), Bound::Excluded(50)));
    assert_eq!(range.collect::<Vec<_>>(), vec![(&40, &"Hello")]);
    let range = tree4.get_range(&(Bound::Included(50), Bound::Excluded(75)));
    assert_eq!(range.collect::<Vec<_>>(), vec![]);
    */

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
