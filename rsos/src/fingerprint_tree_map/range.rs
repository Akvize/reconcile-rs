// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::iter::FusedIterator;
use std::ops::RangeBounds;

use range_cmp::{RangeOrd, RangeOrdering};

use super::{FingerprintTreeMap, ItemRange};

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
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let map: FingerprintTreeMap<i32, &str> = [(30, "c"), (10, "a"), (20, "b")].into_iter().collect();
    ///
    /// // Smallest by key, not insertion order.
    /// assert_eq!(map.first_key_value(), Some((&10, &"a")));
    /// assert_eq!(map.last_key_value(), Some((&30, &"c")));
    /// ```
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
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let empty: FingerprintTreeMap<i32, &str> = FingerprintTreeMap::new();
    /// assert_eq!(empty.last_key_value(), None);
    /// ```
    #[must_use]
    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        let mut node = self.root.as_ref();
        while let Some(children) = node.children.as_ref() {
            node = children.last().unwrap();
        }
        node.keys.last().zip(node.values.last())
    }
}
