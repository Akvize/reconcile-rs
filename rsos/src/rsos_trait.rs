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
//! [`FingerprintTree`](crate::FingerprintTree) for the one realization this crate ships.

use std::hash::Hash;
use std::ops::RangeBounds;

use crate::fingerprint::Fingerprint;
use crate::hrtree::{FingerprintTree, ItemRange};

/// The RSOS interface (Def. 3.9): the seven operations a *replica state* `X ⊆ U` must support to
/// drive range-based set reconciliation — decoupled from any one realization. The paper itself
/// treats RSOS as an abstraction with multiple possible realizations (an in-memory augmented tree
/// and AELMDB, a persistent B-tree, are both realizations in the paper); [`FingerprintTree`] is
/// this crate's one realization, achieving the complexity bounds of Thm. 5.2.
///
/// Method names are the paper's own terms (`size`, `aggregate`, `rank`, `select`, `enumerate`,
/// `insert`, `delete`), not renamed to Rust idiom, so the mapping from spec to code stays legible
/// side by side with the paper. `FingerprintTree` additionally keeps its pre-existing
/// Rust-idiomatic inherent API (`len`, `get_range`, `is_empty`, ...) for ergonomic direct use —
/// this trait is one more way to call it, not the only way.
///
/// A few signatures adapt the paper's math to idiomatic Rust rather than a literal transliteration
/// (documented per-method below); the *operation* semantics match Def. 3.9 exactly.
pub trait Rsos<K, V> {
    /// `size()` → `|X|`: the number of elements currently in the store.
    fn size(&self) -> usize;

    /// `Aggregate(l, u)` → `A(X ∩ [l, u))`: the bundled `(count, Fingerprint)` aggregate (Def.
    /// 3.5's `A(S) = (|S|, Σ(S))`) over a range of keys, in one tree walk. Concrete
    /// `(usize, Fingerprint)` rather than a generic-monoid associated type — see the crate root
    /// docs' "lifting monoid" note on why that generalization is deliberately out of scope today.
    fn aggregate<R: RangeBounds<K>>(&self, range: &R) -> (usize, Fingerprint);

    /// `Rank(z)` → `Rank_X(z)`: the position `z` occupies (or would occupy) in the in-order
    /// sequence of keys.
    fn rank(&self, z: &K) -> usize;

    /// `Select(r)` → `Select_X(r)`: the key at in-order position `r`. Panics if `r` is out of
    /// bounds (matching `FingerprintTree::key_at`'s existing panic-on-out-of-bounds contract,
    /// rather than adding an `Option`/`Result` the paper's own signature doesn't have).
    fn select(&self, r: usize) -> &K;

    /// `Enumerate(l, u)` → the ordered contents of `X ∩ [l, u)`. Returns a concrete iterator type
    /// (over `(&K, &V)` pairs) rather than the paper's set-valued return, since Rust has no
    /// built-in notion of returning "a set" other than an iterator/collection — an iterator is the
    /// idiomatic, zero-copy Rust shape for "ordered contents of a range".
    fn enumerate<'a, R: RangeBounds<K>>(&'a self, range: &'a R) -> ItemRange<'a, K, V, R>
    where
        K: Ord;

    /// `Insert(k, v)`: inserts (or overwrites) `k ↦ v`. Returns the previous value, if any,
    /// matching `FingerprintTree::insert`'s existing return (idiomatic Rust map-insert semantics,
    /// which the paper's own `Insert` does not specify a return for).
    fn insert(&mut self, key: K, value: V) -> Option<V>;

    /// `Delete(k)`: removes `k` if present. Returns the removed value, if any, matching
    /// `FingerprintTree::remove`'s existing return — same rationale as `insert`.
    fn delete(&mut self, key: &K) -> Option<V>;
}

impl<K: Hash + Ord, V: Hash> Rsos<K, V> for FingerprintTree<K, V> {
    fn size(&self) -> usize {
        self.len()
    }

    fn aggregate<R: RangeBounds<K>>(&self, range: &R) -> (usize, Fingerprint) {
        self.range_aggregate(range)
    }

    fn rank(&self, z: &K) -> usize {
        self.insertion_position(z)
    }

    fn select(&self, r: usize) -> &K {
        self.key_at(r)
    }

    fn enumerate<'a, R: RangeBounds<K>>(&'a self, range: &'a R) -> ItemRange<'a, K, V, R>
    where
        K: Ord,
    {
        self.get_range(range)
    }

    fn insert(&mut self, key: K, value: V) -> Option<V> {
        FingerprintTree::insert(self, key, value)
    }

    fn delete(&mut self, key: &K) -> Option<V> {
        self.remove(key)
    }
}
