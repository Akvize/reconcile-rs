// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`IntoIter`]: consumes a [`FingerprintTreeMap`], yielding `(K, V)` in ascending key order.

use std::fmt;
use std::iter::FusedIterator;

use serde::Serialize;

use crate::fingerprint_tree_map::FingerprintTreeMap;

use super::{IntoIter, IntoIterLayer};

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

impl<K, V> IntoIter<K, V> {
    /// One pop-and-possibly-expand step; recurses past `Node` frames without touching
    /// `remaining`, which `next()` adjusts exactly once per yielded element.
    fn advance(&mut self) -> Option<(K, V)> {
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
                self.advance()
            }
            Some(IntoIterLayer::Element(k, v)) => Some((k, v)),
            None => None,
        }
    }
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        let item = self.advance();
        if item.is_some() {
            self.remaining -= 1;
        }
        item
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for IntoIter<K, V> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl<K, V> FusedIterator for IntoIter<K, V> {}

impl<K: fmt::Debug + Clone, V: fmt::Debug + Clone> fmt::Debug for IntoIter<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
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
        let remaining = self.root.subtree_size();
        IntoIter {
            stack: vec![IntoIterLayer::Node(self.root)],
            remaining,
        }
    }
}
