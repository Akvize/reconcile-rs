// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::borrow::Borrow;
use std::cmp::Ordering;

use serde::Serialize;

use crate::aggregate::Aggregate;
use crate::fingerprint::{lift, Fingerprint};

use super::node::Node;
use super::FingerprintTreeMap;

/// The route from the root to the node holding a key: the child index taken at each interior node,
/// then the key's own index inside the node where the key was found.
struct KeyPath {
    descent: Vec<usize>,
    key_index: usize,
}

/// Restores the aggregate invariants for one in-place value mutation, **on drop** — so a
/// panicking callback still leaves every cached [`Aggregate`] consistent with what is stored.
struct Relift<'a, K: Serialize, V: Serialize> {
    root: &'a mut Node<K, V>,
    path: KeyPath,
    key: &'a K,
}

impl<K: Serialize, V: Serialize> Relift<'_, K, V> {
    /// The value this guard will re-lift, reached by following the recorded route.
    fn value_mut(&mut self) -> &mut V {
        let mut node = &mut *self.root;
        for &index in &self.path.descent {
            node = node.children.as_mut().expect("interior node on the route")[index].as_mut();
        }
        &mut node.values[self.path.key_index]
    }
}

impl<K: Serialize, V: Serialize> Drop for Relift<'_, K, V> {
    fn drop(&mut self) {
        /// The signed fingerprint delta this subtree contributed, already applied to its cache.
        fn repair<K: Serialize, V: Serialize>(
            node: &mut Node<K, V>,
            descent: &[usize],
            key_index: usize,
            key: &K,
        ) -> Fingerprint {
            let delta = match descent.split_first() {
                None => {
                    let old_fp = node.fingerprints[key_index];
                    let new_fp = lift(key, &node.values[key_index]);
                    node.fingerprints[key_index] = new_fp;
                    new_fp - old_fp
                }
                Some((&index, rest)) => repair(
                    node.children.as_mut().expect("interior node on the route")[index].as_mut(),
                    rest,
                    key_index,
                    key,
                ),
            };
            node.subtree = Aggregate::new(node.subtree.size(), node.subtree.fingerprint() + delta);
            delta
        }
        repair(self.root, &self.path.descent, self.path.key_index, self.key);
    }
}

impl<K: Ord, V> FingerprintTreeMap<K, V> {
    /// An empty tree. Equivalent to [`Default::default`].
    #[must_use]
    pub fn new() -> Self {
        Default::default()
    }

    /// Returns the value associated with `key`, if present.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        fn aux<'a, K: Borrow<Q>, V, Q: Ord + ?Sized>(
            node: &'a Node<K, V>,
            key: &Q,
        ) -> Option<&'a V> {
            match node.keys.binary_search_by(|probe| probe.borrow().cmp(key)) {
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

    /// Returns whether `key` is present.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.get(key).is_some()
    }
}

impl<K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Calls `callback` with a mutable reference to the value at `key` (`None` if absent), then
    /// re-lifts the element and propagates the fingerprint delta to the root, returning the
    /// callback's own return value.
    ///
    /// The only summary-safe in-place mutation path — no `entry()` handing out a bare `&mut V`
    /// is offered, since it could skip the re-lift.
    ///
    /// # Panic safety
    ///
    /// The repair runs from a [`Drop`] guard, so a panicking `callback` still leaves the cached
    /// aggregates consistent with the stored value; the panic propagates unchanged.
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let mut map = FingerprintTreeMap::new();
    /// map.insert(1, 10);
    /// let before = map.aggregate(..);
    ///
    /// map.with_mut(&1, |v| *v.unwrap() = 99);
    ///
    /// // The stored value moved...
    /// assert_eq!(map.get(&1), Some(&99));
    /// // ...and so did the cached aggregate -- unlike a bare `&mut V`, `with_mut` keeps the
    /// // fingerprint honest without a manual re-lift.
    /// let after = map.aggregate(..);
    /// assert_ne!(before.fingerprint(), after.fingerprint());
    /// assert_eq!(before.size(), after.size());
    /// ```
    pub fn with_mut<R, F: FnOnce(Option<&mut V>) -> R>(&mut self, key: &K, callback: F) -> R {
        let mut descent = Vec::new();
        let mut node = self.root.as_ref();
        let key_index = loop {
            match node.keys.binary_search(key) {
                Ok(index) => break index,
                Err(index) => match node.children.as_ref() {
                    Some(children) => {
                        descent.push(index);
                        node = &children[index];
                    }
                    None => return callback(None),
                },
            }
        };

        let mut guard = Relift {
            root: self.root.as_mut(),
            path: KeyPath { descent, key_index },
            key,
        };
        let value = guard.value_mut();
        callback(Some(value))
    }
}

impl<K: Ord, V> FingerprintTreeMap<K, V> {
    /// Position of `key` in the in-order sequence, or `None` if absent — unlike
    /// [`rank`](FingerprintTreeMap::rank), which returns the insertion point for an absent key.
    ///
    /// ```
    /// use rsos::FingerprintTreeMap;
    ///
    /// let map: FingerprintTreeMap<i32, &str> = [(10, "a"), (20, "b"), (30, "c")].into_iter().collect();
    ///
    /// // A present key reports its own index...
    /// assert_eq!(map.position(&20), Some(1));
    /// // ...while an absent key -- even one that would sort inside the map -- is `None`, unlike
    /// // `rank`, which would still report where it would land.
    /// assert_eq!(map.position(&15), None);
    /// assert_eq!(map.rank(&15), 1);
    /// ```
    pub fn position<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        fn aux<K: Borrow<Q>, V, Q: Ord + ?Sized>(node: &Node<K, V>, key: &Q) -> Option<usize> {
            if let Some(children) = node.children.as_ref() {
                let mut index = 0;
                for i in 0..node.keys.len() {
                    let cmp = node.keys[i].borrow().cmp(key);
                    if cmp == Ordering::Greater {
                        return aux(&children[i], key).map(|offset| index + offset);
                    }
                    index += children[i].subtree.size();
                    if cmp == Ordering::Equal {
                        return Some(index);
                    }
                    index += 1;
                }
                aux(children.last().unwrap().as_ref(), key).map(|offset| index + offset)
            } else {
                node.keys
                    .binary_search_by(|probe| probe.borrow().cmp(key))
                    .ok()
            }
        }
        aux(self.root.as_ref(), key)
    }
}
