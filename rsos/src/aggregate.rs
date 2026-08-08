// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The bundled range aggregate [`Aggregate`]: Def. 3.5's `A(S) = (|S|, Σ(S))`.
//!
//! E. G. Amparore, *Range-Based Set Reconciliation via Range-Summarizable Order-Statistics
//! Stores* (arXiv:2603.19820), Def. 3.5, defines the aggregate not as an arbitrary pair but as a
//! **monoid** `A := (ℕ × M, ⊗, (0, 0_M))`, where `M` is Def. 3.4's element-summary monoid — here
//! [`Fingerprint`] — and `ℕ` is the element count. `⊗` composes componentwise, and `(0, 0_M)` is
//! [`Aggregate::ZERO`]. Because the aggregate of a range is exactly the composition of the
//! aggregates of any partition of that range, a single `O(log n)` tree walk answers both "how many
//! elements" and "what is their combined summary" at once.

use std::ops::{Add, AddAssign};

use crate::fingerprint::Fingerprint;

/// The bundled aggregate `A(S) = (|S|, Σ(S))` over a set (in practice, a range) of elements:
/// Def. 3.5 of arXiv:2603.19820. See the [module docs](self) for the citation.
///
/// This is a **monoid**, not a group: composition is [`Add`] (`⊗`, componentwise), and the
/// identity is [`ZERO`](Aggregate::ZERO) (`(0, 0_M)`).
///
/// # Why there is no `Sub`/`Neg`
///
/// [`Fingerprint`] alone happens to form an abelian *group* — it has `Sub` and `Neg`, since
/// 256-bit addition is invertible. `Aggregate` deliberately does **not** inherit that: `ℕ` under
/// addition is only a monoid, so subtracting a larger count from a smaller one has no meaningful
/// answer, and a `usize` subtraction would silently wrap in release builds while panicking in
/// debug. Def. 3.5 asks for a monoid and this type provides exactly a monoid. Call sites that
/// genuinely need to subtract summaries operate on [`Fingerprint`] directly (reachable through
/// [`fingerprint()`](Aggregate::fingerprint)), which still supports `-` and unary `-`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Aggregate {
    size: usize,
    fingerprint: Fingerprint,
}

impl Aggregate {
    /// The aggregate of the empty range and the identity of `⊗`: `(0, 0_M)`.
    pub const ZERO: Aggregate = Aggregate {
        size: 0,
        fingerprint: Fingerprint::ZERO,
    };

    /// Build an aggregate from its two components: the element count `|S|` and the combined
    /// element summary `Σ(S)`.
    pub const fn new(size: usize, fingerprint: Fingerprint) -> Aggregate {
        Aggregate { size, fingerprint }
    }

    /// `|S|`: the number of elements covered by this aggregate (Def. 3.9's `size` operation when
    /// taken over the whole store).
    pub const fn size(&self) -> usize {
        self.size
    }

    /// `Σ(S)`: the combined element summary (Def. 3.4's monoid `M`).
    ///
    /// NOTE: a *non-empty* range can legitimately summarize to [`Fingerprint::ZERO`]; emptiness is
    /// decided on [`size`](Aggregate::size) — see [`is_empty`](Aggregate::is_empty).
    pub const fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// Whether the aggregate covers no elements at all, i.e. `|S| == 0`.
    ///
    /// Decided on the count, never on the fingerprint: see [`Fingerprint`]'s own note on why a
    /// zero fingerprint does not imply an empty range.
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }
}

impl Add for Aggregate {
    type Output = Aggregate;

    /// `⊗`: componentwise composition of two aggregates over disjoint sets.
    fn add(self, rhs: Aggregate) -> Aggregate {
        Aggregate {
            size: self.size + rhs.size,
            fingerprint: self.fingerprint + rhs.fingerprint,
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
    use crate::fingerprint::hash;

    #[test]
    fn zero_is_identity() {
        let a = Aggregate::new(3, hash(&1u64, &"a") + hash(&2u64, &"b") + hash(&3u64, &"c"));
        assert_eq!(a + Aggregate::ZERO, a);
        assert_eq!(Aggregate::ZERO + a, a);
        assert_eq!(Aggregate::ZERO, Aggregate::default());
    }

    #[test]
    fn add_is_commutative_and_associative() {
        let a = Aggregate::new(1, hash(&1u64, &10u64));
        let b = Aggregate::new(2, hash(&2u64, &20u64));
        let c = Aggregate::new(4, hash(&3u64, &30u64));
        assert_eq!(a + b, b + a);
        assert_eq!((a + b) + c, a + (b + c));
    }

    #[test]
    fn add_composes_componentwise() {
        let fa = hash(&1u64, &10u64);
        let fb = hash(&2u64, &20u64);
        let sum = Aggregate::new(2, fa) + Aggregate::new(5, fb);
        assert_eq!(sum.size(), 7);
        assert_eq!(sum.fingerprint(), fa + fb);
    }

    #[test]
    fn add_assign_matches_add() {
        let a = Aggregate::new(2, hash(&1u64, &10u64));
        let b = Aggregate::new(3, hash(&2u64, &20u64));
        let mut acc = a;
        acc += b;
        assert_eq!(acc, a + b);
    }

    #[test]
    fn emptiness_is_decided_on_the_count_not_the_fingerprint() {
        assert!(Aggregate::ZERO.is_empty());
        // A non-empty aggregate whose summary happens to be zero is still non-empty.
        let f = hash(&7u64, &"x");
        let zero_summary = Aggregate::new(2, f + (-f));
        assert_eq!(zero_summary.fingerprint(), Fingerprint::ZERO);
        assert!(!zero_summary.is_empty());
        assert_eq!(zero_summary.size(), 2);
    }
}
