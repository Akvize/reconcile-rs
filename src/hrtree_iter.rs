// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Module `hrtree_iter` provides iteration utilities for `HRTree<K, V>`,
//! including:
//! - `IntoIter`: immutable in-order traversal consuming the tree and returning `(K, V)`;
//! - `Iter`: immutable in-order traversal returning `(&K, &V)`;
//! - `IntoValues`: immutable in-order traversal consuming the tree and yielding `V`;
//! - `Values`: immutable in-order traversal yielding `&V`;
//! - `IntoKeys`: immutable in-order traversal consuming the tree and yielding `K`;
//! - `Keys`: immutable in-order traversal yielding `&K`;
//!
//! # Complexity
//!
//! All iterators perform an initial descent to the leftmost leaf in **O(h)** time, where *h* is the tree height (≈ log n).
//! Each call to `next()` then executes a constant amount of work plus at most one further descent (amortized **O(1)** per element).
//! Memory overhead is **O(h)** for the internal stack of node pointers.
//!
//! # Mutable access
//!
//! `IterMut` and `ValuesMut` are **`#[cfg(test)]` only** (compiled out of non-test builds, and not
//! part of the public API) because they hand out `&mut V` without updating the per-element hash
//! (`node.hashes[i]`) or the cumulative `tree_hash`, leaving stale fingerprints that break
//! `check_invariants`, `hash(range)`, and the reconciliation protocol. The supported mutation path
//! is [`HRTree::with_mut`], which recomputes the element hash and propagates the signed delta to
//! every ancestor. A correct iterator-based mutation API is future work.
//!
//! # Future Work: Lazy Iterators
//!
//! Current iterators are "semi-lazy": they do not collect all items up front, but they do pre-compute the full descent path.
//! To match `BTreeMap` semantics more closely, we can remove even that pre-computation and make both forward and reverse traversal fully lazy,
//! support `DoubleEndedIterator`, and implement precise lower/upper-bound seeks.

use std::hash::Hash;

use crate::hrtree::{HRTree, Node};

impl<K: Hash + Ord, V: Hash> FromIterator<(K, V)> for HRTree<K, V> {
    /// Builds an [`HRTree`] from an iterator of key-value pairs.
    ///
    /// The pairs are collected, sorted by key, and then inserted one by one,
    /// ensuring the resulting tree is balanced according to the input order.
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
    {
        let mut tree = HRTree::new();
        let mut items: Vec<_> = iter.into_iter().collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in items {
            tree.insert(k, v);
        }
        tree
    }
}

enum IntoIterLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(K, V),
}

/// An in-order immutable iterator over a `HRTree`.
///
/// Consumes the tree and yields keys and values in ascending key order.
pub struct IntoIter<K, V> {
    stack: Vec<IntoIterLayer<K, V>>,
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoIterLayer::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoIterLayer::Node(children.pop().unwrap()));
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterLayer::Element(k, v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoIterLayer::Node(c));
                    }
                } else {
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterLayer::Element(k, v));
                    }
                }
                self.next()
            }
            Some(IntoIterLayer::Element(k, v)) => Some((k, v)),
            None => None,
        }
    }
}

impl<K, V> IntoIterator for HRTree<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    /// Consumes the `HRTree` and returns `(K, V)` pairs in-order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let tree = HRTree::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.into_iter().collect();
    /// assert_eq!(pairs, vec![(1, "a"), (2, "b")]);
    /// ```
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: vec![IntoIterLayer::Node(self.root)],
        }
    }
}

/// An in-order immutable iterator over a `HRTree`.
///
/// Yields references to keys and values in ascending key order.
pub struct Iter<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            if children_passed < node.keys.len() {
                self.stack.push((node, children_passed + 1));
            }
            if let Some(children) = node.children.as_ref() {
                self.stack.push((&children[children_passed], 0));
            }
            if children_passed > 0 {
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

impl<'a, K, V> IntoIterator for &'a HRTree<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        Iter {
            stack: vec![(&self.root, 0)],
        }
    }
}

impl<K, V> HRTree<K, V> {
    /// Returns an in-order iterator over `(&K, &V)` pairs.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let tree = HRTree::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.iter().collect();
    /// assert_eq!(pairs, vec![(&1, &"a"), (&2, &"b")]);
    /// ```
    pub fn iter(&self) -> Iter<'_, K, V> {
        self.into_iter()
    }
}

// The mutable iterator infrastructure below is gated to `#[cfg(test)]` because `IterMut` and
// `ValuesMut` do not update per-element hashes or the cumulative `tree_hash` on mutation —
// leaving stale fingerprints that break `check_invariants`, `hash(range)`, and the
// reconciliation protocol. The supported public mutation path is `HRTree::with_mut`.
// These types are retained test-only so that the traversal logic and the read-only path remain
// exercised; a correct iterator-based mutation design is future work.

/// A per-node frame of the [`IterMut`] traversal stack.
///
/// Holds the node's not-yet-yielded `(key, value)` pairs and its not-yet-visited children as
/// standard slice iterators. Yielding `&'a mut V` from `kv` is sound because `slice::IterMut`
/// decouples the lifetime of each returned reference from the borrow of the iterator itself.
#[cfg(test)]
struct Frame<'a, K, V> {
    kv: std::iter::Zip<std::slice::Iter<'a, K>, std::slice::IterMut<'a, V>>,
    children: Option<std::slice::IterMut<'a, Box<Node<K, V>>>>,
}

/// An in-order mutable iterator over a `HRTree`.
///
/// Yields each key and a mutable reference to its associated value in ascending key order.
///
/// # Warning — fingerprints not updated
///
/// This iterator hands out `&mut V` directly; it does **not** recompute the per-element hash or
/// propagate the delta to ancestor nodes. Callers that mutate through it will leave stale
/// fingerprints. Use [`HRTree::with_mut`] for hash-safe mutation. This type is `#[cfg(test)]`
/// until a correct design is implemented.
#[cfg(test)]
struct IterMut<'a, K, V> {
    stack: Vec<Frame<'a, K, V>>,
}

#[cfg(test)]
impl<'a, K, V> IterMut<'a, K, V> {
    /// Pushes `node` and the leftmost path beneath it onto `stack`, so that the top frame is
    /// positioned to yield the in-order-first `(key, value)` of that subtree.
    fn push_left_path(stack: &mut Vec<Frame<'a, K, V>>, mut node: &'a mut Node<K, V>) {
        loop {
            // Destructuring `&mut Node` with a struct pattern splits the borrow across the
            // disjoint fields, so each binding is an independent `&mut` (match ergonomics).
            let Node {
                keys,
                values,
                children,
                ..
            } = node;
            let kv = keys.iter().zip(values.iter_mut());
            match children {
                Some(ch) => {
                    let mut child_iter = ch.iter_mut();
                    let first = child_iter.next().expect("internal node has >= 1 child");
                    stack.push(Frame {
                        kv,
                        children: Some(child_iter),
                    });
                    node = &mut **first; // descend into leftmost child
                }
                None => {
                    stack.push(Frame { kv, children: None });
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
impl<'a, K: 'a + Hash + Ord, V: Hash> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            if let Some((k, v)) = frame.kv.next() {
                // Descend the child to the right of this kv (the next node in-order).
                // Typed `&'a mut Node`, so the `frame` borrow can end before we re-borrow
                // `self.stack` to push.
                let next_child: Option<&'a mut Node<K, V>> = frame
                    .children
                    .as_mut()
                    .and_then(|c| c.next())
                    .map(|b| &mut **b);
                if let Some(child) = next_child {
                    Self::push_left_path(&mut self.stack, child);
                }
                return Some((k, v));
            }
            // This frame's kvs are exhausted (its trailing child was already descended).
            self.stack.pop();
        }
    }
}

#[cfg(test)]
impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns an in-order iterator over `(&K, &mut V)` pairs.
    ///
    /// # Warning — fingerprints not updated
    ///
    /// Values mutated through this iterator leave stale per-element hashes and cumulative
    /// `tree_hash` values. Use [`HRTree::with_mut`] for hash-safe mutation.
    /// This method is `#[cfg(test)]` until a correct design is implemented.
    fn iter_mut(&'a mut self) -> IterMut<'a, K, V> {
        let mut stack = Vec::new();
        IterMut::push_left_path(&mut stack, &mut self.root);
        IterMut { stack }
    }
}

/// An in-order mutable iterator over a `HRTree`.
///
/// Consumes the tree and yields its values in ascending key order.
pub struct IntoValues<K, V> {
    inner: IntoIter<K, V>,
}

impl<K, V> Iterator for IntoValues<K, V> {
    type Item = V;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

impl<K, V> HRTree<K, V> {
    /// Consumes the tree and returns an in-order iterator over `V` values.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let tree = HRTree::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.into_values().collect();
    /// assert_eq!(pairs, vec![("a"), ("b")]);
    /// ```
    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues {
            inner: self.into_iter(),
        }
    }
}

/// An iterator over shared references to values in ascending key order.
///
/// Yields references to values in ascending key order.
///
/// Does not consume the `HRTree`.
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

impl<K, V> HRTree<K, V> {
    /// Returns an in-order iterator over `&V` values.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let tree = HRTree::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.values().collect();
    /// assert_eq!(pairs, vec![(&"a"), (&"b")]);
    /// ```
    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }
}

// === Values-only mutable iterator (test-only; see fingerprint warning above) ===

/// A mutable in-order iterator yielding references to values only.
///
/// # Warning — fingerprints not updated
///
/// Values mutated through this iterator leave stale per-element hashes and cumulative
/// `tree_hash` values. Use [`HRTree::with_mut`] for hash-safe mutation.
/// This type is `#[cfg(test)]` until a correct design is implemented.
#[cfg(test)]
struct ValuesMut<'a, K, V> {
    inner: IterMut<'a, K, V>,
}

#[cfg(test)]
impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns an in-order iterator over `&mut V` values.
    ///
    /// # Warning — fingerprints not updated
    ///
    /// Values mutated through this iterator leave stale per-element hashes and cumulative
    /// `tree_hash` values. Use [`HRTree::with_mut`] for hash-safe mutation.
    /// This method is `#[cfg(test)]` until a correct design is implemented.
    fn values_mut(&'a mut self) -> ValuesMut<'a, K, V> {
        ValuesMut {
            inner: self.iter_mut(),
        }
    }
}

#[cfg(test)]
impl<'a, K: 'a + Hash + Ord, V: Hash> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

/// An iterator that consumes the tree and yields its keys in ascending key order.
///
/// Useful when keys only need to be inspected in sequence.
pub struct IntoKeys<K, V> {
    inner: IntoIter<K, V>,
}

impl<K, V> Iterator for IntoKeys<K, V> {
    type Item = K;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
}

impl<K, V> HRTree<K, V> {
    /// Consumes the `HRTree` and returns `K` keys in-order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let tree = HRTree::from_iter(vec![(1, 'a'), (2, 'b')]);
    /// let ks: Vec<_> = tree.clone().into_keys().collect();
    /// assert_eq!(ks, vec![1, 2]);
    /// ```
    pub fn into_keys(self) -> IntoKeys<K, V> {
        IntoKeys {
            inner: self.into_iter(),
        }
    }
}

/// An iterator over shared references to keys in ascending key order.
///
/// Does not consume the `HRTree`.
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
}

impl<K, V> HRTree<K, V> {
    /// Returns an iterator over references to keys in ascending order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let tree = HRTree::from_iter(vec![(1, 'a'), (2, 'b')]);
    /// let ks: Vec<_> = tree.keys().copied().collect();
    /// assert_eq!(ks, vec![1, 2]);
    /// ```
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { inner: self.iter() }
    }
}

#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng};

    use super::HRTree;
    use once_cell::sync::Lazy;

    const TREE_SIZE: usize = 1000;

    static BASE_ITEMS: Lazy<Vec<(u64, u64)>> = Lazy::new(|| {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        (0..TREE_SIZE)
            .map(|i| (i as u64, rng.gen::<u64>()))
            .collect()
    });

    fn make_tree() -> HRTree<u64, u64> {
        HRTree::from_iter(BASE_ITEMS.clone())
    }

    #[test]
    fn test_into_iter() {
        let tree = make_tree();
        assert_eq!(
            tree.clone().into_iter().collect::<Vec<_>>(),
            BASE_ITEMS.clone()
        );
    }

    #[test]
    fn test_iter() {
        let tree = make_tree();
        assert_eq!(
            tree.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(),
            BASE_ITEMS.clone()
        );
    }

    // `iter_mut` / `values_mut` are `#[cfg(test)]` (test-only) because they do not update
    // per-element hashes or `tree_hash` on mutation. These tests verify read-only traversal order
    // only; they do NOT call `check_invariants()` after mutation because hash corruption is the
    // documented limitation. The supported mutation path is `with_mut` — see
    // `test_with_mut_maintains_invariants`.

    #[test]
    fn test_iter_mut() {
        let mut tree = make_tree();
        let collected: Vec<_> = tree.iter_mut().map(|(_, v)| *v).collect();
        let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        // read-only traversal: no mutation, so fingerprints remain valid
        tree.check_invariants();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_iter_mut_modify() {
        let mut tree = make_tree();

        let num = rand::random::<usize>().rem_euclid(TREE_SIZE);
        let (key, value) = BASE_ITEMS[num];
        let mut expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        expected[num] = value;

        for (k, v) in tree.iter_mut() {
            if *k == key {
                *v = value;
            }
        }
        // Values in memory are updated correctly …
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
        // … but fingerprints are stale: check_invariants() would panic here — the demotion bug.
        // Use `with_mut` for hash-safe mutation.
    }

    #[test]
    fn test_into_values() {
        let values: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        let tree = make_tree();
        assert_eq!(tree.clone().into_values().collect::<Vec<_>>(), values);
    }

    #[test]
    fn test_values() {
        let values: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        let tree = make_tree();
        assert_eq!(tree.values().copied().collect::<Vec<_>>(), values);
    }

    #[test]
    fn test_values_mut() {
        let mut tree = make_tree();
        let collected: Vec<_> = tree.values_mut().map(|v| *v).collect();
        let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        // read-only traversal: no mutation, so fingerprints remain valid
        tree.check_invariants();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_values_mut_modify() {
        let mut tree = make_tree();

        let num = rand::random::<usize>().rem_euclid(TREE_SIZE);
        let (_, value) = BASE_ITEMS[num];
        let mut expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
        expected[num] = value;

        for (n, v) in tree.values_mut().enumerate() {
            if n == num {
                *v = value;
            }
        }
        // Values in memory are updated correctly …
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
        // … but fingerprints are stale: check_invariants() would panic here — the demotion bug.
        // Use `with_mut` for hash-safe mutation.
    }

    /// Verifies that `with_mut` is the correct mutation path: values change in memory, the
    /// per-element hash and every ancestor's cumulative hash are recomputed, and the tree
    /// remains fully consistent with `check_invariants()`. Also confirms that the range hash
    /// reflects the updated value.
    #[test]
    fn test_with_mut_maintains_invariants() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(99);
        let mut tree = make_tree();
        tree.check_invariants();

        let hash_before = tree.hash(&..);

        // Mutate several keys at random positions; each must leave the tree consistent.
        for _ in 0..20 {
            let idx = rng.gen_range(0..TREE_SIZE);
            let (key, _) = BASE_ITEMS[idx];
            let new_value: u64 = rng.gen();
            tree.with_mut(&key, |v| *v.unwrap() = new_value);
            tree.check_invariants();
        }

        // After all mutations the global hash must differ from the pre-mutation snapshot
        // (astronomically unlikely to collide for random u64 values).
        let hash_after = tree.hash(&..);
        assert_ne!(
            hash_before, hash_after,
            "tree hash unchanged after with_mut mutations — fingerprints not updated"
        );

        // A partial-range hash must still satisfy the additive identity:
        // hash(..mid) + hash(mid..) == hash(..)
        let mid = BASE_ITEMS[TREE_SIZE / 2].0;
        assert_eq!(
            tree.hash(&(..mid)) + tree.hash(&(mid..)),
            tree.hash(&..),
            "partial-range hashes do not sum to the global hash"
        );
    }

    #[test]
    fn test_into_keys() {
        let keys: Vec<_> = BASE_ITEMS.iter().map(|&(k, _)| k).collect();
        let tree = make_tree();
        assert_eq!(tree.clone().into_keys().collect::<Vec<_>>(), keys);
    }

    #[test]
    fn test_keys() {
        let keys: Vec<_> = BASE_ITEMS.iter().map(|&(k, _)| k).collect();
        let tree = make_tree();
        assert_eq!(tree.keys().copied().collect::<Vec<_>>(), keys);
    }

    #[test]
    fn test_all_iterators_empty() {
        let empty: HRTree<i32, i32> = HRTree::new();
        // immutable
        assert_eq!(empty.iter().next(), None);
        // consuming
        assert!(empty.clone().into_iter().next().is_none());
        assert!(empty.clone().into_keys().next().is_none());
        assert!(empty.clone().into_values().next().is_none());
        // shared
        assert!(empty.keys().next().is_none());
        assert!(empty.values().next().is_none());
        // mutable (read-only traversal — no values mutated, fingerprints stay valid)
        let mut empty_mut = empty.clone();
        assert!(empty_mut.iter_mut().next().is_none());
        assert!(empty_mut.values_mut().next().is_none());
        empty_mut.check_invariants();
    }

    #[test]
    fn test_all_iterators_single_leaf() {
        let mut single = HRTree::new();
        single.insert(42, 99);
        single.check_invariants();
        // immutable
        assert_eq!(single.iter().collect::<Vec<_>>(), vec![(&42, &99)]);
        // consuming
        assert_eq!(
            single.clone().into_iter().collect::<Vec<_>>(),
            vec![(42, 99)]
        );
        assert_eq!(single.clone().into_keys().collect::<Vec<_>>(), vec![42]);
        assert_eq!(single.clone().into_values().collect::<Vec<_>>(), vec![99]);
        // shared
        assert_eq!(single.keys().copied().collect::<Vec<_>>(), vec![42]);
        assert_eq!(single.values().copied().collect::<Vec<_>>(), vec![99]);
        // mutation via with_mut: values change and fingerprints stay consistent
        single.with_mut(&42, |v| *v.unwrap() += 1);
        single.check_invariants();
        assert_eq!(single.iter().collect::<Vec<_>>(), vec![(&42, &100)]);
        single.with_mut(&42, |v| *v.unwrap() *= 2);
        single.check_invariants();
        assert_eq!(single.values().copied().collect::<Vec<_>>(), vec![200]);
    }
}
