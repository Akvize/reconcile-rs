// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This [`module`](crate::hrtree) provides
//! a [`Hash Range Tree`](HRTree), a key-value store
//! based on a BTree structure.
//!
//! It is the core of the protocol, as it allows `O(log(n))` access,
//! insertion and removal, as well as `O(log(n))` cumulated hash range-query.
//! The latter property enables querying
//! the cumulated (XORed) hash of all key-value pairs between two keys.

//! Although we did come we the idea independently, it exactly matches a paper
//! published on Arxiv in February 2023:
//! [Range-Based Set Reconciliation](https://arxiv.org/abs/2212.13567), by Aljoscha Meyer
//!
//! [`HRTree`] implements the [`Diffable`](crate::diff::Diffable)
//! and [`HashRangeQueryable`] traits.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::{Bound, RangeBounds};

use arrayvec::ArrayVec;
use range_cmp::{RangeComparable, RangeOrdering};
use tracing::trace;

use crate::diff::HashRangeQueryable;

pub fn hash<K: Hash, V: Hash>(key: &K, value: &V) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

const B: usize = 6;
const MIN_CAPACITY: usize = B - 1;
const MAX_CAPACITY: usize = 2 * B - 1;

type InsertionTuple<K, V> = Option<(K, V, u64, Box<Node<K, V>>)>;

#[derive(Debug, Default)]
struct Node<K, V> {
    keys: ArrayVec<K, MAX_CAPACITY>,
    values: ArrayVec<V, MAX_CAPACITY>,
    hashes: ArrayVec<u64, MAX_CAPACITY>,
    children: Option<ArrayVec<Box<Node<K, V>>, { MAX_CAPACITY + 1 }>>,
    tree_hash: u64,
    tree_size: usize,
}

impl<K, V> Node<K, V> {
    fn new() -> Self {
        Node {
            keys: ArrayVec::new(),
            values: ArrayVec::new(),
            hashes: ArrayVec::new(),
            children: None,
            tree_hash: 0,
            tree_size: 0,
        }
    }

    fn refresh_hash_size(&mut self) {
        let mut cum_hash = 0;
        for hash in self.hashes.iter() {
            cum_hash ^= hash;
        }
        let mut tot_size = self.keys.len();
        if let Some(children) = self.children.as_ref() {
            for child in children {
                cum_hash ^= child.tree_hash;
                tot_size += child.tree_size;
            }
        }
        self.tree_hash = cum_hash;
        self.tree_size = tot_size;
    }

    fn insert(
        &mut self,
        index: usize,
        key: K,
        value: V,
        hash: u64,
        right_child: Option<Box<Node<K, V>>>,
        diff_hash: u64,
    ) -> InsertionTuple<K, V> {
        assert_eq!(self.children.is_none(), right_child.is_none());
        if self.keys.is_full() {
            // TODO: handle case where self.keys.len() == 2 without leaving empty node
            let mid = self.keys.len() / 2;
            // split
            let mut right_sibling = Box::new(Node {
                keys: ArrayVec::from_iter(self.keys.drain(mid + 1..)),
                values: ArrayVec::from_iter(self.values.drain(mid + 1..)),
                hashes: ArrayVec::from_iter(self.hashes.drain(mid + 1..)),
                children: self
                    .children
                    .as_mut()
                    .map(|children| ArrayVec::from_iter(children.drain(mid + 1..))),
                tree_hash: 0,
                tree_size: 0,
            });
            let mid_key = self.keys.pop().unwrap();
            let mid_value = self.values.pop().unwrap();
            let mid_hash = self.hashes.pop().unwrap();
            // do the insert
            let to_insert = if index <= mid {
                self.insert(index, key, value, hash, right_child, diff_hash)
            } else {
                right_sibling.insert(index - mid - 1, key, value, hash, right_child, diff_hash)
            };
            assert!(to_insert.is_none());
            assert!(!self.keys.is_empty());
            assert!(!right_sibling.keys.is_empty());
            // update invariants
            self.refresh_hash_size();
            right_sibling.refresh_hash_size();
            Some((mid_key, mid_value, mid_hash, right_sibling))
        } else {
            // just insert
            self.keys.insert(index, key);
            self.values.insert(index, value);
            self.hashes.insert(index, hash);
            self.tree_size += 1;
            self.tree_hash ^= diff_hash;
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

    fn rebalance_after_deletion(&mut self, index: usize) {
        let children = self.children.as_mut().unwrap();
        if children[index].keys.len() >= MIN_CAPACITY {
            // nothing to do
            return;
        }
        // need to restore minimum node size invariant
        if index > 0 && children[index - 1].keys.len() > MIN_CAPACITY {
            // steal left, rotate right
            // take last separator (k, v, h) from left sibling
            let left_sibling = children[index - 1].as_mut();
            let k = left_sibling.keys.pop().unwrap();
            let v = left_sibling.values.pop().unwrap();
            let h = left_sibling.hashes.pop().unwrap();
            left_sibling.tree_size -= 1;
            left_sibling.tree_hash ^= h;
            // take last child from left sibling if any
            let c = left_sibling.children.as_mut().map(|children| {
                let c = children.pop().unwrap();
                left_sibling.tree_size -= c.tree_size;
                left_sibling.tree_hash ^= c.tree_hash;
                c
            });
            // NOTE: separator (k, v, h) is left of child c
            // exchange sibling's separator with parent's separator
            let k = std::mem::replace(&mut self.keys[index - 1], k);
            let v = std::mem::replace(&mut self.values[index - 1], v);
            let h = std::mem::replace(&mut self.hashes[index - 1], h);
            // NOTE: separator (k, v, h) is now right of child c
            // move separator (k, v, h) in current node
            let current = children[index].as_mut();
            current.keys.insert(0, k);
            current.values.insert(0, v);
            current.hashes.insert(0, h);
            current.tree_size += 1;
            current.tree_hash ^= h;
            // move child c in current node if any
            if let Some(c) = c {
                current.tree_size += c.tree_size;
                current.tree_hash ^= c.tree_hash;
                current.children.as_mut().unwrap().insert(0, c);
            }
        } else if index + 1 < children.len() && children[index + 1].keys.len() > MIN_CAPACITY {
            // steal right, rotate left
            // take first separator (k, v, h) from right sibling
            let right_sibling = children[index + 1].as_mut();
            let k = right_sibling.keys.remove(0);
            let v = right_sibling.values.remove(0);
            let h = right_sibling.hashes.remove(0);
            right_sibling.tree_size -= 1;
            right_sibling.tree_hash ^= h;
            // take first child from right sibling if any
            let c = right_sibling.children.as_mut().map(|children| {
                let c = children.remove(0);
                right_sibling.tree_size -= c.tree_size;
                right_sibling.tree_hash ^= c.tree_hash;
                c
            });
            // NOTE: separator (k, v, h) is right of child c
            // exchange (k, v, h) with separator
            let k = std::mem::replace(&mut self.keys[index], k);
            let v = std::mem::replace(&mut self.values[index], v);
            let h = std::mem::replace(&mut self.hashes[index], h);
            // NOTE: separator (k, v, h) is now left of child c
            // move separator (k, v, h) in current node
            let current = children[index].as_mut();
            current.keys.push(k);
            current.values.push(v);
            current.hashes.push(h);
            current.tree_size += 1;
            current.tree_hash ^= h;
            // move child c in current node if any
            if let Some(c) = c {
                current.tree_size += c.tree_size;
                current.tree_hash ^= c.tree_hash;
                current.children.as_mut().unwrap().push(c);
            }
        } else {
            let merge_into = if index > 0 {
                index - 1
            } else if index + 1 < children.len() {
                index
            } else {
                // root node, nothing to do
                return;
            };

            // merge right sibling in the current node
            let right_sibling = children.remove(merge_into + 1);
            let current = children[merge_into].as_mut();
            // move separator in current node
            let k = self.keys.remove(merge_into);
            let v = self.values.remove(merge_into);
            let h = self.hashes.remove(merge_into);
            current.keys.push(k);
            current.values.push(v);
            current.hashes.push(h);
            current.tree_size += 1;
            current.tree_hash ^= h;
            // move values of right_sibling in current node
            for k in right_sibling.keys {
                current.keys.push(k);
            }
            for v in right_sibling.values {
                current.values.push(v);
            }
            for h in right_sibling.hashes {
                current.hashes.push(h);
            }
            if let Some(child_children) = current.children.as_mut() {
                for c in right_sibling.children.unwrap() {
                    child_children.push(c);
                }
            }
            current.tree_size += right_sibling.tree_size;
            current.tree_hash ^= right_sibling.tree_hash;
        }
    }
}

pub struct HRTree<K, V> {
    root: Box<Node<K, V>>,
}

impl<K, V> Default for HRTree<K, V> {
    fn default() -> Self {
        HRTree {
            root: Box::new(Node::new()),
        }
    }
}

impl<K: Hash + Ord, V: Hash> HRTree<K, V> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn get<'a>(&'a self, key: &K) -> Option<&'a V> {
        fn aux<'a, K: Ord, V>(node: &'a Node<K, V>, key: &K) -> Option<&'a V> {
            match node.keys.binary_search(key) {
                Ok(index) => Some(&node.values[index]),
                Err(index) => {
                    if let Some(children) = node.children.as_ref() {
                        aux(children[index].as_ref(), key)
                    } else {
                        None
                    }
                }
            }
        }
        aux(self.root.as_ref(), key)
    }

    pub fn position(&self, key: &K) -> Option<usize> {
        fn aux<K: Ord, V>(node: &Node<K, V>, key: &K) -> Option<usize> {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = key.cmp(&node.keys[i]);
                    if cmp == Ordering::Less {
                        // recurse left to key
                        return aux(&children[i], key).map(|offset| index + offset);
                    }
                    // pass sub-tree
                    index += children[i].tree_size;
                    if cmp == Ordering::Equal {
                        // found key
                        return Some(index);
                    }
                    // pass node
                    index += 1;
                }
                aux(children.last().unwrap().as_ref(), key).map(|offset| index + offset)
            } else {
                node.keys.binary_search(key).ok()
            }
        }
        aux(self.root.as_ref(), key)
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        // return:
        // - a key and node to be inserted after the current node
        // - the hash difference
        // - the value that was at key, if any
        fn aux<K: Hash + Ord, V: Hash>(
            node: &mut Node<K, V>,
            key: K,
            value: V,
        ) -> (InsertionTuple<K, V>, u64, Option<V>) {
            match node.keys.binary_search(&key) {
                Ok(index) => {
                    let old_hash = node.hashes[index];
                    let new_hash = hash(&key, &value);
                    let diff_hash = old_hash ^ new_hash;
                    node.hashes[index] = new_hash;
                    node.tree_hash ^= diff_hash;
                    let ret = std::mem::replace(&mut node.values[index], value);
                    (None, diff_hash, Some(ret))
                }
                Err(index) => {
                    if let Some(children) = node.children.as_mut() {
                        // internal node
                        let (mut to_insert, diff_hash, ret) = aux(&mut children[index], key, value);
                        if let Some((key, value, hash, right_child)) = to_insert {
                            to_insert =
                                node.insert(index, key, value, hash, Some(right_child), diff_hash)
                        } else {
                            if ret.is_none() {
                                node.tree_size += 1;
                            }
                            node.tree_hash ^= diff_hash;
                        }
                        (to_insert, diff_hash, ret)
                    } else {
                        // leaf
                        let hash = hash(&key, &value);
                        let to_insert = node.insert(index, key, value, hash, None, hash);
                        (to_insert, hash, None)
                    }
                }
            }
        }
        let (to_insert, _, ret) = aux(&mut self.root, key, value);
        // if we still have things to insert at the root, we need to create a new root
        if let Some((key, value, hash, right_child)) = to_insert {
            let new_root = Box::new(Node::new());
            let old_root = std::mem::replace(&mut self.root, new_root);
            let mut children = ArrayVec::new();
            children.push(old_root);
            children.push(right_child);
            self.root.keys.push(key);
            self.root.values.push(value);
            self.root.hashes.push(hash);
            self.root.children = Some(children);
            self.root.refresh_hash_size();
        }
        trace!(
            "Updated state after insertion; global hash is now {}",
            self.root.tree_hash
        );
        ret
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        fn rightmost_child<K, V>(node: &mut Node<K, V>) -> (K, V, u64) {
            if let Some(children) = node.children.as_mut() {
                let (k, v, h) = rightmost_child(children.last_mut().unwrap());
                node.tree_size -= 1;
                node.tree_hash ^= h;
                node.rebalance_after_deletion(node.keys.len());
                (k, v, h)
            } else {
                let k = node.keys.pop().unwrap();
                let v = node.values.pop().unwrap();
                let h = node.hashes.pop().unwrap();
                node.tree_size -= 1;
                node.tree_hash ^= h;
                (k, v, h)
            }
        }
        // return:
        // - the hash diff
        // - the value at the key that was removed, if there was one
        fn aux<K: Ord, V>(node: &mut Node<K, V>, key: &K) -> (u64, Option<V>) {
            match node.keys.binary_search(key) {
                Ok(index) => {
                    if let Some(children) = node.children.as_mut() {
                        // internal node
                        // we need to replace key, value hash with a new separator; we can find it
                        // in the left or right sub-tree
                        let (prev_k, prev_v, prev_h) = rightmost_child(&mut children[index]);
                        node.keys[index] = prev_k;
                        let v = std::mem::replace(&mut node.values[index], prev_v);
                        let h = std::mem::replace(&mut node.hashes[index], prev_h);
                        node.tree_size -= 1;
                        node.tree_hash ^= h;
                        node.rebalance_after_deletion(index);
                        (h, Some(v))
                    } else {
                        // leaf node
                        node.keys.remove(index);
                        let v = node.values.remove(index);
                        let h = node.hashes.remove(index);
                        node.tree_size -= 1;
                        node.tree_hash ^= h;
                        (h, Some(v))
                    }
                }
                Err(index) => {
                    if let Some(children) = node.children.as_mut() {
                        // internal node
                        let (diff_hash, ret) = aux(&mut children[index], key);
                        if ret.is_some() {
                            node.tree_size -= 1;
                        }
                        node.tree_hash ^= diff_hash;
                        node.rebalance_after_deletion(index);
                        (diff_hash, ret)
                    } else {
                        // leaf node
                        (0, None)
                    }
                }
            }
        }
        let ret = aux(&mut self.root, key).1;
        trace!(
            "Updated state after removal; global hash is now {}",
            self.root.tree_hash
        );
        ret
    }

    pub fn check_invariants(&self) {
        // return:
        // - the cumulated hash of the sub-tree
        // - the number of nodes of the sub-tree
        // - the height of the sub-tree
        fn aux<'a, K: Hash + Ord, V: Hash>(
            node: &'a Node<K, V>,
            mut min: Option<&'a K>,
            max: Option<&K>,
        ) -> (u64, usize, usize) {
            let mut cum_hash = 0;
            let mut tot_size = 0;
            let mut max_height = 1;
            // check node size
            if min.is_some() || max.is_some() {
                // this is not the root
                assert!(
                    node.keys.len() >= MIN_CAPACITY,
                    "minimum node size invariant violated"
                );
            }
            // check order
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
                    let (child_hash, child_size, child_height) = aux(&children[i], min, next_max);
                    cum_hash ^= child_hash;
                    tot_size += child_size;
                    if max_height != 1 {
                        assert_eq!(child_height, max_height, "height invariant violated");
                    }
                    max_height = child_height;
                    min = next_max;
                }
                // key
                let hash = hash(&node.keys[i], &node.values[i]);
                assert_eq!(hash, node.hashes[i], "hash cache invalid");
                cum_hash ^= hash;
                tot_size += 1;
            }
            // child after last key
            if let Some(children) = node.children.as_ref() {
                let (child_hash, child_size, child_height) =
                    aux(children.last().unwrap(), min, max);
                cum_hash ^= child_hash;
                tot_size += child_size;
                if max_height != 1 {
                    assert_eq!(child_height, max_height, "height invariant violated");
                }
            }
            assert_eq!(cum_hash, node.tree_hash, "hash invariant violated");
            assert_eq!(tot_size, node.tree_size, "size invariant violated");
            (cum_hash, tot_size, max_height + 1)
        }
        aux(&self.root, None, None);
    }
}

impl<K, V> PartialEq for HRTree<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.root.tree_hash == other.root.tree_hash
    }
}

impl<K, V> Eq for HRTree<K, V> {}

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

enum IntoIterItem<K, V> {
    Node(Box<Node<K, V>>),
    Element(K, V),
}

pub struct IntoIter<K, V> {
    stack: Vec<IntoIterItem<K, V>>,
}

impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);
    fn next(&mut self) -> Option<Self::Item> {
        match self.stack.pop() {
            Some(IntoIterItem::Node(mut node)) => {
                if let Some(mut children) = node.children {
                    self.stack.push(IntoIterItem::Node(children.pop().unwrap()));
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterItem::Element(k, v));
                        let c = children.pop().unwrap();
                        self.stack.push(IntoIterItem::Node(c));
                    }
                } else {
                    while !node.keys.is_empty() {
                        let k = node.keys.pop().unwrap();
                        let v = node.values.pop().unwrap();
                        self.stack.push(IntoIterItem::Element(k, v));
                    }
                }
                self.next()
            }
            Some(IntoIterItem::Element(k, v)) => Some((k, v)),
            None => None,
        }
    }
}

impl<K, V> IntoIterator for HRTree<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            stack: vec![IntoIterItem::Node(self.root)],
        }
    }
}

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
            if children_passed <= node.keys.len() {
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
    pub fn iter(&self) -> Iter<K, V> {
        self.into_iter()
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for HRTree<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Hash + Ord, V: Hash> HashRangeQueryable for HRTree<K, V> {
    type Key = K;
    fn hash<R: RangeBounds<K>>(&self, range: &R) -> u64 {
        fn aux<'a, K: Ord, V, R: RangeBounds<K>>(
            node: &'a Node<K, V>,
            range: &R,
            mut lower_bound: Option<&'a K>,
            upper_bound: Option<&K>,
        ) -> u64 {
            // check if the lower-bound is included in the range
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
            // check if the upper-bound is included in the range
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
            // if both lower and upper bounds are included in the range, just use the tree hash invariant
            if lower_bound_included && upper_bound_included {
                return node.tree_hash;
            }
            // otherwise, recurse in the relevant sub-trees

            let mut cum_hash = 0;
            let mut i = 0;
            while i < node.keys.len() && node.keys[i].range_cmp(range) == RangeOrdering::Below {
                i += 1;
            }
            while i < node.keys.len() && node.keys[i].range_cmp(range) == RangeOrdering::Inside {
                let cur_bound = Some(&node.keys[i]);
                if let Some(children) = node.children.as_ref() {
                    cum_hash ^= aux(&children[i], range, lower_bound, cur_bound);
                }
                cum_hash ^= node.hashes[i];
                lower_bound = cur_bound;
                i += 1;
            }
            if let Some(children) = node.children.as_ref() {
                cum_hash ^= aux(&children[i], range, lower_bound, upper_bound);
            }
            cum_hash
        }
        aux(&self.root, range, None, None)
    }

    fn insertion_position(&self, key: &K) -> usize {
        fn aux<K: Ord, V>(node: &Node<K, V>, key: &K) -> usize {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = key.cmp(&node.keys[i]);
                    if cmp == Ordering::Less {
                        // recurse left to key
                        return index + aux(&children[i], key);
                    }
                    // pass sub-tree
                    index += children[i].tree_size;
                    if cmp == Ordering::Equal {
                        // found key
                        return index;
                    }
                    // pass node
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

    fn key_at(&self, index: usize) -> &K {
        fn aux<K: Ord, V>(node: &Node<K, V>, mut index: usize) -> &K {
            if let Some(children) = node.children.as_ref() {
                for i in 0..node.keys.len() {
                    if index < children[i].tree_size {
                        // recurse
                        return aux(&children[i], index);
                    }
                    // pass sub-tree
                    index -= children[i].tree_size;
                    // check node
                    if index == 0 {
                        return &node.keys[i];
                    }
                    // pass node
                    index -= 1;
                }
                aux(children.last().unwrap(), index)
            } else {
                &node.keys[index]
            }
        }
        aux(&self.root, index)
    }

    fn len(&self) -> usize {
        self.root.tree_size
    }
}

pub struct ItemRange<'a, K, V, R: RangeBounds<K>> {
    range: &'a R,
    stack: Vec<(&'a Node<K, V>, usize)>,
}

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
}

impl<K: Ord, V> HRTree<K, V> {
    pub fn get_range<'a, R: RangeBounds<K>>(&'a self, range: &'a R) -> ItemRange<'a, K, V, R> {
        let mut stack = Vec::new();
        let mut node = self.root.as_ref();
        // traverse interior nodes
        'main_loop: while let Some(children) = node.children.as_ref() {
            for i in 0..node.keys.len() {
                match node.keys[i].range_cmp(range) {
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
                }
            }
            node = children.last().as_ref().unwrap();
        }
        // traverse leaf node
        for i in 0..node.keys.len() {
            match node.keys[i].range_cmp(range) {
                RangeOrdering::Below => (),
                RangeOrdering::Above => {
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
}

#[cfg(test)]
mod tests {
    use std::ops::RangeBounds;

    use rand::{seq::SliceRandom, Rng, SeedableRng};

    use crate::diff::{Diffable, HashRangeQueryable};

    use super::HRTree;

    #[test]
    fn test_simple() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut tree: HRTree<u64, u64> = HRTree::new();
        for _ in 1..=100 {
            tree.insert(rng.gen(), rng.gen());
            tree.check_invariants();
        }
    }

    #[test]
    fn test_hash() {
        // empty
        let mut tree = HRTree::new();
        assert_eq!(tree.hash(&..), 0);
        tree.check_invariants();

        // 1 value
        tree.insert(50, "Hello");
        tree.check_invariants();
        let hash1 = tree.hash(&..);
        assert_ne!(hash1, 0);

        // 2 values
        tree.insert(25, "World!");
        tree.check_invariants();
        let hash2 = tree.hash(&..);
        assert_ne!(hash2, 0);
        assert_ne!(hash2, hash1);

        // 3 values
        tree.insert(75, "Everyone!");
        tree.check_invariants();
        let hash3 = tree.hash(&..);
        assert_ne!(hash3, 0);
        assert_ne!(hash3, hash1);
        assert_ne!(hash3, hash2);

        // back to 2 values
        tree.remove(&75);
        tree.check_invariants();
        let hash4 = tree.hash(&..);
        assert_eq!(hash4, hash2);
    }

    #[test]
    fn big_test() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut tree1 = HRTree::new();
        let mut key_values = Vec::new();

        let mut expected_hash = 0;

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen::<u64>();
            let value: u64 = rng.gen();
            let old = tree1.insert(key, value);
            assert!(old.is_none());
            tree1.check_invariants();
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(tree1.hash(&..), expected_hash);
            key_values.push((key, value));
        }

        // in the tree, the items should now be sorted
        key_values.sort();

        let mut tree2 = HRTree::from_iter(key_values.iter().copied());
        assert_eq!(tree1, tree2);

        // check for partial ranges
        let mid = key_values[key_values.len() / 2].0;
        assert_ne!(tree1.hash(&(mid..)), tree1.hash(&..));
        assert_ne!(tree1.hash(&..mid), tree1.hash(&..));
        assert_eq!(tree1.hash(&..mid) ^ tree1.hash(&(mid..)), tree1.hash(&..));

        for _ in 0..100 {
            let index = rng.gen::<usize>() % key_values.len();
            let key = key_values[index].0;
            assert_eq!(*tree1.key_at(index), key);
            assert_eq!(tree1.position(&key), Some(index));
            assert_eq!(tree1.insertion_position(&key), index);
        }
        assert_eq!(tree1.insertion_position(&0), 0);
        assert_eq!(tree1.insertion_position(&u64::MAX), tree1.len());

        // test iteration
        let items: Vec<(u64, u64)> = tree1.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(items.len(), key_values.len());
        assert_eq!(items, key_values);

        // test get_range
        let from_index = rng.gen_range(0..key_values.len());
        let to_index = rng.gen_range(from_index..key_values.len());
        let from_key = tree1.key_at(from_index);
        let to_key = tree1.key_at(to_index);
        fn test_range<
            R: RangeBounds<u64>,
            SI: std::slice::SliceIndex<[(u64, u64)], Output = [(u64, u64)]>,
        >(
            key_values: &Vec<(u64, u64)>,
            tree: &HRTree<u64, u64>,
            range: R,
            slice_index: SI,
        ) {
            assert_eq!(
                tree.get_range(&range)
                    .map(|(k, v)| (*k, *v))
                    .collect::<Vec<_>>(),
                key_values[slice_index]
            );
        }
        test_range(&key_values, &tree1, from_key..to_key, from_index..to_index);
        test_range(
            &key_values,
            &tree1,
            from_key..=to_key,
            from_index..=to_index,
        );
        test_range(&key_values, &tree1, ..to_key, ..to_index);
        test_range(&key_values, &tree1, ..=to_key, ..=to_index);
        test_range(&key_values, &tree1, from_key.., from_index..);
        test_range(&key_values, &tree1, .., ..);

        // test diff
        let key: u64 = rng.gen::<u64>();
        let value: u64 = rng.gen();
        let old = tree2.insert(key, value);
        assert!(old.is_none());
        let mut diff_ranges1 = Vec::new();
        let mut diff_ranges2 = Vec::new();
        let mut segments1 = tree1.start_diff();
        let mut segments2 = Vec::new();
        while !segments1.is_empty() {
            tree2.diff_round(
                segments1.drain(..).collect(),
                &mut segments2,
                &mut diff_ranges2,
            );
            tree1.diff_round(
                segments2.drain(..).collect(),
                &mut segments1,
                &mut diff_ranges1,
            );
        }
        assert_eq!(diff_ranges1.len(), 0);
        assert_eq!(diff_ranges2.len(), 1);
        let items: Vec<_> = tree2.get_range(&diff_ranges2[0]).collect();
        assert_eq!(items, vec![(&key, &value)]);

        // remove everything one-by-one
        key_values.shuffle(&mut rng);
        for (key, value) in key_values {
            let value2 = tree1.remove(&key);
            tree1.check_invariants();
            assert_eq!(value2, Some(value));
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(tree1.hash(&..), expected_hash);
        }
    }

    #[test]
    fn test_iter() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut key_values = Vec::new();

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen::<u64>();
            let value: u64 = rng.gen();
            key_values.push((key, value));
        }
        let tree = HRTree::from_iter(key_values.clone());
        key_values.sort();
        assert_eq!(
            tree.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>(),
            key_values
        );
        assert_eq!(tree.into_iter().collect::<Vec<_>>(), key_values);
    }
}
