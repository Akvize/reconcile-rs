// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use arrayvec::ArrayVec;

use crate::aggregate::Aggregate;
use crate::fingerprint::Fingerprint;

use super::{element, without, InsertionTuple, Side, MAX_CAPACITY, MIN_CAPACITY};

#[derive(Clone, Debug, Default)]
pub(crate) struct Node<K, V> {
    pub(crate) keys: ArrayVec<K, MAX_CAPACITY>,
    pub(crate) values: ArrayVec<V, MAX_CAPACITY>,
    pub(super) fingerprints: ArrayVec<Fingerprint, MAX_CAPACITY>,
    pub(crate) children: Option<ArrayVec<Box<Node<K, V>>, { MAX_CAPACITY + 1 }>>,
    /// `A(S)` over this node's whole subtree: its own separators plus everything under
    /// `children`.
    pub(super) subtree: Aggregate,
}

impl<K, V> Node<K, V> {
    pub(super) fn new() -> Self {
        Node {
            keys: ArrayVec::new(),
            values: ArrayVec::new(),
            fingerprints: ArrayVec::new(),
            children: None,
            subtree: Aggregate::ZERO,
        }
    }

    /// `|S|` over this node's whole subtree — `O(1)`, cached in [`subtree`](Node::subtree).
    /// Crate-visible so the iterators in `fingerprint_tree_map_iter` can seed an exact
    /// `remaining` count without an unconstrained-generic `FingerprintTreeMap::len` call.
    pub(crate) fn subtree_size(&self) -> usize {
        self.subtree.size()
    }

    /// Recompute [`subtree`](Node::subtree) by composing own separators with each child's
    /// aggregate.
    pub(super) fn refresh_aggregate(&mut self) {
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

    pub(super) fn insert(
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
            // Safe to split at any `self.keys.len() == MAX_CAPACITY` here: the `B.checked_sub(3)`
            // assertion above rules out the `B == 2` case that would leave an empty sibling node.
            let mid = self.keys.len() / 2;
            let mut right_sibling = Box::new(Node {
                keys: ArrayVec::from_iter(self.keys.drain(mid + 1..)),
                values: ArrayVec::from_iter(self.values.drain(mid + 1..)),
                fingerprints: ArrayVec::from_iter(self.fingerprints.drain(mid + 1..)),
                children: self
                    .children
                    .as_mut()
                    .map(|children| ArrayVec::from_iter(children.drain(mid + 1..))),
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
    /// underflowing child at `index`, restoring minimum occupancy. [`Side`] picks which sibling;
    /// the two cases are mirror images.
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

    pub(super) fn rebalance_after_deletion(&mut self, index: usize) {
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
