// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Rsos`] trait: arXiv:2603.19820 Def. 3.9, realized by
//! [`FingerprintTreeMap`](crate::FingerprintTreeMap).

use std::ops::RangeBounds;

use serde::Serialize;

use crate::aggregate::Aggregate;
use crate::fingerprint_tree_map::FingerprintTreeMap;

/// The RSOS interface (Def. 3.9): the seven operations a replica state `X ⊆ U` must support.
///
/// Method names are the paper's, not Rust idiom; [`FingerprintTreeMap`] keeps a Rust-idiomatic
/// inherent API alongside (crate root docs). `K` is the paper's `U`; the value is the payload
/// Def. 3.4's lift consults, and is an *associated* type so `rbsr`'s
/// `impl<K, T: Rsos<K>> RsosView<K> for T` stays expressible (rustc E0207).
pub trait Rsos<K> {
    /// The type stored against each key.
    type Value;

    /// `size()` → `|X|`: the number of elements currently in the store.
    fn size(&self) -> usize;

    /// `Aggregate(l, u)` → `A(X ∩ [l, u))` in one tree walk.
    fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate;

    /// `Rank(z)` → `Rank_X(z)`: the position `z` occupies (or would occupy) in the in-order
    /// sequence of keys.
    fn rank(&self, z: &K) -> usize;

    /// `Select(r)` → `Select_X(r)`: the key at in-order position `r`.
    ///
    /// # Panics
    ///
    /// If `r >= size()`.
    fn select(&self, r: usize) -> &K;

    /// `Enumerate(l, u)` → the ordered contents of `X ∩ [l, u)`, as `(&K, &Self::Value)` pairs.
    ///
    /// RPITIT rather than a named iterator type, so the contract stays realization-independent;
    /// the cost is that `Rsos` is not object-safe.
    fn enumerate<'a, R: RangeBounds<K> + 'a>(
        &'a self,
        range: R,
    ) -> impl Iterator<Item = (&'a K, &'a Self::Value)> + 'a
    where
        K: Ord + 'a,
        Self::Value: 'a;

    /// `Insert(k, v)`: inserts or overwrites `k ↦ v`, returning the previous value.
    fn insert(&mut self, key: K, value: Self::Value) -> Option<Self::Value>;

    /// `Delete(k)`: removes `k` if present, returning the removed value.
    fn delete(&mut self, key: &K) -> Option<Self::Value>;
}

impl<K: Serialize + Ord, V: Serialize> Rsos<K> for FingerprintTreeMap<K, V> {
    type Value = V;

    fn size(&self) -> usize {
        self.len()
    }

    fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate {
        FingerprintTreeMap::aggregate(self, range)
    }

    fn rank(&self, z: &K) -> usize {
        FingerprintTreeMap::rank(self, z)
    }

    fn select(&self, r: usize) -> &K {
        FingerprintTreeMap::select(self, r)
    }

    fn enumerate<'a, R: RangeBounds<K> + 'a>(
        &'a self,
        range: R,
    ) -> impl Iterator<Item = (&'a K, &'a V)> + 'a
    where
        K: Ord + 'a,
        V: 'a,
    {
        self.range(range)
    }

    fn insert(&mut self, key: K, value: V) -> Option<V> {
        FingerprintTreeMap::insert(self, key, value)
    }

    fn delete(&mut self, key: &K) -> Option<V> {
        self.remove(key)
    }
}
