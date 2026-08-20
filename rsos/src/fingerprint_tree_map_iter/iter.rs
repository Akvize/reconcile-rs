// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`Iter`]: yields `(&K, &V)` over a [`FingerprintTreeMap`] in ascending key order.

use std::fmt;
use std::iter::FusedIterator;

use crate::fingerprint_tree_map::FingerprintTreeMap;

use super::Iter;

impl<'a, K, V> Iter<'a, K, V> {
    /// One pop-and-possibly-descend step; recurses past frames it has already fully visited
    /// without touching `remaining`, which `next()` adjusts exactly once per yielded element.
    fn advance(&mut self) -> Option<(&'a K, &'a V)> {
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
                self.advance()
            }
        } else {
            None
        }
    }
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);
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

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl<K, V> FusedIterator for Iter<'_, K, V> {}

// Hand-written, not `#[derive(Clone)]`: the stack holds `&'a Node<K, V>`, always `Clone`
// regardless of `K`/`V`, so deriving would add a `K: Clone, V: Clone` bound this impl doesn't
// need.
impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Iter {
            stack: self.stack.clone(),
            remaining: self.remaining,
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Iter<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

impl<'a, K, V> IntoIterator for &'a FingerprintTreeMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        Iter {
            stack: vec![(&self.root, 0)],
            remaining: self.root.subtree_size(),
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
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K, V> {
        self.into_iter()
    }
}
