// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`IntoValues`]/[`Values`]: yield `V`/`&V` over a [`FingerprintTreeMap`] in ascending key order,
//! by projecting [`IntoIter`](super::IntoIter)/[`Iter`](super::Iter). `ValuesMut` (`#[cfg(test)]`,
//! not linked here -- the whole `iter_mut` module it wraps is itself `#[cfg(test)]`-only, so a
//! normal `cargo doc` build never sees it) does the same over `super::iter_mut::IterMut`, and is
//! `pub(super)` to match `values_mut`'s visibility -- `tests.rs` (a sibling of this module) calls
//! it directly.

use std::fmt;
use std::iter::FusedIterator;

#[cfg(test)]
use serde::Serialize;

use crate::fingerprint_tree_map::FingerprintTreeMap;

use super::{IntoValues, Values};

impl<K, V> Iterator for IntoValues<K, V> {
    type Item = V;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IntoValues<K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for IntoValues<K, V> {}

impl<K: Clone, V: fmt::Debug + Clone> fmt::Debug for IntoValues<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
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
    #[must_use]
    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues {
            inner: self.into_iter(),
        }
    }
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(_, v)| v)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Values<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for Values<'_, K, V> {}

// Hand-written, not `#[derive(Clone)]`: `Iter`'s own `Clone` needs no `K: Clone, V: Clone` bound
// (see there), and deriving here would add one anyway.
impl<K, V> Clone for Values<'_, K, V> {
    fn clone(&self) -> Self {
        Values {
            inner: self.inner.clone(),
        }
    }
}

impl<K, V: fmt::Debug> fmt::Debug for Values<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
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
    #[must_use]
    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }
}

/// Yields `&mut V` in ascending key order; leaves fingerprints stale.
#[cfg(test)]
pub(super) struct ValuesMut<'a, K, V> {
    inner: super::iter_mut::IterMut<'a, K, V>,
}

#[cfg(test)]
impl<'a, K: Serialize + Ord, V: Serialize> FingerprintTreeMap<K, V> {
    /// Yields `&mut V` in ascending key order; leaves fingerprints stale.
    pub(super) fn values_mut(&'a mut self) -> ValuesMut<'a, K, V> {
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
