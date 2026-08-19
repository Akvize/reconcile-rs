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

use std::cmp::Ordering;
use std::iter::FusedIterator;
use std::ops::{Bound, RangeBounds};

use arrayvec::ArrayVec;
use range_cmp::{RangeOrd, RangeOrdering};
use serde::Serialize;
use tracing::trace;

use crate::aggregate::Aggregate;
use crate::fingerprint::{lift, Fingerprint};

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

/// The route from the root to the node holding a key: the child index taken at each interior node,
/// then the key's own index inside the node where the key was found.
struct KeyPath {
    descent: Vec<usize>,
    key_index: usize,
}

/// Restores the aggregate invariants for one in-place value mutation, **on drop** — so a
/// panicking callback still leaves every cached [`Aggregate`] consistent with what is stored.
struct Relift<'a, K: Serialize, V: Serialize> {
    root: &'a mut Node<K, V>,
    path: KeyPath,
    key: &'a K,
}

impl<K: Serialize, V: Serialize> Relift<'_, K, V> {
    /// The value this guard will re-lift, reached by following the recorded route.
    fn value_mut(&mut self) -> &mut V {
        let mut node = &mut *self.root;
        for &index in &self.path.descent {
            node = node.children.as_mut().expect("interior node on the route")[index].as_mut();
        }
        &mut node.values[self.path.key_index]
    }
}

impl<K: Serialize, V: Serialize> Drop for Relift<'_, K, V> {
    fn drop(&mut self) {
        /// The signed fingerprint delta this subtree contributed, already applied to its cache.
        fn repair<K: Serialize, V: Serialize>(
            node: &mut Node<K, V>,
            descent: &[usize],
            key_index: usize,
            key: &K,
        ) -> Fingerprint {
            let delta = match descent.split_first() {
                None => {
                    let old_fp = node.fingerprints[key_index];
                    let new_fp = lift(key, &node.values[key_index]);
                    node.fingerprints[key_index] = new_fp;
                    new_fp - old_fp
                }
                Some((&index, rest)) => repair(
                    node.children.as_mut().expect("interior node on the route")[index].as_mut(),
                    rest,
                    key_index,
                    key,
                ),
            };
            node.subtree = Aggregate::new(node.subtree.size(), node.subtree.fingerprint() + delta);
            delta
        }
        repair(self.root, &self.path.descent, self.path.key_index, self.key);
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Node<K, V> {
    pub(crate) keys: ArrayVec<K, MAX_CAPACITY>,
    pub(crate) values: ArrayVec<V, MAX_CAPACITY>,
    fingerprints: ArrayVec<Fingerprint, MAX_CAPACITY>,
    pub(crate) children: Option<ArrayVec<Box<Node<K, V>>, { MAX_CAPACITY + 1 }>>,
    /// `A(S)` over this node's whole subtree: its own separators plus everything under
    /// `children`.
    subtree: Aggregate,
}

impl<K, V> Node<K, V> {
    fn new() -> Self {
        Node {
            keys: ArrayVec::new(),
            values: ArrayVec::new(),
            fingerprints: ArrayVec::new(),
            children: None,
            subtree: Aggregate::ZERO,
        }
    }

    /// `|S|` over this node's whole subtree — `O(1)`, cached in [`subtree`](Node::subtree).
    /// Crate-visible so the iterators in `fingerprint_tree_map_iter` can seed an exact
    /// `remaining` count without an unconstrained-generic `FingerprintTreeMap::len` call.
    pub(crate) fn subtree_size(&self) -> usize {
        self.subtree.size()
    }

    /// Recompute [`subtree`](Node::subtree) by composing own separators with each child's
    /// aggregate.
    fn refresh_aggregate(&mut self) {
        let mut aggregate = Aggregate::ZERO;
        for fingerprint in self.fingerprints.iter() {
            aggregate += element(*fingerprint);
        }
        if let Some(children) = self.children.as_ref() {
            for child in children {
                aggregate += child.subtree;
            }
        }
        self.subtree = aggregate;
    }

    fn insert(
        &mut self,
        index: usize,
        key: K,
        value: V,
        fingerprint: Fingerprint,
        right_child: Option<Box<Node<K, V>>>,
        diff_fp: Fingerprint,
    ) -> InsertionTuple<K, V> {
        assert_eq!(self.children.is_none(), right_child.is_none());
        if self.keys.is_full() {
            // Safe to split at any `self.keys.len() == MAX_CAPACITY` here: the `B.checked_sub(3)`
            // assertion above rules out the `B == 2` case that would leave an empty sibling node.
            let mid = self.keys.len() / 2;
            let mut right_sibling = Box::new(Node {
                keys: ArrayVec::from_iter(self.keys.drain(mid + 1..)),
                values: ArrayVec::from_iter(self.values.drain(mid + 1..)),
                fingerprints: ArrayVec::from_iter(self.fingerprints.drain(mid + 1..)),
                children: self
                    .children
                    .as_mut()
                    .map(|children| ArrayVec::from_iter(children.drain(mid + 1..))),
                subtree: Aggregate::ZERO,
            });
            let mid_key = self.keys.pop().unwrap();
            let mid_value = self.values.pop().unwrap();
            let mid_fp = self.fingerprints.pop().unwrap();
            let to_insert = if index <= mid {
                self.insert(index, key, value, fingerprint, right_child, diff_fp)
            } else {
                right_sibling.insert(
                    index - mid - 1,
                    key,
                    value,
                    fingerprint,
                    right_child,
                    diff_fp,
                )
            };
            assert!(to_insert.is_none());
            assert!(!self.keys.is_empty());
            assert!(!right_sibling.keys.is_empty());
            self.refresh_aggregate();
            right_sibling.refresh_aggregate();
            Some((mid_key, mid_value, mid_fp, right_sibling))
        } else {
            self.keys.insert(index, key);
            self.values.insert(index, value);
            self.fingerprints.insert(index, fingerprint);
            self.subtree += Aggregate::new(1, diff_fp);
            if let Some(right_child) = right_child {
                assert!(self.children.is_some());
                self.children
                    .as_mut()
                    .unwrap()
                    .insert(index + 1, right_child);
            }
            None
        }
    }

    /// Rotate one separator (and its adjacent child) from an over-full sibling into the
    /// underflowing child at `index`, restoring minimum occupancy. [`Side`] picks which sibling;
    /// the two cases are mirror images.
    fn steal(&mut self, index: usize, side: Side) {
        let from_left = side == Side::Left;
        let children = self.children.as_mut().unwrap();
        let (sibling_index, sep_index) = if from_left {
            (index - 1, index - 1)
        } else {
            (index + 1, index)
        };
        // take the boundary separator (k, v, h) from the sibling
        let sibling = children[sibling_index].as_mut();
        let (k, v, h) = if from_left {
            (
                sibling.keys.pop().unwrap(),
                sibling.values.pop().unwrap(),
                sibling.fingerprints.pop().unwrap(),
            )
        } else {
            (
                sibling.keys.remove(0),
                sibling.values.remove(0),
                sibling.fingerprints.remove(0),
            )
        };
        sibling.subtree = without(sibling.subtree, element(h));
        // take the boundary child from the sibling if any
        let c = sibling.children.as_mut().map(|children| {
            let c = if from_left {
                children.pop().unwrap()
            } else {
                children.remove(0)
            };
            sibling.subtree = without(sibling.subtree, c.subtree);
            c
        });
        // exchange the sibling's separator with the parent's separator
        let k = std::mem::replace(&mut self.keys[sep_index], k);
        let v = std::mem::replace(&mut self.values[sep_index], v);
        let h = std::mem::replace(&mut self.fingerprints[sep_index], h);
        // move the separator into the current (underflowing) node, at the end facing the sibling
        let current = self.children.as_mut().unwrap()[index].as_mut();
        if from_left {
            current.keys.insert(0, k);
            current.values.insert(0, v);
            current.fingerprints.insert(0, h);
        } else {
            current.keys.push(k);
            current.values.push(v);
            current.fingerprints.push(h);
        }
        current.subtree += element(h);
        // move the rotated child into the current node if any
        if let Some(c) = c {
            current.subtree += c.subtree;
            let current_children = current.children.as_mut().unwrap();
            if from_left {
                current_children.insert(0, c);
            } else {
                current_children.push(c);
            }
        }
    }

    fn rebalance_after_deletion(&mut self, index: usize) {
        let children = self.children.as_mut().unwrap();
        if children[index].keys.len() >= MIN_CAPACITY {
            return;
        }
        if index > 0 && children[index - 1].keys.len() > MIN_CAPACITY {
            self.steal(index, Side::Left);
        } else if index + 1 < children.len() && children[index + 1].keys.len() > MIN_CAPACITY {
            self.steal(index, Side::Right);
        } else {
            let merge_into = if index > 0 {
                index - 1
            } else if index + 1 < children.len() {
                index
            } else {
                // Root: no sibling to steal from or merge with.
                return;
            };

            let right_sibling = children.remove(merge_into + 1);
            let current = children[merge_into].as_mut();
            let k = self.keys.remove(merge_into);
            let v = self.values.remove(merge_into);
            let h = self.fingerprints.remove(merge_into);
            current.keys.push(k);
            current.values.push(v);
            current.fingerprints.push(h);
            current.subtree += element(h);
            for k in right_sibling.keys {
                current.keys.push(k);
            }
            for v in right_sibling.values {
                current.values.push(v);
            }
            for h in right_sibling.fingerprints {
                current.fingerprints.push(h);
            }
            if let Some(child_children) = current.children.as_mut() {
                for c in right_sibling.children.unwrap() {
                    child_children.push(c);
                }
            }
            current.subtree += right_sibling.subtree;
        }
    }
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

impl<K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// An empty tree. Equivalent to [`Default::default`].
    #[must_use]
    pub fn new() -> Self {
        Default::default()
    }

    /// Returns the value associated with `key`, if present.
    pub fn get<'a>(&'a self, key: &K) -> Option<&'a V> {
        fn aux<'a, K: Ord, V>(node: &'a Node<K, V>, key: &K) -> Option<&'a V> {
            match node.keys.binary_search(key) {
                Ok(index) => Some(&node.values[index]),
                Err(index) => {
                    if let Some(children) = node.children.as_ref() {
                        aux(children[index].as_ref(), key)
                    } else {
                        None
                    }
                }
            }
        }
        aux(self.root.as_ref(), key)
    }

    /// Returns whether `key` is present.
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Calls `callback` with a mutable reference to the value at `key` (`None` if absent), then
    /// re-lifts the element and propagates the fingerprint delta to the root, returning the
    /// callback's own return value.
    ///
    /// The only summary-safe in-place mutation path — no `entry()` handing out a bare `&mut V`
    /// is offered, since it could skip the re-lift.
    ///
    /// # Panic safety
    ///
    /// The repair runs from a [`Drop`] guard, so a panicking `callback` still leaves the cached
    /// aggregates consistent with the stored value; the panic propagates unchanged.
    pub fn with_mut<R, F: FnOnce(Option<&mut V>) -> R>(&mut self, key: &K, callback: F) -> R {
        let mut descent = Vec::new();
        let mut node = self.root.as_ref();
        let key_index = loop {
            match node.keys.binary_search(key) {
                Ok(index) => break index,
                Err(index) => match node.children.as_ref() {
                    Some(children) => {
                        descent.push(index);
                        node = &children[index];
                    }
                    None => return callback(None),
                },
            }
        };

        let mut guard = Relift {
            root: self.root.as_mut(),
            path: KeyPath { descent, key_index },
            key,
        };
        let value = guard.value_mut();
        callback(Some(value))
    }

    /// Position of `key` in the in-order sequence, or `None` if absent — unlike
    /// [`rank`](FingerprintTreeMap::rank), which returns the insertion point for an absent key.
    pub fn position(&self, key: &K) -> Option<usize> {
        fn aux<K: Ord, V>(node: &Node<K, V>, key: &K) -> Option<usize> {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = key.cmp(&node.keys[i]);
                    if cmp == Ordering::Less {
                        return aux(&children[i], key).map(|offset| index + offset);
                    }
                    index += children[i].subtree.size();
                    if cmp == Ordering::Equal {
                        return Some(index);
                    }
                    index += 1;
                }
                aux(children.last().unwrap().as_ref(), key).map(|offset| index + offset)
            } else {
                node.keys.binary_search(key).ok()
            }
        }
        aux(self.root.as_ref(), key)
    }

    /// Inserts `key`/`value`, returning the previous value if `key` was already present.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        /// The right sibling to insert if this node split, the fingerprint delta, and the
        /// previous value at `key`.
        fn aux<K: Serialize + Ord, V: Serialize>(
            node: &mut Node<K, V>,
            key: K,
            value: V,
        ) -> (InsertionTuple<K, V>, Fingerprint, Option<V>) {
            match node.keys.binary_search(&key) {
                Ok(index) => {
                    let old_fp = node.fingerprints[index];
                    let new_fp = lift(&key, &value);
                    let diff_fp = new_fp - old_fp;
                    node.fingerprints[index] = new_fp;
                    node.subtree =
                        Aggregate::new(node.subtree.size(), node.subtree.fingerprint() + diff_fp);
                    let ret = std::mem::replace(&mut node.values[index], value);
                    (None, diff_fp, Some(ret))
                }
                Err(index) => {
                    if let Some(children) = node.children.as_mut() {
                        let (mut to_insert, diff_fp, ret) = aux(&mut children[index], key, value);
                        if let Some((key, value, fingerprint, right_child)) = to_insert {
                            to_insert = node.insert(
                                index,
                                key,
                                value,
                                fingerprint,
                                Some(right_child),
                                diff_fp,
                            )
                        } else {
                            let added = usize::from(ret.is_none());
                            node.subtree += Aggregate::new(added, diff_fp);
                        }
                        (to_insert, diff_fp, ret)
                    } else {
                        let fingerprint = lift(&key, &value);
                        let to_insert =
                            node.insert(index, key, value, fingerprint, None, fingerprint);
                        (to_insert, fingerprint, None)
                    }
                }
            }
        }
        let (to_insert, _, ret) = aux(&mut self.root, key, value);
        // if we still have things to insert at the root, we need to create a new root
        if let Some((key, value, fingerprint, right_child)) = to_insert {
            let new_root = Box::new(Node::new());
            let old_root = std::mem::replace(&mut self.root, new_root);
            let mut children = ArrayVec::new();
            children.push(old_root);
            children.push(right_child);
            self.root.keys.push(key);
            self.root.values.push(value);
            self.root.fingerprints.push(fingerprint);
            self.root.children = Some(children);
            self.root.refresh_aggregate();
        }
        trace!(
            "Updated state after insertion; global fingerprint is now {}",
            self.root.subtree.fingerprint()
        );
        ret
    }

    /// Removes `key`, returning its value if it was present.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        fn rightmost_child<K, V>(node: &mut Node<K, V>) -> (K, V, Fingerprint) {
            if let Some(children) = node.children.as_mut() {
                let (k, v, fp) = rightmost_child(children.last_mut().unwrap());
                node.subtree = without(node.subtree, element(fp));
                node.rebalance_after_deletion(node.keys.len());
                (k, v, fp)
            } else {
                let k = node.keys.pop().unwrap();
                let v = node.values.pop().unwrap();
                let fp = node.fingerprints.pop().unwrap();
                node.subtree = without(node.subtree, element(fp));
                (k, v, fp)
            }
        }
        /// The fingerprint delta and the value removed at `key`, if present.
        fn aux<K: Ord, V>(node: &mut Node<K, V>, key: &K) -> (Fingerprint, Option<V>) {
            match node.keys.binary_search(key) {
                Ok(index) => {
                    if let Some(children) = node.children.as_mut() {
                        let (prev_k, prev_v, prev_fp) = rightmost_child(&mut children[index]);
                        node.keys[index] = prev_k;
                        let v = std::mem::replace(&mut node.values[index], prev_v);
                        let fp = std::mem::replace(&mut node.fingerprints[index], prev_fp);
                        node.subtree = without(node.subtree, element(fp));
                        node.rebalance_after_deletion(index);
                        (fp, Some(v))
                    } else {
                        node.keys.remove(index);
                        let v = node.values.remove(index);
                        let fp = node.fingerprints.remove(index);
                        node.subtree = without(node.subtree, element(fp));
                        (fp, Some(v))
                    }
                }
                Err(index) => {
                    if let Some(children) = node.children.as_mut() {
                        let (diff_fp, ret) = aux(&mut children[index], key);
                        let removed = Aggregate::new(usize::from(ret.is_some()), diff_fp);
                        node.subtree = without(node.subtree, removed);
                        node.rebalance_after_deletion(index);
                        (diff_fp, ret)
                    } else {
                        (Fingerprint::ZERO, None)
                    }
                }
            }
        }
        let ret = aux(&mut self.root, key).1;
        trace!(
            "Updated state after removal; global fingerprint is now {}",
            self.root.subtree.fingerprint()
        );
        ret
    }

    /// Removes every entry, resetting the tree to the same empty state [`new`](Self::new) produces.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Removes every entry for which `keep` returns `false`. `O(n log n)`: collect, then remove.
    pub fn retain<F: FnMut(&K, &V) -> bool>(&mut self, mut keep: F)
    where
        K: Clone,
    {
        let to_remove: Vec<K> = self
            .iter()
            .filter(|(k, v)| !keep(k, v))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &to_remove {
            self.remove(key);
        }
    }

    /// Recomputes every cached [`Aggregate`] and checks the ordering, occupancy and height
    /// invariants. `O(n)` — for tests.
    ///
    /// # Panics
    ///
    /// If any invariant is violated.
    pub fn check_invariants(&self) {
        /// The independently recomputed aggregate and height of this subtree.
        fn aux<'a, K: Serialize + Ord, V: Serialize>(
            node: &'a Node<K, V>,
            mut min: Option<&'a K>,
            max: Option<&K>,
        ) -> (Aggregate, usize) {
            let mut cum = Aggregate::ZERO;
            let mut max_height = 1;
            if min.is_some() || max.is_some() {
                assert!(
                    node.keys.len() >= MIN_CAPACITY,
                    "minimum node size invariant violated"
                );
            }
            if let Some(min) = min {
                assert!(min <= &node.keys[0], "order invariant violated");
            }
            for i in 1..node.keys.len() {
                assert!(node.keys[i - 1] <= node.keys[i], "order invariant violated");
            }
            if let Some(max) = max {
                assert!(node.keys.last().unwrap() <= max, "order invariant violated");
            }
            for i in 0..node.keys.len() {
                // child before key
                if let Some(children) = node.children.as_ref() {
                    let next_max = Some(&node.keys[i]);
                    let (child, child_height) = aux(&children[i], min, next_max);
                    cum += child;
                    if max_height != 1 {
                        assert_eq!(child_height, max_height, "height invariant violated");
                    }
                    max_height = child_height;
                    min = next_max;
                }
                // key
                let fingerprint = lift(&node.keys[i], &node.values[i]);
                assert_eq!(
                    fingerprint, node.fingerprints[i],
                    "per-element fingerprint cache invalid"
                );
                cum += element(fingerprint);
            }
            // child after last key
            if let Some(children) = node.children.as_ref() {
                let (child, child_height) = aux(children.last().unwrap(), min, max);
                cum += child;
                if max_height != 1 {
                    assert_eq!(child_height, max_height, "height invariant violated");
                }
            }
            assert_eq!(cum, node.subtree, "subtree aggregate invariant violated");
            (cum, max_height + 1)
        }
        aux(&self.root, None, None);
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

impl<K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Bundled [`Aggregate`] over a range of keys in one `O(log n)` tree walk;
    /// [`Rsos::aggregate`](crate::Rsos::aggregate)'s realization.
    ///
    /// Takes the range by value, as [`range`](Self::range) does.
    pub fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate {
        fn aux<'a, K: Ord, V, R: RangeBounds<K>>(
            node: &'a Node<K, V>,
            range: &R,
            mut lower_bound: Option<&'a K>,
            upper_bound: Option<&K>,
        ) -> Aggregate {
            let lower_bound_included = match range.start_bound() {
                Bound::Unbounded => true,
                Bound::Included(key) | Bound::Excluded(key) => {
                    if let Some(lower_bound) = lower_bound {
                        key < lower_bound
                    } else {
                        false
                    }
                }
            };
            let upper_bound_included = match range.end_bound() {
                Bound::Unbounded => true,
                Bound::Included(key) | Bound::Excluded(key) => {
                    if let Some(upper_bound) = upper_bound {
                        key > upper_bound
                    } else {
                        false
                    }
                }
            };
            // Both bounds inside the range: the cached subtree aggregate is the answer.
            if lower_bound_included && upper_bound_included {
                return node.subtree;
            }
            let mut cum = Aggregate::ZERO;
            let mut i = 0;
            while i < node.keys.len() && node.keys[i].rcmp(range) == RangeOrdering::Below {
                i += 1;
            }
            while i < node.keys.len() && node.keys[i].rcmp(range) == RangeOrdering::Inside {
                let cur_bound = Some(&node.keys[i]);
                if let Some(children) = node.children.as_ref() {
                    cum += aux(&children[i], range, lower_bound, cur_bound);
                }
                cum += element(node.fingerprints[i]);
                lower_bound = cur_bound;
                i += 1;
            }
            if let Some(children) = node.children.as_ref() {
                cum += aux(&children[i], range, lower_bound, upper_bound);
            }
            cum
        }
        aux(&self.root, &range, None, None)
    }

    /// Position of `key` in the in-order sequence, or the position it would occupy after
    /// insertion; [`Rsos::rank`](crate::Rsos::rank)'s realization.
    pub fn rank(&self, key: &K) -> usize {
        fn aux<K: Ord, V>(node: &Node<K, V>, key: &K) -> usize {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = key.cmp(&node.keys[i]);
                    if cmp == Ordering::Less {
                        return index + aux(&children[i], key);
                    }
                    index += children[i].subtree.size();
                    if cmp == Ordering::Equal {
                        return index;
                    }
                    index += 1;
                }
                index + aux(children.last().unwrap(), key)
            } else {
                match node.keys.binary_search(key) {
                    Ok(index) => index,
                    Err(index) => index,
                }
            }
        }
        aux(&self.root, key)
    }

    /// Reference to the key at the given in-order position; [`Rsos::select`](crate::Rsos::select)'s
    /// realization.
    ///
    /// # Panics
    ///
    /// If the position is out of bounds.
    #[must_use]
    pub fn select(&self, index: usize) -> &K {
        fn aux<K: Ord, V>(node: &Node<K, V>, mut index: usize) -> &K {
            if let Some(children) = node.children.as_ref() {
                for i in 0..node.keys.len() {
                    if index < children[i].subtree.size() {
                        return aux(&children[i], index);
                    }
                    index -= children[i].subtree.size();
                    if index == 0 {
                        return &node.keys[i];
                    }
                    index -= 1;
                }
                aux(children.last().unwrap(), index)
            } else {
                &node.keys[index]
            }
        }
        aux(&self.root, index)
    }

    /// Number of elements in the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.root.subtree.size()
    }

    /// Whether the tree holds no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Iterator over the key-value pairs of a [`FingerprintTreeMap`] whose key falls within a range, in
/// key order. Returned by [`FingerprintTreeMap::range`].
///
/// Named `ItemRange`, not `Range`: the latter would collide with [`std::ops::Range`], which this
/// type's own generic parameter `R` is frequently instantiated with. Frozen (#291).
pub struct ItemRange<'a, K, V, R: RangeBounds<K>> {
    /// Owned, not borrowed: a borrowed range makes `map.range(lo..hi)` on runtime bounds `E0716`.
    range: R,
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K: Ord, V, R: RangeBounds<K>> Iterator for ItemRange<'a, K, V, R> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            #[allow(clippy::collapsible_if)]
            if 0 < children_passed && children_passed <= node.keys.len() {
                if !self.range.contains(&node.keys[children_passed - 1]) {
                    self.stack.clear();
                    return None;
                }
            }
            if children_passed <= node.keys.len() {
                self.stack.push((node, children_passed + 1));
                if let Some(children) = node.children.as_ref() {
                    self.stack.push((&children[children_passed], 0));
                }
            }
            if 0 < children_passed && children_passed <= node.keys.len() {
                Some((
                    &node.keys[children_passed - 1],
                    &node.values[children_passed - 1],
                ))
            } else {
                self.next()
            }
        } else {
            None
        }
    }

    /// Exact: mirrors [`next`](Self::next)'s traversal on a cloned stack (references are `Copy`,
    /// so this borrows rather than mutates `self`) to count the remaining matches without
    /// requiring `R: Clone`.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let mut stack = self.stack.clone();
        let mut count = 0usize;
        while let Some((node, children_passed)) = stack.pop() {
            #[allow(clippy::collapsible_if)]
            if 0 < children_passed && children_passed <= node.keys.len() {
                if !self.range.contains(&node.keys[children_passed - 1]) {
                    break;
                }
            }
            if children_passed <= node.keys.len() {
                stack.push((node, children_passed + 1));
                if let Some(children) = node.children.as_ref() {
                    stack.push((&children[children_passed], 0));
                }
            }
            if 0 < children_passed && children_passed <= node.keys.len() {
                count += 1;
            }
        }
        (count, Some(count))
    }
}

/// Exact per [`size_hint`](Iterator::size_hint) above.
impl<'a, K: Ord, V, R: RangeBounds<K>> ExactSizeIterator for ItemRange<'a, K, V, R> {}

/// Range exhaustion clears the stack (see [`next`](Iterator::next)) rather than leaving it
/// mid-traversal, so a post-`None` call is always `None` again.
impl<'a, K: Ord, V, R: RangeBounds<K>> FusedIterator for ItemRange<'a, K, V, R> {}

impl<'a, K, V, R: RangeBounds<K> + Clone> Clone for ItemRange<'a, K, V, R> {
    fn clone(&self) -> Self {
        ItemRange {
            range: self.range.clone(),
            stack: self.stack.clone(),
        }
    }
}

impl<'a, K: std::fmt::Debug + Ord, V: std::fmt::Debug, R: RangeBounds<K> + Clone> std::fmt::Debug
    for ItemRange<'a, K, V, R>
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_map().entries(self.clone()).finish()
    }
}

impl<K: Ord, V> FingerprintTreeMap<K, V> {
    /// Iterator over the key-value pairs whose key falls in `range`, in key order;
    /// [`Rsos::enumerate`](crate::Rsos::enumerate)'s realization.
    ///
    /// Takes the range **by value**, as [`BTreeMap::range`](std::collections::BTreeMap::range)
    /// does: a borrowed range must outlive the iterator, which makes runtime bounds `E0716`.
    pub fn range<R: RangeBounds<K>>(&self, range: R) -> ItemRange<'_, K, V, R> {
        let mut stack = Vec::new();
        let mut node = self.root.as_ref();
        // traverse interior nodes
        'main_loop: while let Some(children) = node.children.as_ref() {
            for i in 0..node.keys.len() {
                match node.keys[i].rcmp(&range) {
                    RangeOrdering::Below => (),
                    RangeOrdering::Above => {
                        node = &children[i];
                        continue 'main_loop;
                    }
                    RangeOrdering::Inside => {
                        stack.push((node, i + 1));
                        node = &children[i];
                        continue 'main_loop;
                    }
                    RangeOrdering::Empty => break,
                }
            }
            node = children.last().as_ref().unwrap();
        }
        // traverse leaf node
        for i in 0..node.keys.len() {
            match node.keys[i].rcmp(&range) {
                RangeOrdering::Below => (),
                RangeOrdering::Above | RangeOrdering::Empty => {
                    break;
                }
                RangeOrdering::Inside => {
                    stack.push((node, i + 1));
                    break;
                }
            }
        }
        ItemRange { range, stack }
    }

    /// The smallest key and its value, or `None` if the tree is empty. `O(log n)`: descends the
    /// leftmost path.
    #[must_use]
    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        let mut node = self.root.as_ref();
        while let Some(children) = node.children.as_ref() {
            node = &children[0];
        }
        node.keys.first().zip(node.values.first())
    }

    /// The largest key and its value, or `None` if the tree is empty. `O(log n)`: descends the
    /// rightmost path.
    #[must_use]
    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        let mut node = self.root.as_ref();
        while let Some(children) = node.children.as_ref() {
            node = children.last().unwrap();
        }
        node.keys.last().zip(node.values.last())
    }
}

#[cfg(test)]
mod tests {
    use std::ops::RangeBounds;

    use rand::{seq::SliceRandom, Rng, SeedableRng};

    use crate::aggregate::Aggregate;
    use crate::fingerprint::Fingerprint;

    use super::FingerprintTreeMap;

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
    fn test_aggregate() {
        // empty
        let mut tree = FingerprintTreeMap::new();
        assert_eq!(tree.aggregate(..), Aggregate::ZERO);
        tree.check_invariants();

        // 1 value
        tree.insert(50, "Hello");
        tree.check_invariants();
        let agg1 = tree.aggregate(..);
        assert_eq!(agg1.size(), 1);
        // Fingerprints, not aggregates: differing sizes would make `!=` trivial.
        assert_ne!(agg1.fingerprint(), Fingerprint::ZERO);

        // 2 values
        tree.insert(25, "World!");
        tree.check_invariants();
        let agg2 = tree.aggregate(..);
        assert_eq!(agg2.size(), 2);
        assert_ne!(agg2.fingerprint(), Fingerprint::ZERO);
        assert_ne!(agg2.fingerprint(), agg1.fingerprint());

        // 3 values
        tree.insert(75, "Everyone!");
        tree.check_invariants();
        let agg3 = tree.aggregate(..);
        assert_eq!(agg3.size(), 3);
        assert_ne!(agg3.fingerprint(), Fingerprint::ZERO);
        assert_ne!(agg3.fingerprint(), agg1.fingerprint());
        assert_ne!(agg3.fingerprint(), agg2.fingerprint());

        tree.remove(&75);
        tree.check_invariants();
        assert_eq!(tree.aggregate(..), agg2);
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
    fn retain_drops_non_matching_and_preserves_invariants() {
        let mut tree: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
        for k in 0..30 {
            tree.insert(k, k * 10);
        }
        tree.check_invariants();

        tree.retain(|k, _| k % 2 == 0);
        tree.check_invariants();

        assert_eq!(tree.len(), 15);
        for k in 0..30 {
            if k % 2 == 0 {
                assert_eq!(tree.get(&k), Some(&(k * 10)));
            } else {
                assert_eq!(tree.get(&k), None);
            }
        }

        tree.retain(|_, v| *v < 100);
        tree.check_invariants();
        assert!(tree.iter().all(|(_, v)| *v < 100));
    }

    #[test]
    fn with_mut_keeps_aggregates_consistent_when_the_callback_panics() {
        let mut tree: FingerprintTreeMap<u64, u64> = (0..50).map(|k| (k, k)).collect();
        let before = tree.aggregate(..);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tree.with_mut(&17, |v| {
                *v.expect("17 is present") = 999;
                panic!("callback blew up mid-mutation");
            })
        }));
        assert!(outcome.is_err(), "the panic must still propagate");

        assert_eq!(tree.get(&17), Some(&999));

        // The caches must describe what is actually stored.
        tree.check_invariants();
        let after = tree.aggregate(..);
        assert_ne!(after, before, "the summary must have moved with the value");
        assert_eq!(after.size(), 50, "no element was added or removed");

        tree.insert(200, 200);
        tree.check_invariants();
        assert_eq!(tree.len(), 51);
    }

    #[test]
    fn with_mut_passes_the_callback_result_back() {
        let mut tree: FingerprintTreeMap<u64, u64> = (0..10).map(|k| (k, k)).collect();

        let doubled = tree.with_mut(&3, |v| {
            let v = v.expect("3 is present");
            *v *= 2;
            *v
        });
        assert_eq!(doubled, 6);
        assert_eq!(tree.get(&3), Some(&6));
        tree.check_invariants();

        assert!(tree.with_mut(&999, |v| v.is_none()));
        tree.check_invariants();
    }

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

    #[test]
    fn equality_is_content_not_shape() {
        let ascending: FingerprintTreeMap<u64, u64> = (0..200).map(|k| (k, k * 7)).collect();
        let mut descending = FingerprintTreeMap::new();
        for k in (0..200).rev() {
            descending.insert(k, k * 7);
        }
        assert_eq!(ascending, descending);
        assert_eq!(ascending.aggregate(..), descending.aggregate(..));
    }

    #[test]
    fn equality_sees_values_and_cardinality() {
        let base: FingerprintTreeMap<u64, u64> = (0..50).map(|k| (k, k)).collect();

        let mut different_value = base.clone();
        different_value.insert(17, 999);
        assert_ne!(base, different_value);

        let mut fewer = base.clone();
        fewer.remove(&17);
        assert_ne!(base, fewer);

        fewer.insert(17, 17);
        assert_eq!(base, fewer);

        let empty_a: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
        let empty_b: FingerprintTreeMap<u64, u64> = FingerprintTreeMap::new();
        assert_eq!(empty_a, empty_b);
        assert_ne!(base, empty_a);
    }

    /// `==` must read the whole [`Aggregate`], not the fingerprint alone.
    #[test]
    fn equality_reads_the_whole_aggregate_not_just_the_fingerprint() {
        let a: FingerprintTreeMap<u64, u64> = (0..10).map(|k| (k, k)).collect();
        let b: FingerprintTreeMap<u64, u64> = (0..10).map(|k| (k, k)).collect();
        assert_eq!(a, b);
        assert_eq!(a.aggregate(..), b.aggregate(..));

        let fp = a.aggregate(..).fingerprint();
        assert_ne!(Aggregate::new(10, fp), Aggregate::new(9, fp));
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
            expected += Aggregate::new(1, super::lift(&key, &value));
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
        expected += Aggregate::new(1, super::lift(&key, &value2));
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
                expected.fingerprint() - super::lift(&key, &value),
            );
            assert_eq!(tree1.aggregate(..), expected);
        }
    }

    /// Both halves of the aggregate must agree with an independent walk of `range`.
    #[test]
    fn aggregate_count_matches_range_count() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut tree: FingerprintTreeMap<u32, u32> = FingerprintTreeMap::new();
        let mut keys = Vec::new();
        for _ in 0..75 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            tree.insert(key, value);
            keys.push(key);
        }
        keys.sort_unstable();
        tree.check_invariants();

        let check = |range: &dyn Fn() -> (std::ops::Bound<u32>, std::ops::Bound<u32>)| {
            let range = range();
            let aggregate = tree.aggregate(range);
            assert_eq!(
                aggregate.size(),
                tree.range(range).count(),
                "aggregate size disagrees with range().count() for {range:?}"
            );
            assert_eq!(
                aggregate.is_empty(),
                tree.range(range).next().is_none(),
                "aggregate is_empty disagrees with range() for {range:?}"
            );
            let expected_fingerprint = tree
                .range(range)
                .fold(Fingerprint::ZERO, |acc, (k, v)| acc + super::lift(k, v));
            assert_eq!(
                aggregate.fingerprint(),
                expected_fingerprint,
                "aggregate fingerprint disagrees with the summed element lifts for {range:?}"
            );
        };

        // empty range: nothing between a key and itself, excluded
        let mid = keys[keys.len() / 2];
        check(&|| {
            (
                std::ops::Bound::Included(mid),
                std::ops::Bound::Excluded(mid),
            )
        });
        // full range
        check(&|| (std::ops::Bound::Unbounded, std::ops::Bound::Unbounded));
        // partial ranges
        let lo = keys[keys.len() / 4];
        let hi = keys[3 * keys.len() / 4];
        check(&|| (std::ops::Bound::Included(lo), std::ops::Bound::Excluded(hi)));
        check(&|| (std::ops::Bound::Excluded(lo), std::ops::Bound::Included(hi)));
        check(&|| (std::ops::Bound::Unbounded, std::ops::Bound::Excluded(hi)));
        check(&|| (std::ops::Bound::Included(lo), std::ops::Bound::Unbounded));
        // an empty tree
        let empty: FingerprintTreeMap<u32, u32> = FingerprintTreeMap::new();
        assert_eq!(empty.aggregate(..), Aggregate::ZERO);
    }
}
