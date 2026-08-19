// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The bundled range aggregate [`Aggregate`]: the monoid `A := (ℕ × M, ⊗, (0, 0_M))` of
//! arXiv:2603.19820 Def. 3.5, with `M` = [`Fingerprint`].

use std::ops::{Add, AddAssign};

use serde::{Deserialize, Serialize};

use crate::fingerprint::Fingerprint;

/// The bundled aggregate `A(S) = (|S|, Σ(S))` over a range of elements (arXiv:2603.19820
/// Def. 3.5).
///
/// A **monoid**, not a group: composition is [`Add`], identity is [`ZERO`](Aggregate::ZERO). No
/// `Sub`/`Neg` — `ℕ` under addition is not a group; subtract on [`Fingerprint`] instead.
///
/// Field declaration order is the wire order: `fingerprint` before `size`, inverse of Def. 3.5's
/// written pair. Reordering is a silent wire break, caught only by `tests/wire_format.rs`.
///
/// ```
/// use rsos::{lift, Aggregate};
///
/// let a = Aggregate::new(1, lift(&1, &"one"));
/// let b = Aggregate::new(1, lift(&2, &"two"));
///
/// // Aggregates for disjoint ranges compose under `+`, and the count adds up -- this is what
/// // lets a subtree's aggregate be maintained from its children's, not recomputed on every read.
/// let whole = a + b;
/// assert_eq!(whole.size(), 2);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregate {
    fingerprint: Fingerprint,
    size: usize,
}

impl Aggregate {
    /// The aggregate of the empty range and the identity of `⊗`: `(0, 0_M)`.
    pub const ZERO: Aggregate = Aggregate {
        fingerprint: Fingerprint::ZERO,
        size: 0,
    };

    /// Build an aggregate from `(|S|, Σ(S))` — Def. 3.5's argument order, not the field order.
    #[must_use]
    pub const fn new(size: usize, fingerprint: Fingerprint) -> Aggregate {
        Aggregate { fingerprint, size }
    }

    /// `|S|`: the number of elements covered.
    #[must_use]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// `Σ(S)`: the combined element summary.
    ///
    /// A non-empty range can summarize to [`Fingerprint::ZERO`]; emptiness is decided on
    /// [`size`](Aggregate::size).
    #[must_use]
    pub const fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// `|S| == 0`. Decided on the count, never on the fingerprint.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }
}

impl Add for Aggregate {
    type Output = Aggregate;

    /// `⊗`: componentwise, over disjoint sets.
    fn add(self, rhs: Aggregate) -> Aggregate {
        Aggregate {
            fingerprint: self.fingerprint + rhs.fingerprint,
            size: self.size + rhs.size,
        }
    }
}

impl AddAssign for Aggregate {
    fn add_assign(&mut self, rhs: Aggregate) {
        *self = *self + rhs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::lift;

    #[test]
    fn zero_is_identity() {
        let a = Aggregate::new(3, lift(&1u64, &"a") + lift(&2u64, &"b") + lift(&3u64, &"c"));
        assert_eq!(a + Aggregate::ZERO, a);
        assert_eq!(Aggregate::ZERO + a, a);
        assert_eq!(Aggregate::ZERO, Aggregate::default());
    }

    #[test]
    fn add_is_commutative_and_associative() {
        let a = Aggregate::new(1, lift(&1u64, &10u64));
        let b = Aggregate::new(2, lift(&2u64, &20u64));
        let c = Aggregate::new(4, lift(&3u64, &30u64));
        assert_eq!(a + b, b + a);
        assert_eq!((a + b) + c, a + (b + c));
    }

    #[test]
    fn add_composes_componentwise() {
        let fa = lift(&1u64, &10u64);
        let fb = lift(&2u64, &20u64);
        let sum = Aggregate::new(2, fa) + Aggregate::new(5, fb);
        assert_eq!(sum.size(), 7);
        assert_eq!(sum.fingerprint(), fa + fb);
    }

    #[test]
    fn add_assign_matches_add() {
        let a = Aggregate::new(2, lift(&1u64, &10u64));
        let b = Aggregate::new(3, lift(&2u64, &20u64));
        let mut acc = a;
        acc += b;
        assert_eq!(acc, a + b);
    }

    #[test]
    fn emptiness_is_decided_on_the_count_not_the_fingerprint() {
        assert!(Aggregate::ZERO.is_empty());
        // A non-empty aggregate whose summary happens to be zero is still non-empty.
        let f = lift(&7u64, &"x");
        let zero_summary = Aggregate::new(2, f + (-f));
        assert_eq!(zero_summary.fingerprint(), Fingerprint::ZERO);
        assert!(!zero_summary.is_empty());
        assert_eq!(zero_summary.size(), 2);
    }
}
