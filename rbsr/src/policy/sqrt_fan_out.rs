// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RefinementPolicy`](super::RefinementPolicy) for [`SqrtFanOut`](super::SqrtFanOut).

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, RefinementPolicy, SplitStride};

/// Cut every `⌊√m⌋` elements: `Θ(√m)` children, still a rank-balanced partition.
///
/// Enumeration cutoffs are [`FixedFanOut`](super::FixedFanOut)'s (`cutoffs::shared_cutoffs`), plus
/// a lone local element facing a larger remote range, which is re-advertised for the peer to cut.
///
/// **Cost.** A size-derived stride makes the first SPLIT of a whole-store round emit `~√n`
/// children whatever `d` is: communication is `Θ(√n)`, not `O(d log n)`, and the paper's
/// `T_loc = O(hL + bhI + K)` does not apply. It buys depth — `Θ(log log n)` rounds. Competitive
/// only as `d` approaches `√n`; measured in `benches/protocol.rs`.
///
/// ```
/// use rbsr::{Comparison, Decision, RefinementPolicy, SqrtFanOut};
/// use rsos::{Aggregate, Fingerprint};
///
/// let comparison = Comparison::new(
///     Aggregate::new(400, Fingerprint([1, 0, 0, 0])),
///     Aggregate::new(400, Fingerprint([2, 0, 0, 0])),
///     0,
/// );
/// let Decision::Split(stride) = SqrtFanOut.decide(comparison) else {
///     panic!("a mismatching range must split");
/// };
/// // The stride is the square root of the span, so -- unlike `FixedFanOut` -- the child count
/// // grows with the range instead of staying capped at a constant `b`.
/// assert_eq!(stride.get(), 20);
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SqrtFanOut;

impl RefinementPolicy for SqrtFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        // `f32` and truncation are part of the rule: past f32's mantissa `f64` would disagree.
        let stride = (comparison.span() as f32).sqrt() as usize;
        Decision::Split(SplitStride::per_child(stride))
    }
}
