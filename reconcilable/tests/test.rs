use std::hash::Hash;
use std::ops::Bound;

use diff::{Diffable, Diffs, HashRangeQueryable};
use htree::HTree;

pub fn diff<K, D: Diffable<Key = K>>(local: &D, remote: &D) -> (Diffs<K>, Diffs<K>) {
    let mut local_diffs = Vec::new();
    let mut remote_diffs = Vec::new();
    let mut segments = local.start_diff();
    while !segments.is_empty() {
        segments = remote.diff_round(&mut remote_diffs, segments);
        segments = local.diff_round(&mut local_diffs, segments);
    }
    (local_diffs, remote_diffs)
}

pub fn reconcile<K, V>(local: &mut HTree<K, V>, remote: &mut HTree<K, V>)
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    let (diffs1, diffs2) = diff(local, remote);
    for diff in diffs1 {
        for (k, v) in local.get_range(&diff) {
            remote.insert(k.clone(), v.clone());
        }
    }
    for diff in diffs2 {
        for (k, v) in remote.get_range(&diff) {
            local.insert(k.clone(), v.clone());
        }
    }
}

#[test]
fn test_compare() {
    let tree1 = HTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Everyone!")]);
    let tree2 = HTree::from_iter([(75, "Everyone!"), (50, "Hello"), (25, "World!")]);
    let tree3 = HTree::from_iter([(75, "Everyone!"), (25, "World!"), (50, "Hello")]);
    let tree4 = HTree::from_iter([(75, "Everyone!"), (25, "World!"), (40, "Hello")]);
    let tree5 = HTree::from_iter([(25, "World!"), (50, "Hello"), (75, "Goodbye!")]);

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
