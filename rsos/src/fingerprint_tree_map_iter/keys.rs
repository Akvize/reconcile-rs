// Copyright 2025 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`IntoKeys`]/[`Keys`]: yield `K`/`&K` over a [`FingerprintTreeMap`] in ascending key order, by
//! projecting [`IntoIter`](super::IntoIter)/[`Iter`](super::Iter).

use std::fmt;
use std::iter::FusedIterator;

use crate::fingerprint_tree_map::FingerprintTreeMap;

use super::{IntoKeys, Keys};

impl<K, V> Iterator for IntoKeys<K, V> {
    type Item = K;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IntoKeys<K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for IntoKeys<K, V> {}

impl<K: fmt::Debug + Clone, V: Clone> fmt::Debug for IntoKeys<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
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
    #[must_use]
    pub fn into_keys(self) -> IntoKeys<K, V> {
        IntoKeys {
            inner: self.into_iter(),
        }
    }
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, _)| k)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Keys<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for Keys<'_, K, V> {}

// Hand-written, not `#[derive(Clone)]`: `Iter`'s own `Clone` needs no `K: Clone, V: Clone` bound
// (see there), and deriving here would add one anyway.
impl<K, V> Clone for Keys<'_, K, V> {
    fn clone(&self) -> Self {
        Keys {
            inner: self.inner.clone(),
        }
    }
}

impl<K: fmt::Debug, V> fmt::Debug for Keys<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
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
    #[must_use]
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { inner: self.iter() }
    }
}
