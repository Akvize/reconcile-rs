// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`FingerprintTreeMap`]: a from-scratch `ArrayVec`-node B-tree (order 6) caching a per-subtree
//! [`Aggregate`] at every node — `O(log n)` access, insertion, removal and range aggregate.
//!
//! The [`Rsos`](crate::Rsos) realization this crate ships (Meyer, arXiv:2212.13567;
//! arXiv:2603.19820). Lift and combiner: [`crate::fingerprint`]. Trait-to-inherent name mapping:
//! crate root docs.
//!
//! Split across siblings by concern: `node` owns the `Node<K, V>` storage and rebalancing;
//! `access`/`mutate`/`query`/`range` each own one `impl FingerprintTreeMap` group (point access,
//! insert/remove, order-statistics, range iteration); this file keeps the public type definitions
//! (their module location is their `cargo public-api`-visible path — see AGENTS.md §11) plus the
//! shared support (`Side`/`without`/`element`) every sibling draws on.

use std::ops::RangeBounds;

use crate::aggregate::Aggregate;
use crate::fingerprint::Fingerprint;

mod access;
mod mutate;
mod node;
mod query;
mod range;

pub use mutate::Entry;
pub(crate) use node::Node;

const B: usize = 6;
const MIN_CAPACITY: usize = B - 1;
const MAX_CAPACITY: usize = 2 * B - 1;

const _: usize = B.checked_sub(3).expect(
    "B must be >= 3: Node::insert's split only fires at MAX_CAPACITY, and at B == 2 \
     (MAX_CAPACITY == 3) a split at mid = 1 leaves a sibling with zero keys",
);

type InsertionTuple<K, V> = Option<(K, V, Fingerprint, Box<Node<K, V>>)>;

/// Which sibling a [`Node::steal`] rotates a separator from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// Remove `part` from `whole`.
///
/// Precondition: `part` was previously composed into `whole` — which is why [`Aggregate`], a
/// monoid, publishes no `Sub`.
fn without(whole: Aggregate, part: Aggregate) -> Aggregate {
    debug_assert!(
        whole.size() >= part.size(),
        "removed more than was composed"
    );
    Aggregate::new(
        whole.size() - part.size(),
        whole.fingerprint() - part.fingerprint(),
    )
}

/// The aggregate of a single element: `(1, lift(k, v))`.
fn element(fingerprint: Fingerprint) -> Aggregate {
    Aggregate::new(1, fingerprint)
}

/// This crate's [`Rsos`](crate::Rsos) realization: an in-memory, `ArrayVec`-node B-tree (order 6)
/// caching a per-subtree [`Aggregate`] at every node.
///
/// ```
/// use rsos::FingerprintTreeMap;
///
/// let mut map = FingerprintTreeMap::new();
/// map.insert(1, "one");
/// map.insert(2, "two");
/// map.insert(3, "three");
///
/// assert_eq!(map.get(&2), Some(&"two"));
/// assert_eq!(map.len(), 3);
///
/// // The whole-tree aggregate is what a peer compares over the wire to detect divergence --
/// // two trees with the same aggregate over the same range are assumed to hold the same data.
/// let whole = map.aggregate(..);
/// assert_eq!(whole.size(), 3);
/// ```
#[derive(Clone)]
pub struct FingerprintTreeMap<K, V> {
    pub(crate) root: Box<Node<K, V>>,
}

impl<K, V> Default for FingerprintTreeMap<K, V> {
    fn default() -> Self {
        FingerprintTreeMap {
            root: Box::new(Node::new()),
        }
    }
}

/// `O(1)` content comparison on the whole root [`Aggregate`] — size **and** fingerprint, never the
/// fingerprint alone (`ARCHITECTURE.md` §5 invariant 3), so tree shape does not matter and no
/// bound on `K`/`V` is needed. Collision-resistant, not information-theoretically exact.
impl<K, V> PartialEq for FingerprintTreeMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.root.subtree == other.root.subtree
    }
}

/// Bounded on `K: Eq, V: Eq`, unlike [`PartialEq`] above: the bound is not needed to compute `==`,
/// but `FingerprintTreeMap<u64, f64>` must not claim [`Eq`] when `f64` does not.
impl<K: Eq, V: Eq> Eq for FingerprintTreeMap<K, V> {}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for FingerprintTreeMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Iterator over the key-value pairs of a [`FingerprintTreeMap`] whose key falls within a range, in
/// key order. Returned by [`FingerprintTreeMap::range`].
///
/// Named `ItemRange`, not `Range`: the latter would collide with [`std::ops::Range`], which this
/// type's own generic parameter `R` is frequently instantiated with. Frozen (#291).
///
/// ```
/// use rsos::FingerprintTreeMap;
///
/// let map: FingerprintTreeMap<i32, &str> =
///     [(10, "a"), (20, "b"), (30, "c"), (40, "d")].into_iter().collect();
///
/// // Only the keys inside the bound are yielded, in key order -- not the whole map.
/// let pairs: Vec<_> = map.range(20..40).collect();
/// assert_eq!(pairs, vec![(&20, &"b"), (&30, &"c")]);
///
/// // Its count agrees with the aggregate computed over the same range: both walk the same subtree.
/// assert_eq!(map.range(20..40).count(), map.aggregate(20..40).size());
/// ```
pub struct ItemRange<'a, K, V, R: RangeBounds<K>> {
    /// Owned, not borrowed: a borrowed range makes `map.range(lo..hi)` on runtime bounds `E0716`.
    range: R,
    stack: Vec<(&'a Node<K, V>, usize)>,
}

#[cfg(test)]
mod tests;
