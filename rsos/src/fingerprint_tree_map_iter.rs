// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! In-order iterators over [`FingerprintTreeMap`]: `O(h)` initial descent, amortized `O(1)` per
//! `next()`, `O(h)` stack.
//!
//! `IterMut`/`ValuesMut` are `#[cfg(test)]`-only: they hand out `&mut V` without updating the
//! element fingerprint or the cached subtree aggregate. [`FingerprintTreeMap::with_mut`] is the
//! supported mutation path.

use serde::Serialize;

use crate::fingerprint_tree_map::{FingerprintTreeMap, Node};

impl<K: Serialize + Ord, V: Serialize> FromIterator<(K, V)> for FingerprintTreeMap<K, V> {
    /// Builds a [`FingerprintTreeMap`] from key-value pairs, sorted by key before insertion.
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
    {
        let mut tree = FingerprintTreeMap::new();
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

/// Consumes the tree, yielding `(K, V)` in ascending key order.
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

impl<K, V> IntoIterator for FingerprintTreeMap<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    /// Consumes the tree, yielding `(K, V)` in ascending key order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rsos::FingerprintTreeMap;
    /// let tree = FingerprintTreeMap::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.into_iter().collect();
    /// assert_eq!(pairs, vec![(1, "a"), (2, "b")]);
    /// ```
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: vec![IntoIterLayer::Node(self.root)],
        }
    }
}

/// Yields `(&K, &V)` in ascending key order.
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

impl<'a, K, V> IntoIterator for &'a FingerprintTreeMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        Iter {
            stack: vec![(&self.root, 0)],
        }
    }
}

impl<K, V> FingerprintTreeMap<K, V> {
    /// Yields `(&K, &V)` in ascending key order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rsos::FingerprintTreeMap;
    /// let tree = FingerprintTreeMap::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.iter().collect();
    /// assert_eq!(pairs, vec![(&1, &"a"), (&2, &"b")]);
    /// ```
    pub fn iter(&self) -> Iter<'_, K, V> {
        self.into_iter()
    }
}

/// A per-node frame of the [`IterMut`] traversal stack.
#[cfg(test)]
struct Frame<'a, K, V> {
    kv: std::iter::Zip<std::slice::Iter<'a, K>, std::slice::IterMut<'a, V>>,
    children: Option<std::slice::IterMut<'a, Box<Node<K, V>>>>,
}

/// Yields `(&K, &mut V)` in ascending key order.
///
/// Mutation through it leaves stale fingerprints; use [`FingerprintTreeMap::with_mut`].
#[cfg(test)]
struct IterMut<'a, K, V> {
    stack: Vec<Frame<'a, K, V>>,
}

#[cfg(test)]
impl<'a, K, V> IterMut<'a, K, V> {
    /// Pushes `node` and the leftmost path beneath it, so the top frame yields in-order first.
    fn push_left_path(stack: &mut Vec<Frame<'a, K, V>>, mut node: &'a mut Node<K, V>) {
        loop {
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
impl<'a, K: 'a + Serialize + Ord, V: Serialize> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            if let Some((k, v)) = frame.kv.next() {
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
            self.stack.pop();
        }
    }
}

#[cfg(test)]
impl<'a, K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Yields `(&K, &mut V)` in ascending key order; leaves fingerprints stale.
    fn iter_mut(&'a mut self) -> IterMut<'a, K, V> {
        let mut stack = Vec::new();
        IterMut::push_left_path(&mut stack, &mut self.root);
        IterMut { stack }
    }
}

/// Consumes the tree, yielding `V` in ascending key order.
pub struct IntoValues<K, V> {
    inner: IntoIter<K, V>,
}

impl<K, V> Iterator for IntoValues<K, V> {
    type Item = V;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

impl<K, V> FingerprintTreeMap<K, V> {
    /// Consumes the tree, yielding `V` in ascending key order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rsos::FingerprintTreeMap;
    /// let tree = FingerprintTreeMap::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.into_values().collect();
    /// assert_eq!(pairs, vec![("a"), ("b")]);
    /// ```
    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues {
            inner: self.into_iter(),
        }
    }
}

/// Yields `&V` in ascending key order.
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

impl<K, V> FingerprintTreeMap<K, V> {
    /// Yields `&V` in ascending key order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rsos::FingerprintTreeMap;
    /// let tree = FingerprintTreeMap::from_iter(vec![(1, "a"), (2, "b")]);
    /// let pairs: Vec<_> = tree.values().collect();
    /// assert_eq!(pairs, vec![(&"a"), (&"b")]);
    /// ```
    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }
}

/// Yields `&mut V` in ascending key order; leaves fingerprints stale.
#[cfg(test)]
struct ValuesMut<'a, K, V> {
    inner: IterMut<'a, K, V>,
}

#[cfg(test)]
impl<'a, K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Yields `&mut V` in ascending key order; leaves fingerprints stale.
    fn values_mut(&'a mut self) -> ValuesMut<'a, K, V> {
        ValuesMut {
            inner: self.iter_mut(),
        }
    }
}

#[cfg(test)]
impl<'a, K: 'a + Serialize + Ord, V: Serialize> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
}

/// Consumes the tree, yielding `K` in ascending key order.
pub struct IntoKeys<K, V> {
    inner: IntoIter<K, V>,
}

impl<K, V> Iterator for IntoKeys<K, V> {
    type Item = K;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
}

impl<K, V> FingerprintTreeMap<K, V> {
    /// Consumes the tree, yielding `K` in ascending key order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rsos::FingerprintTreeMap;
    /// let tree = FingerprintTreeMap::from_iter(vec![(1, 'a'), (2, 'b')]);
    /// let ks: Vec<_> = tree.clone().into_keys().collect();
    /// assert_eq!(ks, vec![1, 2]);
    /// ```
    pub fn into_keys(self) -> IntoKeys<K, V> {
        IntoKeys {
            inner: self.into_iter(),
        }
    }
}

/// Yields `&K` in ascending key order.
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
}

impl<K, V> FingerprintTreeMap<K, V> {
    /// Yields `&K` in ascending key order.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use rsos::FingerprintTreeMap;
    /// let tree = FingerprintTreeMap::from_iter(vec![(1, 'a'), (2, 'b')]);
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

    use super::FingerprintTreeMap;
    use once_cell::sync::Lazy;

    const TREE_SIZE: usize = 1000;

    static BASE_ITEMS: Lazy<Vec<(u64, u64)>> = Lazy::new(|| {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        (0..TREE_SIZE)
            .map(|i| (i as u64, rng.gen::<u64>()))
            .collect()
    });

    fn make_tree() -> FingerprintTreeMap<u64, u64> {
        FingerprintTreeMap::from_iter(BASE_ITEMS.clone())
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
        let collected: Vec<_> = tree.iter().map(|(_, &v)| v).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn test_with_mut_maintains_invariants() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(99);
        let mut tree = make_tree();
        tree.check_invariants();

        let aggregate_before = tree.aggregate(..);

        for _ in 0..20 {
            let idx = rng.gen_range(0..TREE_SIZE);
            let (key, _) = BASE_ITEMS[idx];
            let new_value: u64 = rng.gen();
            tree.with_mut(&key, |v| *v.unwrap() = new_value);
            tree.check_invariants();
        }

        // `with_mut` overwrites in place: the fingerprint moves, the count must not.
        let aggregate_after = tree.aggregate(..);
        assert_ne!(
            aggregate_before.fingerprint(),
            aggregate_after.fingerprint(),
            "tree fingerprint unchanged after with_mut mutations — fingerprints not updated"
        );
        assert_eq!(
            aggregate_before.size(),
            aggregate_after.size(),
            "with_mut changed the element count"
        );

        let mid = BASE_ITEMS[TREE_SIZE / 2].0;
        assert_eq!(
            tree.aggregate(..mid) + tree.aggregate(mid..),
            tree.aggregate(..),
            "partial-range aggregates do not compose into the global aggregate"
        );
        assert_eq!(tree.aggregate(..), aggregate_after);
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
        let empty: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
        // immutable
        assert_eq!(empty.iter().next(), None);
        // consuming
        assert!(empty.clone().into_iter().next().is_none());
        assert!(empty.clone().into_keys().next().is_none());
        assert!(empty.clone().into_values().next().is_none());
        // shared
        assert!(empty.keys().next().is_none());
        assert!(empty.values().next().is_none());
        let mut empty_mut = empty.clone();
        assert!(empty_mut.iter_mut().next().is_none());
        assert!(empty_mut.values_mut().next().is_none());
        empty_mut.check_invariants();
    }

    #[test]
    fn test_all_iterators_single_leaf() {
        let mut single = FingerprintTreeMap::new();
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
        single.with_mut(&42, |v| *v.unwrap() += 1);
        single.check_invariants();
        assert_eq!(single.iter().collect::<Vec<_>>(), vec![(&42, &100)]);
        single.with_mut(&42, |v| *v.unwrap() *= 2);
        single.check_invariants();
        assert_eq!(single.values().copied().collect::<Vec<_>>(), vec![200]);
    }
}
