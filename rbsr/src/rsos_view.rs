// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RsosView`]: the read-only slice of the RSOS contract the protocol driver needs.

use std::ops::RangeBounds;

use rsos::{Aggregate, Rsos};

/// The four Def. 3.9 operations RBSR performs against a local set: `size`, `Aggregate`, `Rank`,
/// `Select`.
///
/// Narrower than [`Rsos`], and carrying no value type at all — RBSR never touches a stored value.
/// Never implemented by hand: the blanket impl below covers every [`Rsos`].
pub trait RsosView<K> {
    /// `size()` → `|X|`: the number of elements currently in the set.
    fn size(&self) -> usize;

    /// `Aggregate(l, u)` → `A(X ∩ [l, u))`. Taken by value, as [`rsos::Rsos::aggregate`] is.
    fn aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate;

    /// `Rank(z)` → `Rank_X(z)`: the number of keys strictly below `z`.
    fn rank(&self, z: &K) -> usize;

    /// `Select(r)` → `Select_X(r)`: the key at in-order position `r`.
    ///
    /// # Panics
    ///
    /// If `r >= size()`, as [`Rsos::select`] does.
    fn select(&self, r: usize) -> &K;
}

/// Every RSOS is an RSOS view. Fully-qualified forwarding: both traits name these methods.
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
