// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Construction and [`RefinementPolicy`](super::RefinementPolicy) for
//! [`FixedFanOut`](super::FixedFanOut), **the default policy**.

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, FanOut, RefinementPolicy, SplitStride};

/// **The default policy**: `SPLITBYRANK(O_X, l, u, b)` at a constant `b`, with this crate's
/// enumeration cutoffs.
///
/// A constant `b` is what makes the family's published bounds apply: `O(d log n)` communication,
/// `Θ(log_b n)` rounds, `T_loc = O(hL + bhI + K)`.
///
/// [`Default`] is [`FanOut::NEGENTROPY`]. `b` trades three quantities that bottom out separately —
/// bytes and local work follow `b / ln b`, one-way messages fall as `log_b n` to a floor, and the
/// widest round grows linearly in `b` and must fit a datagram. Swept over 2…256 by
/// `benches/protocol.rs`'s `fan_out_sweep`; the chosen value's evidence is in `SOTA.md` §2.2.
///
/// ```
/// use rbsr::{Comparison, Decision, FanOut, FixedFanOut, RefinementPolicy};
/// use rsos::{Aggregate, Fingerprint};
///
/// let small = Comparison::new(
///     Aggregate::new(100, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(100, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
/// let large = Comparison::new(
///     Aggregate::new(1_000_000, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(1_000_000, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
///
/// // The child count stays at or under `b` however wide the range is -- unlike `SqrtFanOut`,
/// // whose child count grows with it.
/// for comparison in [small, large] {
///     let Decision::Split(stride) = FixedFanOut::default().decide(comparison) else {
///         panic!("a mismatching range must split");
///     };
///     let children = comparison.span().div_ceil(stride.get());
///     assert!(children <= FanOut::NEGENTROPY.get());
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedFanOut {
    fan_out: FanOut,
}

impl FixedFanOut {
    /// A policy splitting into at most `fan_out` children per range.
    pub const fn new(fan_out: FanOut) -> FixedFanOut {
        FixedFanOut { fan_out }
    }

    /// The branching factor this policy splits at.
    pub const fn fan_out(&self) -> FanOut {
        self.fan_out
    }
}

impl Default for FixedFanOut {
    fn default() -> FixedFanOut {
        FixedFanOut::new(FanOut::NEGENTROPY)
    }
}

impl RefinementPolicy for FixedFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        Decision::Split(SplitStride::for_fan_out(comparison.span(), self.fan_out))
    }
}
