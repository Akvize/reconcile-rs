// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides [`FingerprintTreeMap`], a from-scratch `ArrayVec`-node B-tree (order 6) that caches a
//! per-subtree [`Fingerprint`] (and element count) at every node.
//!
//! It allows `O(log(n))` access, insertion and removal, as well as `O(log(n))` cumulated
//! range-aggregate queries. The latter property enables querying the cumulated [`Fingerprint`] of all
//! key-value pairs between two keys.
//!
//! The per-element lift and the way fingerprints are combined (256-bit addition,
//! *not* XOR) live in [`crate::fingerprint`]; see that module for
//! why the combiner and the underlying hash function are chosen the way they are.
//!
//! This per-node-cached-subtree-fingerprint structure was arrived at independently, but it matches
//! a structure described in A. Meyer, *Range-Based Set Reconciliation* (arXiv:2212.13567) — see the
//! crate root docs for the full citation, including the later RSOS paper this crate also implements.
//!
//! [`FingerprintTreeMap`] exposes the range-aggregate queries (`aggregate`,
//! `rank`, `select`, `len`) that a range-based-set-reconciliation anti-entropy
//! protocol (such as this workspace's `rbsr`) needs to drive range reconciliation. It also
//! implements the [`Rsos`](crate::Rsos) trait, whose seven methods are the paper's own Def. 3.9
//! terms; four of them share a name with the inherent method they delegate to and three
//! (`size`/`enumerate`/`delete` vs. `len`/`range`/`remove`) do not, because the inherent API keeps
//! Rust's container spellings. The crate root docs carry the full operation-by-operation mapping
//! table and the reasoning.

use std::cmp::Ordering;
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

type InsertionTuple<K, V> = Option<(K, V, Fingerprint, Box<Node<K, V>>)>;

/// Which sibling a [`Node::steal`] rotates a separator from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// Remove `part` from `whole`, where `part` is known to be one of the aggregates previously
/// composed into `whole`.
///
/// [`Aggregate`] deliberately exposes no `Sub`: Def. 3.5 is a *monoid*, and `ℕ` under addition has
/// no inverse, so a general `whole - part` could underflow. Inside the tree the precondition
/// always holds — every call below un-does a composition this same code made when the element or
/// subtree was inserted — so the count subtraction is sound here and nowhere else. Keeping it as
/// one private helper is what confines that precondition to this file instead of publishing a
/// `Sub` impl the monoid does not have.
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

#[derive(Clone, Debug, Default)]
pub(crate) struct Node<K, V> {
    pub(crate) keys: ArrayVec<K, MAX_CAPACITY>,
    pub(crate) values: ArrayVec<V, MAX_CAPACITY>,
    fingerprints: ArrayVec<Fingerprint, MAX_CAPACITY>,
    pub(crate) children: Option<ArrayVec<Box<Node<K, V>>, { MAX_CAPACITY + 1 }>>,
    /// Def. 3.5's bundled aggregate `A(S) = (|S|, Σ(S))` over this node's whole subtree — the
    /// separators held here plus everything under `children`.
    ///
    /// One field, not the former `tree_hash: Fingerprint` + `tree_size: usize` pair. Those two
    /// always described the same set and had to be updated in lockstep by every mutation below;
    /// that was an invariant kept by discipline, and a mutation touching one and forgetting the
    /// other still compiled. Bundling them into the [`Aggregate`] monoid makes it structural: the
    /// halves compose together through `⊗` ([`Add`](std::ops::Add)) or not at all, so they can no
    /// longer drift apart.
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

    /// Recompute [`subtree`](Node::subtree) from scratch by composing this node's own separators
    /// with each child's already-correct aggregate — `⊗` over a partition of the subtree, exactly
    /// Def. 3.5's homomorphism.
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
            // TODO: handle case where self.keys.len() == 2 without leaving empty node
            let mid = self.keys.len() / 2;
            let mut right_sibling = Box::new(Node {
                keys: ArrayVec::from_iter(self.keys.drain(mid + 1..)),
                values: ArrayVec::from_iter(self.values.drain(mid + 1..)),
                fingerprints: ArrayVec::from_iter(self.fingerprints.drain(mid + 1..)),
                children: self
                    .children
                    .as_mut()
                    .map(|children| ArrayVec::from_iter(children.drain(mid + 1..))),
                // Both halves start empty together and are recomputed below by
                // `refresh_aggregate`, once the split has settled.
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
            // One element more, and `diff_fp` folded into Σ(S) — one `⊗`, not two updates.
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
    /// underflowing child at `index`, restoring the minimum-occupancy invariant.
    ///
    /// [`Side::Left`] steals the *last* separator of the left sibling (`index - 1`) and
    /// rotates right; [`Side::Right`] steals the *first* separator of the right sibling
    /// (`index + 1`) and rotates left. The two cases are exact mirror images, so they share this
    /// body and differ only by which end of the sibling is popped, which parent separator is
    /// exchanged, and which end of the current node receives the rotated entry.
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

/// This crate's one [`Rsos`](crate::Rsos) realization: an in-memory, `ArrayVec`-node B-tree (order
/// 6) that caches a per-subtree [`Aggregate`] at every node. See the [module docs](self) for the
/// full background and the [crate root docs](crate) for how its API maps onto the RSOS contract.
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
    /// Creates an empty tree.
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

    /// Calls `callback` with a mutable reference to the value at `key` (or `None` if absent), then
    /// re-lifts the element and propagates the resulting fingerprint delta up to the root.
    ///
    /// This is the supported way to mutate a value in place: unlike the `#[cfg(test)]`-only
    /// `IterMut`, it keeps every cached [`Aggregate`] consistent, so `check_invariants` and
    /// `aggregate` stay correct afterward.
    pub fn with_mut<F: FnOnce(Option<&mut V>)>(&mut self, key: &K, callback: F) {
        fn aux<K: Serialize + Ord, V: Serialize, F: FnOnce(Option<&mut V>)>(
            node: &mut Node<K, V>,
            key: &K,
            callback: F,
        ) -> Fingerprint {
            match node.keys.binary_search(key) {
                Ok(index) => {
                    let v = Some(&mut node.values[index]);
                    callback(v);
                    // callback likely modified v, so we need to restore the aggregate invariants
                    let old_fp = node.fingerprints[index];
                    let new_fp = lift(key, &node.values[index]);
                    node.fingerprints[index] = new_fp;
                    // signed delta to apply to this node and every ancestor's fingerprint. The
                    // element count is unchanged (one value was overwritten in place), so this
                    // touches only the Σ(S) half.
                    let diff_fp = new_fp - old_fp;
                    node.subtree =
                        Aggregate::new(node.subtree.size(), node.subtree.fingerprint() + diff_fp);
                    diff_fp
                }
                Err(index) => {
                    if let Some(children) = node.children.as_mut() {
                        let diff_fp = aux(children[index].as_mut(), key, callback);
                        node.subtree = Aggregate::new(
                            node.subtree.size(),
                            node.subtree.fingerprint() + diff_fp,
                        );
                        diff_fp
                    } else {
                        callback(None);
                        // callback cannot change the content of the tree, no invariant to restore
                        Fingerprint::ZERO
                    }
                }
            }
        }
        aux(self.root.as_mut(), key, callback);
    }

    /// Position of `key` in the in-order sequence, or `None` if it is not present.
    ///
    /// Unlike [`rank`](FingerprintTreeMap::rank), which always returns a position (the insertion
    /// point for an absent key), this distinguishes "absent" from "present at position 0".
    pub fn position(&self, key: &K) -> Option<usize> {
        fn aux<K: Ord, V>(node: &Node<K, V>, key: &K) -> Option<usize> {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = key.cmp(&node.keys[i]);
                    if cmp == Ordering::Less {
                        // recurse left to key
                        return aux(&children[i], key).map(|offset| index + offset);
                    }
                    // pass sub-tree
                    index += children[i].subtree.size();
                    if cmp == Ordering::Equal {
                        // found key
                        return Some(index);
                    }
                    // pass node
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
        // return:
        // - a key and node to be inserted after the current node
        // - the fingerprint difference
        // - the value that was at key, if any
        fn aux<K: Serialize + Ord, V: Serialize>(
            node: &mut Node<K, V>,
            key: K,
            value: V,
        ) -> (InsertionTuple<K, V>, Fingerprint, Option<V>) {
            match node.keys.binary_search(&key) {
                Ok(index) => {
                    let old_fp = node.fingerprints[index];
                    let new_fp = lift(&key, &value);
                    // signed delta to apply to this node and every ancestor; the element count is
                    // unchanged, so only the Σ(S) half moves.
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
                            // An overwrite leaves `|S|` alone; a genuine new element adds one.
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
        // return:
        // - the fingerprint diff
        // - the value at the key that was removed, if there was one
        fn aux<K: Ord, V>(node: &mut Node<K, V>, key: &K) -> (Fingerprint, Option<V>) {
            match node.keys.binary_search(key) {
                Ok(index) => {
                    if let Some(children) = node.children.as_mut() {
                        // The removed key's separator is replaced by its in-order predecessor,
                        // pulled up from the rightmost leaf of the left subtree.
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
                        // Nothing found below means `diff_fp` is `ZERO` and no element left, so
                        // this composes to a no-op — the two halves move together either way.
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

    /// Walks the whole tree, independently recomputing every cached [`Aggregate`], and asserts the
    /// B-tree ordering, minimum-occupancy and height invariants alongside it.
    ///
    /// # Panics
    ///
    /// Panics if any invariant is violated — a bug in the tree's mutation logic rather than a
    /// condition callers can trigger through the public API. Intended for tests, not production
    /// call sites, given the `O(n)` cost.
    pub fn check_invariants(&self) {
        // return:
        // - the independently recomputed aggregate of the sub-tree
        // - the height of the sub-tree
        fn aux<'a, K: Serialize + Ord, V: Serialize>(
            node: &'a Node<K, V>,
            mut min: Option<&'a K>,
            max: Option<&K>,
        ) -> (Aggregate, usize) {
            let mut cum = Aggregate::ZERO;
            let mut max_height = 1;
            // check node size
            if min.is_some() || max.is_some() {
                // this is not the root
                assert!(
                    node.keys.len() >= MIN_CAPACITY,
                    "minimum node size invariant violated"
                );
            }
            // check order
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
            // One assertion, not two: the cached aggregate is a single value now, so a drift
            // between its halves is not even expressible.
            assert_eq!(cum, node.subtree, "subtree aggregate invariant violated");
            (cum, max_height + 1)
        }
        aux(&self.root, None, None);
    }
}

impl<K, V> PartialEq for FingerprintTreeMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.root.subtree.fingerprint() == other.root.subtree.fingerprint()
    }
}

impl<K, V> Eq for FingerprintTreeMap<K, V> {}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for FingerprintTreeMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Bundled [`Aggregate`] over a range of keys: Def. 3.5's `A(S) = (|S|, Σ(S))`, answered in a
    /// single `O(log n)` tree walk from each node's cached `subtree` aggregate (see the internal
    /// `Node` type), which *is* one [`Aggregate`] value rather than two separately-maintained
    /// halves. This is [`Rsos::aggregate`](crate::Rsos::aggregate)'s realization, under the same
    /// name.
    ///
    /// `pub` — it is reconciliation mechanism (it drives range-based-set-reconciliation protocols
    /// such as this workspace's `rbsr`-to-be), not general-purpose public API, but reachable
    /// across the crate boundary now that `FingerprintTreeMap` lives in its own crate. Callers that
    /// only need `Σ(S)` write `.aggregate(range).fingerprint()`: identical cost, the same single
    /// tree walk.
    pub fn aggregate<R: RangeBounds<K>>(&self, range: &R) -> Aggregate {
        fn aux<'a, K: Ord, V, R: RangeBounds<K>>(
            node: &'a Node<K, V>,
            range: &R,
            mut lower_bound: Option<&'a K>,
            upper_bound: Option<&K>,
        ) -> Aggregate {
            // check if the lower-bound is included in the range
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
            // check if the upper-bound is included in the range
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
            // if both lower and upper bounds are included in the range, the node's whole cached
            // subtree aggregate is the answer
            if lower_bound_included && upper_bound_included {
                return node.subtree;
            }
            // otherwise, recurse in the relevant sub-trees

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
        aux(&self.root, range, None, None)
    }

    /// Position of `key` in the in-order sequence if present, or the position it would occupy
    /// after insertion otherwise. This realizes Def. 3.9's `Rank` operation (see
    /// [`Rsos::rank`](crate::Rsos::rank)); `pub` so range-based-set-reconciliation protocol
    /// crates outside `rsos` can drive it.
    ///
    /// Named `rank` rather than the home-grown `insertion_position` it used to carry: `rank` is
    /// both the paper's term and the standard order-statistic-tree term, and Rust has no competing
    /// convention for this operation, so the trait and the inherent API can share one name.
    pub fn rank(&self, key: &K) -> usize {
        fn aux<K: Ord, V>(node: &Node<K, V>, key: &K) -> usize {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = key.cmp(&node.keys[i]);
                    if cmp == Ordering::Less {
                        // recurse left to key
                        return index + aux(&children[i], key);
                    }
                    // pass sub-tree
                    index += children[i].subtree.size();
                    if cmp == Ordering::Equal {
                        // found key
                        return index;
                    }
                    // pass node
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

    /// Reference to the key at the given in-order position. Panics if out of bounds. This
    /// realizes Def. 3.9's `Select` operation (see [`Rsos::select`](crate::Rsos::select));
    /// `pub` so range-based-set-reconciliation protocol crates outside `rsos` can drive it.
    ///
    /// Named `select` rather than the home-grown `key_at` it used to carry, for the same reason as
    /// [`rank`](Self::rank): the paper's term is also the standard order-statistic-tree term, and
    /// no Rust convention competes for it.
    pub fn select(&self, index: usize) -> &K {
        fn aux<K: Ord, V>(node: &Node<K, V>, mut index: usize) -> &K {
            if let Some(children) = node.children.as_ref() {
                for i in 0..node.keys.len() {
                    if index < children[i].subtree.size() {
                        // recurse
                        return aux(&children[i], index);
                    }
                    // pass sub-tree
                    index -= children[i].subtree.size();
                    // check node
                    if index == 0 {
                        return &node.keys[i];
                    }
                    // pass node
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
    pub fn len(&self) -> usize {
        self.root.subtree.size()
    }

    /// Whether the tree holds no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Iterator over the key-value pairs of a [`FingerprintTreeMap`] whose key falls within a range, in
/// key order. Returned by [`FingerprintTreeMap::range`].
pub struct ItemRange<'a, K, V, R: RangeBounds<K>> {
    range: &'a R,
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
}

impl<K: Ord, V> FingerprintTreeMap<K, V> {
    /// Iterator over the key-value pairs whose key falls in `range`, in key order. This realizes
    /// Def. 3.9's `Enumerate` operation (see [`Rsos::enumerate`](crate::Rsos::enumerate)).
    ///
    /// Named `range` rather than the paper's `enumerate` — this is the one place the two
    /// vocabularies genuinely conflict. [`BTreeMap::range`](std::collections::BTreeMap::range) is
    /// std's spelling for exactly this operation, and `enumerate` on a Rust API would suggest
    /// [`Iterator::enumerate`]'s `(index, item)` pairing, which is not what this yields. The
    /// trait keeps `enumerate` because it is explicitly the paper's contract; the inherent API
    /// keeps Rust's. (The former name here, `get_range`, matched neither.)
    pub fn range<'a, R: RangeBounds<K>>(&'a self, range: &'a R) -> ItemRange<'a, K, V, R> {
        let mut stack = Vec::new();
        let mut node = self.root.as_ref();
        // traverse interior nodes
        'main_loop: while let Some(children) = node.children.as_ref() {
            for i in 0..node.keys.len() {
                match node.keys[i].rcmp(range) {
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
            match node.keys[i].rcmp(range) {
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
    fn test_aggregate() {
        // empty
        let mut tree = FingerprintTreeMap::new();
        assert_eq!(tree.aggregate(&..), Aggregate::ZERO);
        tree.check_invariants();

        // 1 value
        tree.insert(50, "Hello");
        tree.check_invariants();
        let agg1 = tree.aggregate(&..);
        assert_eq!(agg1.size(), 1);
        // The `assert_ne!`s below compare *fingerprints*, not whole aggregates: with different
        // element counts an aggregate-level `!=` would hold trivially on the size half and stop
        // saying anything about Σ(S), which is what these assertions exist to check.
        assert_ne!(agg1.fingerprint(), Fingerprint::ZERO);

        // 2 values
        tree.insert(25, "World!");
        tree.check_invariants();
        let agg2 = tree.aggregate(&..);
        assert_eq!(agg2.size(), 2);
        assert_ne!(agg2.fingerprint(), Fingerprint::ZERO);
        assert_ne!(agg2.fingerprint(), agg1.fingerprint());

        // 3 values
        tree.insert(75, "Everyone!");
        tree.check_invariants();
        let agg3 = tree.aggregate(&..);
        assert_eq!(agg3.size(), 3);
        assert_ne!(agg3.fingerprint(), Fingerprint::ZERO);
        assert_ne!(agg3.fingerprint(), agg1.fingerprint());
        assert_ne!(agg3.fingerprint(), agg2.fingerprint());

        // back to 2 values: both halves must return to their earlier state, so this one compares
        // the whole aggregate.
        tree.remove(&75);
        tree.check_invariants();
        assert_eq!(tree.aggregate(&..), agg2);
    }

    #[test]
    fn big_test() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut tree1 = FingerprintTreeMap::new();
        let mut key_values = Vec::new();

        // Independently accumulated expectation for the *whole* bundled aggregate: every insert
        // composes one more single-element aggregate `(1, lift(k, v))` through `⊗`.
        let mut expected = Aggregate::ZERO;

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen();
            let value: u64 = rng.gen();
            let old = tree1.insert(key, value);
            assert!(old.is_none());
            tree1.check_invariants();
            expected += Aggregate::new(1, super::lift(&key, &value));
            assert_eq!(tree1.aggregate(&..), expected);
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
        // Proper sub-ranges: compare fingerprints, since the sizes differ anyway and an
        // aggregate-level `!=` would say nothing about Σ(S).
        assert_ne!(
            tree1.aggregate(&(mid..)).fingerprint(),
            tree1.aggregate(&..).fingerprint()
        );
        assert_ne!(
            tree1.aggregate(&..mid).fingerprint(),
            tree1.aggregate(&..).fingerprint()
        );
        // The Def. 3.5 monoid homomorphism, over *both* halves at once: composing the aggregates
        // of a partition of the key space with `⊗` must reproduce the aggregate of the whole.
        assert_eq!(
            tree1.aggregate(&..mid) + tree1.aggregate(&(mid..)),
            tree1.aggregate(&..)
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
                tree.range(&range)
                    .map(|(k, v)| (*k, *v))
                    .collect::<Vec<_>>(),
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

        // NOTE: the diff-protocol exchange between `tree1`/`tree2` used to be exercised here too,
        // but the anti-entropy protocol (`proto`/future `rbsr`) lives in a different crate now
        // (this crate is a leaf with no dependency on it) — that coverage lives in the
        // `reconcile` crate's own tests (`tests/diff.rs`, `tests/proptest_fingerprint_tree_map.rs`) instead.

        // remove everything one-by-one
        key_values.shuffle(&mut rng);
        for (key, value) in key_values {
            let value2 = tree1.remove(&key);
            tree1.check_invariants();
            assert_eq!(value2, Some(value));
            // `Aggregate` is a monoid, not a group (no `Sub`) — removal decomposes into the
            // count and the fingerprint, the latter of which *is* an abelian group.
            expected = Aggregate::new(
                expected.size() - 1,
                expected.fingerprint() - super::lift(&key, &value),
            );
            assert_eq!(tree1.aggregate(&..), expected);
        }
    }

    /// The bundled [`Aggregate`]'s count half (Def. 3.5's `A(S) = (|S|, Σ(S))`) must agree with
    /// the independently-computed `range(range).count()` over a handful of ranges (empty,
    /// full, partial) on a tree with several dozen inserted keys. The fingerprint half is checked
    /// the same independent way: against the per-element lifts of `range`'s own contents,
    /// summed by hand.
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
            let aggregate = tree.aggregate(&range);
            assert_eq!(
                aggregate.size(),
                tree.range(&range).count(),
                "aggregate size disagrees with range().count() for {range:?}"
            );
            assert_eq!(
                aggregate.is_empty(),
                tree.range(&range).next().is_none(),
                "aggregate is_empty disagrees with range() for {range:?}"
            );
            let expected_fingerprint = tree
                .range(&range)
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
        assert_eq!(empty.aggregate(&..), Aggregate::ZERO);
    }
}
