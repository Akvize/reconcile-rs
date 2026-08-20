// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! In-order iterators over [`FingerprintTreeMap`](crate::fingerprint_tree_map::FingerprintTreeMap):
//! `O(h)` initial descent, amortized `O(1)` per `next()`, `O(h)` stack.
//!
//! `IterMut`/`ValuesMut` are `#[cfg(test)]`-only: they hand out `&mut V` without updating the
//! element fingerprint or the cached subtree aggregate.
//! [`FingerprintTreeMap::with_mut`](crate::fingerprint_tree_map::FingerprintTreeMap::with_mut) is
//! the supported mutation path.
//!
//! `Iter`, `IntoIter`, `Keys`, `Values`, `IntoKeys`, `IntoValues` all implement `ExactSizeIterator`
//! (`len()`/`size_hint()` are `O(1)`, seeded once from the tree's cached subtree size) and
//! `FusedIterator`, plus `Clone` and `Debug`.
//!
//! Split across siblings by iterator family: `into_iter`/`iter`/`iter_mut`/`keys`/`values` each own
//! one traversal's `impl` blocks; this file keeps the public type definitions (their module
//! location is their `cargo public-api`-visible path -- see AGENTS.md §11).

use crate::fingerprint_tree_map::Node;

mod into_iter;
mod iter;
#[cfg(test)]
mod iter_mut;
mod keys;
mod values;

#[derive(Clone, Debug)]
enum IntoIterLayer<K, V> {
    Node(Box<Node<K, V>>),
    Element(K, V),
}

/// Consumes the tree, yielding `(K, V)` in ascending key order.
#[derive(Clone)]
pub struct IntoIter<K, V> {
    stack: Vec<IntoIterLayer<K, V>>,
    remaining: usize,
}

/// Yields `(&K, &V)` in ascending key order.
pub struct Iter<'a, K, V> {
    stack: Vec<(&'a Node<K, V>, usize)>,
    remaining: usize,
}

/// Consumes the tree, yielding `V` in ascending key order.
#[derive(Clone)]
pub struct IntoValues<K, V> {
    inner: IntoIter<K, V>,
}

/// Yields `&V` in ascending key order.
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

/// Consumes the tree, yielding `K` in ascending key order.
#[derive(Clone)]
pub struct IntoKeys<K, V> {
    inner: IntoIter<K, V>,
}

/// Yields `&K` in ascending key order.
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

#[cfg(test)]
mod tests;
