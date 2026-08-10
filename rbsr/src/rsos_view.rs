// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RsosView`]: the read-only slice of the RSOS contract that the RBSR protocol driver actually needs.

use std::ops::RangeBounds;

use rsos::{Aggregate, Rsos};

/// The four read-only operations RBSR performs against a local set, named with the literal terms of
/// Def. 3.9 of E. G. Amparore, *Range-Based Set Reconciliation via Range-Summarizable
/// Order-Statistics Stores* (arXiv:2603.19820): `size`, `Aggregate`, `Rank`, `Select`.
///
/// This is deliberately narrower than the full seven-operation [`Rsos`] contract: `Enumerate`,
/// `Insert` and `Delete` are absent because the protocol driver never calls them — it reads key
/// positions and range aggregates and nothing else. For the same reason `RsosView` carries **no
/// value type at all**, neither parameter nor associated type: RBSR never touches a stored value,
/// so nothing in this crate's public signatures needs to name one.
///
/// There is no need to implement this by hand. The blanket implementation below covers every
/// [`Rsos`] implementor, so [`rsos::FingerprintTreeMap`] — and any future RSOS backend — satisfies it
/// automatically.
pub trait RsosView<K> {
    /// `size()` → `|X|`: the number of elements currently in the set.
    fn size(&self) -> usize;

    /// `Aggregate(l, u)` → `A(X ∩ [l, u))`: the bundled [`Aggregate`] over a range of keys
    /// (Def. 3.5's `A(S) = (|S|, Σ(S))`), answering "how many" and "which fingerprint" in a
    /// single query rather than two.
    fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate;

    /// `Rank(z)` → `Rank_X(z)`: the position `z` occupies (or would occupy) in the in-order
    /// sequence of keys — equivalently, the number of keys strictly below `z`.
    fn rank(&self, z: &K) -> usize;

    /// `Select(r)` → `Select_X(r)`: the key at in-order position `r`. Panics if `r` is out of
    /// bounds, matching [`Rsos::select`].
    fn select(&self, r: usize) -> &K;
}

/// Every RSOS is an RSOS view. `RsosView` is this crate's own trait, so the orphan rules allow the
/// blanket implementation over the foreign [`Rsos`] — and the dependency edge still points one way
/// only (`rbsr` → `rsos`; `rsos` neither depends on nor knows about `rbsr`).
///
/// Each method forwards to its `Rsos` namesake with fully-qualified syntax: both traits are in
/// scope here and both name these methods, so `self.size()` would be ambiguous.
impl<K, T: Rsos<K>> RsosView<K> for T {
    fn size(&self) -> usize {
        <T as Rsos<K>>::size(self)
    }

    fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate {
        <T as Rsos<K>>::aggregate(self, range)
    }

    fn rank(&self, z: &K) -> usize {
        <T as Rsos<K>>::rank(self, z)
    }

    fn select(&self, r: usize) -> &K {
        <T as Rsos<K>>::select(self, r)
    }
}
