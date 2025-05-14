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

use std::{hash::Hash, marker::PhantomData};

use crate::hrtree::{HRTree, Node};

impl<K: Hash + Ord, V: Hash> FromIterator<(K, V)> for HRTree<K, V> {
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
    pub fn iter(&self) -> Iter<K, V> {
        self.into_iter()
    }
}

/// An in-order mutable iterator over a `HRTree`.
///
/// Yields each key and a mutable reference to its associated value in ascending key order.
pub struct IterMut<'a, K, V> {
    stack: Vec<(*mut Node<K, V>, usize)>,
    _marker: PhantomData<&'a mut V>,
}

impl<'a, K: 'a + Hash + Ord, V: Hash> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node_ptr, idx)) = self.stack.pop() {
            unsafe {
                let node = &mut *node_ptr;
                if idx < node.keys.len() {
                    // Prepare next
                    self.stack.push((node_ptr, idx + 1));
                    // Traverse right subtree
                    if let Some(children) = node.children.as_mut() {
                        let mut child_ptr: *mut Node<K, V> = &mut *children[idx + 1];
                        while let Some(gc) = (*child_ptr).children.as_mut() {
                            self.stack.push((child_ptr, 0));
                            child_ptr = &mut *gc[0];
                        }
                        self.stack.push((child_ptr, 0));
                    }
                    return Some((&node.keys[idx], &mut node.values[idx]));
                }
            }
        }
        None
    }
}

impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns an in-order iterator over `(&K, &mut V)` pairs.
    ///
    /// # Safety
    ///
    /// Uses raw pointers internally to allow multiple mutable borrows,
    /// but ensures safety by strictly controlling traversal and lifetime.
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
        // Unsafe pointer to root
        let mut cur_ptr: *mut Node<K, V> = &mut *self.root;
        unsafe {
            // Descend to leftmost leaf
            while let Some(children) = (*cur_ptr).children.as_mut() {
                stack.push((cur_ptr, 0));
                cur_ptr = &mut *children[0];
            }
            stack.push((cur_ptr, 0));
        }
        IterMut {
            stack,
            _marker: PhantomData,
        }
    }
}

enum IntoValuesLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(V),
}

/// An in-order mutable iterator over a `HRTree`.
///
/// Consumes the tree and yields its values in ascending key order.
pub struct IntoValues<K, V> {
    stack: Vec<IntoValuesLayer<K, V>>,
}

impl<K, V> Iterator for IntoValues<K, V> {
    type Item = V;
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoValuesLayer::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoValuesLayer::Node(children.pop().unwrap()));
                    while !node.values.is_empty() {
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoValuesLayer::Element(v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoValuesLayer::Node(c));
                    }
                } else {
                    while !node.values.is_empty() {
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoValuesLayer::Element(v));
                    }
                }
                self.next()
            }
            Some(IntoValuesLayer::Element(v)) => Some(v),
            None => None,
        }
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
            stack: vec![IntoValuesLayer::Node(self.root)],
        }
    }
}

/// An iterator over shared references to values in ascending key order.
///
/// Yields references to values in ascending key order.
///
/// Does not consume the `HRTree`.
pub struct Values<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            if children_passed < node.keys.len() {
                self.stack.push((node, children_passed + 1));
            }
            if let Some(children) = node.children.as_ref() {
                self.stack.push((&children[children_passed], 0));
            }
            if children_passed > 0 {
                Some(&node.values[children_passed - 1])
            } else {
                self.next()
            }
        } else {
            None
        }
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
        Values {
            stack: vec![(&self.root, 0)],
        }
    }
}

// === Values-only mutable iterator ===

/// A mutable in-order iterator yielding references to values only.
///
/// Useful when only the values need to be updated or inspected in sequence.
pub struct ValuesMut<'a, K, V> {
    stack: Vec<(*mut Node<K, V>, usize)>,
    _marker: PhantomData<&'a mut V>,
}

impl<'a, K: Hash + Ord, V: Hash> HRTree<K, V> {
    /// Returns an in-order iterator over `&mut V` values.
    ///
    /// # Safety
    ///
    /// Uses raw pointers internally to allow multiple mutable borrows,
    /// but ensures safety by strictly controlling traversal and lifetime.
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
        let mut stack = Vec::new();
        let mut cur_ptr: *mut Node<K, V> = &mut *self.root;
        unsafe {
            while let Some(children) = (*cur_ptr).children.as_mut() {
                stack.push((cur_ptr, 0));
                cur_ptr = &mut *children[0];
            }
            stack.push((cur_ptr, 0));
        }
        ValuesMut {
            stack,
            _marker: PhantomData,
        }
    }
}

impl<'a, K: Hash + Ord, V: Hash> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node_ptr, idx)) = self.stack.pop() {
            unsafe {
                let node = &mut *node_ptr;
                if idx < node.keys.len() {
                    self.stack.push((node_ptr, idx + 1));
                    if let Some(children) = node.children.as_mut() {
                        let mut child_ptr: *mut Node<K, V> = &mut *children[idx + 1] as *mut _;
                        while let Some(gc) = (*child_ptr).children.as_mut() {
                            self.stack.push((child_ptr, 0));
                            child_ptr = &mut *gc[0];
                        }
                        self.stack.push((child_ptr, 0));
                    }
                    return Some(&mut node.values[idx]);
                }
            }
        }
        None
    }
}

enum IntoKeysLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(K),
}

/// An iterator that consumes the tree and yields its keys in ascending key order.
///
/// Useful when keys only need to be inspected in sequence.
pub struct IntoKeys<K, V> {
    stack: Vec<IntoKeysLayer<K, V>>,
}

impl<K, V> Iterator for IntoKeys<K, V> {
    type Item = K;
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoKeysLayer::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack
                        .push(IntoKeysLayer::Node(children.pop().unwrap()));
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        self.stack.push(IntoKeysLayer::Element(k));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoKeysLayer::Node(c));
                    }
                } else {
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        self.stack.push(IntoKeysLayer::Element(k));
                    }
                }
                self.next()
            }
            Some(IntoKeysLayer::Element(k)) => Some(k),
            None => None,
        }
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
            stack: vec![IntoKeysLayer::Node(self.root)],
        }
    }
}

/// An iterator over shared references to keys in ascending key order.
///
/// Does not consume the `HRTree`.
pub struct Keys<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some((node, children_passed)) = self.stack.pop() {
            if children_passed < node.keys.len() {
                self.stack.push((node, children_passed + 1));
            }
            if let Some(children) = node.children.as_ref() {
                self.stack.push((&children[children_passed], 0));
            }
            if children_passed > 0 {
                Some(&node.keys[children_passed - 1])
            } else {
                self.next()
            }
        } else {
            None
        }
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
        Keys {
            stack: vec![(&self.root, 0)],
        }
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
