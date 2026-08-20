// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cmp::Ordering;
use std::ops::{Bound, RangeBounds};

use range_cmp::{RangeOrd, RangeOrdering};
use serde::Serialize;

use crate::aggregate::Aggregate;

use super::node::Node;
use super::{element, FingerprintTreeMap};

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
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let map: FingerprintTreeMap<i32, &str> = [(10, "a"), (20, "b"), (30, "c")].into_iter().collect();
    ///
    /// // A present key's rank is its in-order index...
    /// assert_eq!(map.rank(&20), 1);
    /// // ...and an absent key still gets the index it would land at if inserted, unlike
    /// // `position`, which is `None` for a key that was never stored.
    /// assert_eq!(map.rank(&15), 1);
    /// assert_eq!(map.rank(&100), map.len());
    /// ```
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
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let map: FingerprintTreeMap<i32, &str> = [(10, "a"), (20, "b"), (30, "c")].into_iter().collect();
    ///
    /// assert_eq!(map.select(1), &20);
    ///
    /// // `select` and `rank` are inverses over a present key: selecting a key's own rank returns
    /// // that key back.
    /// assert_eq!(map.select(map.rank(&30)), &30);
    /// ```
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
