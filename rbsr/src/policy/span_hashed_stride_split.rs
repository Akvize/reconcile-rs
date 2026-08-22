// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RefinementPolicy`](super::RefinementPolicy) for
//! [`SpanHashedStrideSplit`](super::SpanHashedStrideSplit) — `cfg(reconcile_internal_testing)`-gated,
//! see the type's own docs for what it controls for.

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, RefinementPolicy, SplitStride};

/// The width of the span-independent probes' stride range: strides land in `1..=STRIDE_SPREAD`.
///
/// Named once because [`FingerprintDerivedSplit`](super::FingerprintDerivedSplit) and
/// [`SpanHashedStrideSplit`] must draw from the *same* support for the second to be a control for
/// the first.
pub const STRIDE_SPREAD: u64 = 32;

/// **Test-only probe (#356), `cfg(reconcile_internal_testing)`-gated.**
/// [`FingerprintDerivedSplit`](super::FingerprintDerivedSplit)'s stride *distribution*, drawn from
/// the span instead of the fingerprint: `1 + mix(span) mod 32`.
///
/// The tighter of the two oracle-independent controls.
/// [`ConstantStrideSplit`](super::ConstantStrideSplit) differs from the coupled probe in both the
/// source of the stride and its spread; this one differs in the source alone — it is a
/// deterministic function of how many elements fall in the range, exactly the "cut by rank"
/// property the soundness union bound needs, and it scatters over the same `1..=`[`STRIDE_SPREAD`]
/// support. Never a shipped policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpanHashedStrideSplit;

/// Fibonacci hashing: scatter a small count over the whole word, then read the **high** bits.
///
/// Emphatically not a range digest — the input is a count of elements, which
/// [`Comparison::span`](super::Comparison::span) already exposes as a soundness-safe quantity.
/// Both halves are load-bearing: multiplying by an odd constant leaves the low bits almost
/// unmoved, so the shift is what turns the product into an avalanche.
///
/// Deliberately two operations rather than a full SplitMix64 finalizer. Over a span this small
/// every `x ^= x >> k` step of that finalizer is the identity — `x >> 30` is zero for any span
/// under a billion — so those steps would be unreachable code no test over a realistic span could
/// distinguish from any mutation of it.
fn mix(x: u64) -> u64 {
    x.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40
}

impl RefinementPolicy for SpanHashedStrideSplit {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        let stride = 1 + mix(comparison.span() as u64) % STRIDE_SPREAD;
        Decision::Split(SplitStride::per_child(stride as usize))
    }
}
