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
//! - `IterMut`: mutable in-order traversal returning `(&K, &mut V)`;
//! - `IntoValues`: immutable in-order traversal consuming the tree and yielding `V`;
//! - `Values`: immutable in-order traversal yielding `&V`;
//! - `ValuesMut`: mutable in-order traversal yielding `&mut V`;
//! - `IntoKeys`: immutable in-order traversal consuming the tree and yielding `K`;
//! - `Keys`: immutable in-order traversal yielding `&K`;
//!
//! # Complexity
//!
//! All iterators perform an initial descent to the leftmost leaf in **O(h)** time, where *h* is the tree height (≈ log n).
//! Each call to `next()` then executes a constant amount of work plus at most one further descent (amortized **O(1)** per element).
//! Memory overhead is **O(h)** for the internal stack of node pointers.
//!
//! # Mutable iterators (fully safe)
//!
//! `IterMut` and `ValuesMut` are implemented in safe Rust, with **no raw pointers and no `unsafe`**.
//! The traversal keeps a stack of per-node frames, each holding the node's remaining `(key, value)`
//! pairs and its remaining children as standard slice iterators (`slice::Iter` / `slice::IterMut`).
//! This works because `slice::IterMut::next` hands out a `&'a mut V` whose lifetime is decoupled from
//! the borrow of the iterator itself, so distinct values can be yielded across many `next()` calls
//! without ever aliasing — which is exactly what the previous raw-pointer implementation emulated by
//! hand. The whole crate is `#![forbid(unsafe_code)]`.
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

/// A per-node frame of the [`IterMut`] traversal stack.
///
/// Holds the node's not-yet-yielded `(key, value)` pairs and its not-yet-visited children as
/// standard slice iterators. Yielding `&'a mut V` from `kv` is sound because `slice::IterMut`
/// decouples the lifetime of each returned reference from the borrow of the iterator itself.
struct Frame<'a, K, V> {
    kv: std::iter::Zip<std::slice::Iter<'a, K>, std::slice::IterMut<'a, V>>,
    children: Option<std::slice::IterMut<'a, Box<Node<K, V>>>>,
}

/// An in-order mutable iterator over a `HRTree`.
///
/// Yields each key and a mutable reference to its associated value in ascending key order.
pub struct IterMut<'a, K, V> {
    stack: Vec<Frame<'a, K, V>>,
}

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

impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns an in-order iterator over `(&K, &mut V)` pairs.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let mut tree = HRTree::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.iter_mut().collect();
    /// assert_eq!(pairs, vec![(&1, &mut "a"), (&2, &mut "b")]);
    /// ```
    pub fn iter_mut(&'a mut self) -> IterMut<'a, K, V> {
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

// === Values-only mutable iterator ===

/// A mutable in-order iterator yielding references to values only.
///
/// Useful when only the values need to be updated or inspected in sequence.
pub struct ValuesMut<'a, K, V> {
    inner: IterMut<'a, K, V>,
}

impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns an in-order iterator over `&mut V` values.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use reconcile::HRTree;
    /// let mut tree = HRTree::from_iter(vec![(1, 10), (2, 20)]);
    /// for val in tree.values_mut() {
    ///     *val += 1;
    /// }
    /// assert_eq!(tree.values().copied().collect::<Vec<_>>(), vec![11, 21]);
    /// ```
    pub fn values_mut(&'a mut self) -> ValuesMut<'a, K, V> {
        ValuesMut {
            inner: self.iter_mut(),
        }
    }
}

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

    #[test]
    fn test_iter_mut() {
        let mut tree = make_tree();
        let collected: Vec<_> = tree.iter_mut().map(|(_, v)| *v).collect();
        let expected: Vec<_> = BASE_ITEMS.iter().map(|&(_, v)| v).collect();
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
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
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
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
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
        // mutable
        let mut empty_mut = empty.clone();
        assert!(empty_mut.iter_mut().next().is_none());
        assert!(empty_mut.values_mut().next().is_none());
    }

    #[test]
    fn test_all_iterators_single_leaf() {
        let mut single = HRTree::new();
        single.insert(42, 99);
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
        // mutable
        for (_k, v) in single.iter_mut() {
            *v += 1;
        }
        assert_eq!(single.iter().collect::<Vec<_>>(), vec![(&42, &100)]);
        for val in single.values_mut() {
            *val *= 2;
        }
        assert_eq!(single.values().copied().collect::<Vec<_>>(), vec![200]);
    }
}
