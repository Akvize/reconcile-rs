// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! #420: pins `RefinementPolicy`'s progress law (`rbsr/src/policy.rs`, "Law: eventual progress")
//! for every *shipped* policy — whenever [`Comparison::span`] is greater than one, a
//! [`Decision::Split`] must choose a stride strictly below the span. That is what makes the
//! driver's own guard (`ARCHITECTURE.md` §5 invariant 13) a backstop for the shipped policies
//! rather than something they lean on: catching a regression here, at the policy, is cheaper than
//! catching it as a forced-`Enumerate` fallback at the driver.
//!
//! `FingerprintDerivedSplit` (#356) is deliberately excluded — it exists precisely to violate this
//! law, `cfg(reconcile_internal_testing)`-gated so it can never ship. `oracle_dependent_split_vs_the_union_bound.rs`
//! measures it.

#![forbid(unsafe_code)]

use proptest::prelude::*;

use rbsr::{
    Comparison, Decision, EnumerateBelowThreshold, FanOut, FixedFanOut, RefinementPolicy,
    SqrtFanOut,
};
use rsos::{Aggregate, Fingerprint};

/// A comparison over two arbitrary same-shaped aggregates. The fingerprint limbs are free
/// parameters (not tied to `local`/`remote`) so a policy cannot pass by accident correlating span
/// with fingerprint content — the exact coupling #356's counter-example exploited.
fn comparison(local: usize, remote: usize, local_limb: u64, remote_limb: u64) -> Comparison {
    Comparison::new(
        Aggregate::new(local, Fingerprint([local_limb, 0, 0, 0])),
        Aggregate::new(remote, Fingerprint([remote_limb, 0, 0, 0])),
        0,
    )
}

proptest! {
    #[test]
    fn sqrt_fan_out_always_narrows_a_span_above_one(
        local in 2usize..1_000_000,
        remote in 0usize..1_000_000,
        local_limb: u64,
        remote_limb: u64,
    ) {
        let comparison = comparison(local, remote, local_limb, remote_limb);
        if let Decision::Split(stride) = SqrtFanOut.decide(comparison) {
            prop_assert!(
                stride.get() < comparison.span(),
                "span={}: stride {} does not narrow the range",
                comparison.span(),
                stride.get()
            );
        }
    }

    #[test]
    fn fixed_fan_out_always_narrows_a_span_above_one(
        local in 2usize..1_000_000,
        remote in 0usize..1_000_000,
        local_limb: u64,
        remote_limb: u64,
        fan_out in 0usize..256,
    ) {
        let policy = FixedFanOut::new(FanOut::new(fan_out));
        let comparison = comparison(local, remote, local_limb, remote_limb);
        if let Decision::Split(stride) = policy.decide(comparison) {
            prop_assert!(
                stride.get() < comparison.span(),
                "span={}, b={}: stride {} does not narrow the range",
                comparison.span(),
                fan_out,
                stride.get()
            );
        }
    }

    #[test]
    fn enumerate_below_threshold_always_narrows_a_span_above_one(
        local in 2usize..1_000_000,
        remote in 0usize..1_000_000,
        local_limb: u64,
        remote_limb: u64,
        threshold in 0usize..256,
        fan_out in 0usize..256,
    ) {
        let policy = EnumerateBelowThreshold::new(threshold, FanOut::new(fan_out));
        let comparison = comparison(local, remote, local_limb, remote_limb);
        if let Decision::Split(stride) = policy.decide(comparison) {
            prop_assert!(
                stride.get() < comparison.span(),
                "span={}, t={}, b={}: stride {} does not narrow the range",
                comparison.span(),
                threshold,
                fan_out,
                stride.get()
            );
        }
    }
}
