// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Rsos`] trait: the formal RSOS (Range-Summarizable Order-Statistics Store) contract,
//! literally Def. 3.9 of E. G. Amparore, *Range-Based Set Reconciliation via Range-Summarizable
//! Order-Statistics Stores* (arXiv:2603.19820). See the crate root docs for the full citation and
//! [`FingerprintTreeMap`](crate::FingerprintTreeMap) for the one realization this crate ships.

use std::hash::Hash;
use std::ops::RangeBounds;

use crate::aggregate::Aggregate;
use crate::fingerprint_tree_map::FingerprintTreeMap;

/// The RSOS interface (Def. 3.9): the seven operations a *replica state* `X ⊆ U` must support to
/// drive range-based set reconciliation — decoupled from any one realization. The paper itself
/// treats RSOS as an abstraction with multiple possible realizations (an in-memory augmented tree
/// and AELMDB, a persistent B-tree, are both realizations in the paper); [`FingerprintTreeMap`] is
/// this crate's one realization, achieving the complexity bounds of Thm. 5.2.
///
/// Method names are the paper's own terms (`size`, `aggregate`, `rank`, `select`, `enumerate`,
/// `insert`, `delete`), not renamed to Rust idiom, so the mapping from spec to code stays legible
/// side by side with the paper. `FingerprintTreeMap` additionally keeps its pre-existing
/// Rust-idiomatic inherent API (`len`, `range`, `is_empty`, ...) for ergonomic direct use —
/// this trait is one more way to call it, not the only way.
///
/// A few signatures adapt the paper's math to idiomatic Rust rather than a literal transliteration
/// (documented per-method below); the *operation* semantics match Def. 3.9 exactly.
///
/// # Why a key parameter and a value type are not a widening of `U`
///
/// Naming both a key type and a stored value type is not a widening of the paper's single element
/// space: **`X` is the set of keys**, so `K` is the paper's `U`, and the value is the payload the
/// Def. 3.4 lift consults. A map assigns exactly one value per key, which is exactly what makes
/// `lift` total on `X`. See the crate root docs for the full argument.
///
/// # Why the stored value is an associated type, not a type parameter
///
/// The value type is [`Rsos::Value`], an *associated* type, so the trait is parameterized by the
/// key type alone. A store realizes RSOS for a given key ordering in exactly one way — there is no
/// meaningful "same store, two different value types" implementation to disambiguate between —
/// which is the textbook criterion for an associated type over a type parameter. It also keeps
/// downstream blanket implementations expressible: `rbsr`'s
/// `impl<K, T: Rsos<K>> RsosView<K> for T` (RBSR needs the key-side operations only, never the
/// values) would not compile against an `Rsos<K, V>` shape, since `V` would appear in neither the
/// implemented trait nor the self type (rustc E0207).
pub trait Rsos<K> {
    /// The type stored against each key. Associated rather than a trait type parameter — see the
    /// trait-level docs.
    type Value;

    /// `size()` → `|X|`: the number of elements currently in the store.
    fn size(&self) -> usize;

    /// `Aggregate(l, u)` → `A(X ∩ [l, u))`: the bundled [`Aggregate`] (Def. 3.5's
    /// `A(S) = (|S|, Σ(S))`, the monoid `(ℕ×M, ⊗, (0, 0_M))`) over a range of keys, in one tree
    /// walk. Concrete [`Aggregate`] over [`Fingerprint`](crate::Fingerprint) rather than a
    /// generic-monoid associated type — see the crate root docs' "lifting monoid" note on why
    /// that generalization is deliberately out of scope today.
    fn aggregate<R: RangeBounds<K>>(&self, range: &R) -> Aggregate;

    /// `Rank(z)` → `Rank_X(z)`: the position `z` occupies (or would occupy) in the in-order
    /// sequence of keys.
    fn rank(&self, z: &K) -> usize;

    /// `Select(r)` → `Select_X(r)`: the key at in-order position `r`. Panics if `r` is out of
    /// bounds (matching `FingerprintTreeMap::select`'s existing panic-on-out-of-bounds contract,
    /// rather than adding an `Option`/`Result` the paper's own signature doesn't have).
    fn select(&self, r: usize) -> &K;

    /// `Enumerate(l, u)` → the ordered contents of `X ∩ [l, u)`. Returns an iterator (over
    /// `(&K, &Self::Value)` pairs) rather than the paper's set-valued return, since Rust has no
    /// built-in notion of returning "a set" other than an iterator/collection — an iterator is the
    /// idiomatic, zero-copy Rust shape for "ordered contents of a range".
    ///
    /// The return type is an anonymous `impl Iterator` (RPITIT, stable since Rust 1.75), not a
    /// named concrete type: this trait is the realization-*independent* RSOS contract, so naming
    /// [`FingerprintTreeMap`]'s own `ItemRange` here would tie the abstraction to one realization's
    /// iterator. The cost is that `Rsos` is not object-safe — deliberate and free today, since
    /// every call site uses a concrete, monomorphized store.
    fn enumerate<'a, R: RangeBounds<K>>(
        &'a self,
        range: &'a R,
    ) -> impl Iterator<Item = (&'a K, &'a Self::Value)> + 'a
    where
        K: Ord + 'a,
        Self::Value: 'a;

    /// `Insert(k, v)`: inserts (or overwrites) `k ↦ v`. Returns the previous value, if any,
    /// matching `FingerprintTreeMap::insert`'s existing return (idiomatic Rust map-insert
    /// semantics, which the paper's own `Insert` does not specify a return for).
    fn insert(&mut self, key: K, value: Self::Value) -> Option<Self::Value>;

    /// `Delete(k)`: removes `k` if present. Returns the removed value, if any, matching
    /// `FingerprintTreeMap::remove`'s existing return — same rationale as `insert`.
    fn delete(&mut self, key: &K) -> Option<Self::Value>;
}

impl<K: Hash + Ord, V: Hash> Rsos<K> for FingerprintTreeMap<K, V> {
    type Value = V;

    fn size(&self) -> usize {
        self.len()
    }

    fn aggregate<R: RangeBounds<K>>(&self, range: &R) -> Aggregate {
        FingerprintTreeMap::aggregate(self, range)
    }

    fn rank(&self, z: &K) -> usize {
        FingerprintTreeMap::rank(self, z)
    }

    fn select(&self, r: usize) -> &K {
        FingerprintTreeMap::select(self, r)
    }

    fn enumerate<'a, R: RangeBounds<K>>(
        &'a self,
        range: &'a R,
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
