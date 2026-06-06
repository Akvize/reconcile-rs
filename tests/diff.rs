#![cfg(feature = "internal-testing")]

use std::hash::Hash;
use std::ops::{Bound, RangeBounds};

use reconcile::fingerprint::Fingerprint;
use reconcile::hrtree::HRTree;
use reconcile::testing::{DiffRange, Diffable, HashRangeQueryable, HashSegment};

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
            std::mem::take(&mut local_segments),
            &mut remote_segments,
            &mut remote_diff_ranges,
        );
        local.diff_round(
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

/// Per-element hash with a *forced collision*: keys `1` and `2` both hash to the
/// same value, so any range containing exactly `{1, 2}` XORs to `0` even though it
/// is non-empty. Every other key hashes to a distinct, non-zero value.
///
/// This lets us deterministically reproduce issue #106 (the `hash == 0` sentinel
/// aliasing a non-empty range) without relying on a real, non-portable
/// `DefaultHasher` collision.
fn elem_hash(key: i32) -> Fingerprint {
    match key {
        // Keys 1 and 2 are crafted to cancel under the additive combiner:
        // `elem_hash(1) + elem_hash(2) == ZERO (mod 2²⁵⁶)`, so the non-empty
        // range {1, 2} fingerprints to ZERO — exactly the issue #106 hazard.
        1 => Fingerprint([7, 0, 0, 0]),
        2 => -Fingerprint([7, 0, 0, 0]),
        k => {
            let m = (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            Fingerprint([m, m ^ 0x5555_5555_5555_5555, m.rotate_left(17), m | 0x80])
        }
    }
}

/// Minimal in-memory store over sorted `i32` keys implementing the public
/// [`HashRangeQueryable`] trait, with a controllable per-element hash (see
/// [`elem_hash`]). The blanket impl gives us [`Diffable`] for free, so we can
/// drive it through the same [`diff`]/[`reconcile`] helpers as `HRTree`.
struct MockStore {
    keys: Vec<i32>,
}

impl MockStore {
    fn new(mut keys: Vec<i32>) -> Self {
        keys.sort_unstable();
        keys.dedup();
        MockStore { keys }
    }

    /// Collect the keys this store holds that fall inside any of the given ranges.
    fn keys_in_ranges(&self, ranges: &[DiffRange<i32>]) -> Vec<i32> {
        let mut out: Vec<i32> = self
            .keys
            .iter()
            .copied()
            .filter(|k| ranges.iter().any(|r| r.contains(k)))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

impl HashRangeQueryable for MockStore {
    type Key = i32;

    fn hash<R: RangeBounds<i32>>(&self, range: &R) -> Fingerprint {
        self.keys
            .iter()
            .filter(|k| range.contains(k))
            .fold(Fingerprint::ZERO, |acc, &k| acc + elem_hash(k))
    }

    fn insertion_position(&self, key: &i32) -> usize {
        self.keys.partition_point(|k| k < key)
    }

    fn key_at(&self, index: usize) -> &i32 {
        &self.keys[index]
    }

    fn len(&self) -> usize {
        self.keys.len()
    }
}

/// Regression test for issue #106 (headline counterexample).
///
/// A holds `{1, 2}`, whose range hash XORs to `0` (`elem_hash(1) ^ elem_hash(2)`),
/// while empty B also hashes to `0`. The buggy code short-circuited on the very
/// first `hash == local_hash` (`0 == 0`) check and concluded "in sync", silently
/// losing both elements. With the size-based decision, A must be found to owe
/// `{1, 2}` to B.
#[test]
fn nonempty_range_hashing_to_zero_vs_empty() {
    let a = MockStore::new(vec![1, 2]);
    let b = MockStore::new(vec![]);

    // Sanity: the non-empty range really does hash to the empty sentinel value.
    assert_eq!(a.hash(&..), Fingerprint::ZERO);
    assert_eq!(b.hash(&..), Fingerprint::ZERO);

    let (local_diff_ranges, remote_diff_ranges) = diff(&a, &b);

    // A owns {1, 2} that B is missing; B owns nothing.
    assert_eq!(a.keys_in_ranges(&local_diff_ranges), vec![1, 2]);
    assert_eq!(b.keys_in_ranges(&remote_diff_ranges), Vec::<i32>::new());
}

/// Regression test for issue #106 (branches B/C): a non-empty range whose hash is
/// `0` must not be mistaken for an empty range when the peer holds different,
/// non-empty content over the same range.
///
/// A holds `{1, 2}` (range hash `0`, size 2); B holds `{1, 2, 5}` (range hash
/// `elem_hash(5)`, size 3). The buggy code, seeing A's advertised `hash == 0`,
/// treated A as empty, pushed A's whole range as a difference, and never
/// discovered B's extra key `5`. The fix refines the range and isolates `5`.
#[test]
fn nonempty_collision_vs_different_content() {
    let a = MockStore::new(vec![1, 2]);
    let b = MockStore::new(vec![1, 2, 5]);

    assert_eq!(a.hash(&..), Fingerprint::ZERO);
    assert_ne!(b.hash(&..), Fingerprint::ZERO);

    let (local_diff_ranges, remote_diff_ranges) = diff(&a, &b);

    // A (local) owns nothing B is missing; B (remote) owns {5} that A is missing.
    assert_eq!(a.keys_in_ranges(&local_diff_ranges), Vec::<i32>::new());
    assert_eq!(b.keys_in_ranges(&remote_diff_ranges), vec![5]);
}
