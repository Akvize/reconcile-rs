// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::borrow::Borrow;

use arrayvec::ArrayVec;
use serde::Serialize;
use tracing::trace;

use crate::aggregate::Aggregate;
use crate::fingerprint::{lift, Fingerprint};

use super::node::Node;
use super::{element, without, FingerprintTreeMap, InsertionTuple, MIN_CAPACITY};

impl<K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
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
}

impl<K: Ord, V> FingerprintTreeMap<K, V> {
    /// Removes `key`, returning its value if it was present.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
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
        fn aux<K: Borrow<Q>, V, Q: Ord + ?Sized>(
            node: &mut Node<K, V>,
            key: &Q,
        ) -> (Fingerprint, Option<V>) {
            match node.keys.binary_search_by(|probe| probe.borrow().cmp(key)) {
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
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let mut map: FingerprintTreeMap<i32, i32> = (0..6).map(|k| (k, k * 10)).collect();
    /// map.retain(|k, _| k % 2 == 0);
    ///
    /// // Only the entries `keep` approved survive, in the same key order.
    /// assert_eq!(
    ///     map.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(),
    ///     vec![(0, 0), (2, 20), (4, 40)]
    /// );
    /// ```
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
}

impl<K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
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
                max_height = child_height;
            }
            assert_eq!(cum, node.subtree, "subtree aggregate invariant violated");
            (cum, max_height + 1)
        }
        let (_, height) = aux(&self.root, None, None);

        // Height must count actual levels, not just agree between siblings: walk straight down
        // the leftmost path (an independent measurement) and check it agrees with `aux`'s own
        // count. `aux` counts one level higher than the walk (a leaf with no children still
        // reports height 2, since `max_height` starts at 1 and is never touched before the
        // final `+ 1`), hence the `+ 1` below.
        let mut walked_height = 1;
        let mut node = self.root.as_ref();
        while let Some(children) = node.children.as_ref() {
            walked_height += 1;
            node = &children[0];
        }
        assert_eq!(
            height,
            walked_height + 1,
            "recursive height computation disagrees with a direct walk to a leaf"
        );
    }

    /// A view onto `key`'s slot, for a get-or-insert an `rsos`-only dependent otherwise has no way
    /// to express (`with_mut` already covers in-place mutation of a *known-present* key).
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let mut map: FingerprintTreeMap<&str, i32> = FingerprintTreeMap::new();
    /// map.entry("a").or_insert(1);
    /// map.entry("a").and_modify(|v| *v += 10).or_insert(0);
    /// assert_eq!(map.get("a"), Some(&11));
    /// ```
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V>
    where
        K: Clone,
    {
        Entry { map: self, key }
    }
}

/// A view into a single slot of a [`FingerprintTreeMap`], from [`FingerprintTreeMap::entry`].
///
/// Closure-shaped, not the `&mut V`-handing-out guard `std::collections::btree_map::Entry` offers:
/// [`with_mut`](FingerprintTreeMap::with_mut) is the crate's only summary-safe path to a mutable
/// reference precisely because it re-lifts from a [`Drop`] guard: a bare `&mut V` returned here
/// could be mutated after the entry is gone with no chance to repair the cached fingerprint, and a
/// `Drop`-guard-returning `or_insert` would be unsound under `mem::forget`. So every method below
/// either returns a shared `&V` (reading never skips a re-lift) or takes the mutation as a closure
/// (`and_modify`), which runs through `with_mut` internally.
pub struct Entry<'a, K, V> {
    map: &'a mut FingerprintTreeMap<K, V>,
    key: K,
}

impl<'a, K: Serialize + Ord + Clone, V: Serialize> Entry<'a, K, V> {
    /// Inserts `default` if the entry is vacant, then returns the (possibly just-inserted) value.
    pub fn or_insert(self, default: V) -> &'a V {
        self.or_insert_with(|| default)
    }

    /// Inserts the result of `default` if the entry is vacant, then returns the (possibly
    /// just-inserted) value. `default` runs only when the entry is vacant.
    pub fn or_insert_with(self, default: impl FnOnce() -> V) -> &'a V {
        if !self.map.contains_key(&self.key) {
            self.map.insert(self.key.clone(), default());
        }
        self.map
            .get(&self.key)
            .expect("just inserted, or already present")
    }

    /// Inserts `V::default()` if the entry is vacant, then returns the (possibly just-inserted)
    /// value.
    pub fn or_default(self) -> &'a V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }

    /// Calls `f` with a mutable reference to the value if the entry is occupied, re-lifting
    /// afterward via [`with_mut`](FingerprintTreeMap::with_mut) — a no-op on a vacant entry.
    /// Returns `self` so it can chain into `or_insert`/`or_insert_with`/`or_default`.
    pub fn and_modify(self, f: impl FnOnce(&mut V)) -> Self {
        self.map.with_mut(&self.key, |v| {
            if let Some(v) = v {
                f(v);
            }
        });
        self
    }
}
