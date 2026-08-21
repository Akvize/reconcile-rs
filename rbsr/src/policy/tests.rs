// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::*;

use rsos::Fingerprint;

/// Two aggregates of the given sizes guaranteed not to agree, plus a fresh round budget.
fn mismatch(local: usize, remote: usize) -> Comparison {
    Comparison::new(
        Aggregate::new(local, Fingerprint([1, 0, 0, 0])),
        Aggregate::new(remote, Fingerprint([2, 0, 0, 0])),
        0,
    )
}

/// How many children a stride emits over a span, mirroring the driver's loop.
fn children(span: usize, stride: SplitStride) -> usize {
    span.div_ceil(stride.get()).max(1)
}

#[test]
fn agreeing_aggregates_are_skipped_by_every_policy() {
    let aggregate = Aggregate::new(1_000, Fingerprint([9, 9, 9, 9]));
    let agreed = Comparison::new(aggregate, aggregate, 0);
    assert_eq!(SqrtFanOut.decide(agreed), Decision::Skip);
    assert_eq!(FixedFanOut::default().decide(agreed), Decision::Skip);
    assert_eq!(
        EnumerateBelowThreshold::PAPER.decide(agreed),
        Decision::Skip
    );
}

/// Matching fingerprints with mismatched sizes must not be read as agreement.
#[test]
fn matching_fingerprint_with_wrong_size_does_not_agree() {
    let comparison = Comparison::new(Aggregate::new(2, Fingerprint::ZERO), Aggregate::ZERO, 0);
    assert!(!comparison.agrees());
    assert_ne!(SqrtFanOut.decide(comparison), Decision::Skip);
}

#[test]
fn sqrt_fan_out_emits_root_m_children() {
    for span in [100usize, 400, 2_500, 1_000_000] {
        let Decision::Split(stride) = SqrtFanOut.decide(mismatch(span, span)) else {
            panic!("a mismatching range of {span} elements must split");
        };
        assert_eq!(stride.get(), (span as f32).sqrt() as usize);
        let emitted = children(span, stride);
        let root = (span as f64).sqrt() as usize;
        assert!(
            emitted >= root / 2 && emitted <= root * 2,
            "span={span}: {emitted} children, expected ~√span = {root}"
        );
    }
}

/// The child count must stop growing with the range.
#[test]
fn fixed_fan_out_is_constant_in_the_range_size() {
    let policy = FixedFanOut::default();
    for span in [100usize, 400, 2_500, 1_000_000] {
        let Decision::Split(stride) = policy.decide(mismatch(span, span)) else {
            panic!("a mismatching range of {span} elements must split");
        };
        assert!(
            children(span, stride) <= policy.fan_out().get(),
            "span={span}: {} children exceeds b={}",
            children(span, stride),
            policy.fan_out().get()
        );
        assert!(stride.get() < span, "span={span}: the split must refine");
    }
}

#[test]
fn algorithm1_enumerates_at_or_below_the_threshold_and_splits_above() {
    let policy = EnumerateBelowThreshold::new(32, FanOut::NEGENTROPY);
    for span in [0usize, 1, 31, 32] {
        assert_eq!(policy.decide(mismatch(span, 64)), Decision::Enumerate);
    }
    let Decision::Split(stride) = policy.decide(mismatch(33, 64)) else {
        panic!("a range above the threshold must split");
    };
    assert!(stride.get() < 33);
}

/// Neither a zero stride nor a fan-out of one is representable, so neither can hang the
/// protocol.
#[test]
fn degenerate_parameters_are_unrepresentable() {
    assert_eq!(SplitStride::per_child(0), SplitStride::ONE);
    assert_eq!(
        SplitStride::for_fan_out(0, FanOut::BINARY),
        SplitStride::ONE
    );
    assert_eq!(FanOut::new(0), FanOut::BINARY);
    assert_eq!(FanOut::new(1), FanOut::BINARY);
    assert_eq!(
        EnumerateBelowThreshold::new(0, FanOut::BINARY).threshold(),
        1
    );
}

#[test]
fn for_fan_out_never_exceeds_the_requested_branching_factor() {
    for span in [2usize, 3, 5, 9, 10, 17, 1_000, 999_983] {
        for b in [2usize, 3, 16, 64] {
            let fan_out = FanOut::new(b);
            let stride = SplitStride::for_fan_out(span, fan_out);
            assert!(
                children(span, stride) <= b,
                "span={span}, b={b}: {} children",
                children(span, stride)
            );
            assert!(stride.get() < span, "span={span}, b={b}: must refine");
        }
    }
}

/// Pins [`FingerprintDerivedSplit`]'s exact stride formula (`1 + limb % 32`) against known
/// fingerprint limbs, not just "differs from a rank-cut policy somewhere" — the mutation gate
/// (`AGENTS.md` `.claude/rules/tests.md`) needs a witness for `+`, not `*` or another operator
/// combining the `1`, and for `%`, not `/`, dividing by `32`.
#[cfg(reconcile_internal_testing)]
#[test]
fn fingerprint_derived_split_stride_is_one_plus_limb_mod_32() {
    // (fingerprint low limb, expected stride): 0 and 32 both reduce to remainder 0 (stride 1,
    // pinning `%` over `/`, which would instead give 0 and 1); 31 and 63 both reduce to 31
    // (stride 32, pinning `+` over `*`, which would instead give 0 and 0).
    for (limb, expected_stride) in [(0u64, 1usize), (31, 32), (32, 1), (63, 32)] {
        let comparison = Comparison::new(
            Aggregate::new(1_000, Fingerprint([limb, 0, 0, 0])),
            Aggregate::new(2_000, Fingerprint([9, 9, 9, 9])),
            0,
        );
        let Decision::Split(stride) = FingerprintDerivedSplit.decide(comparison) else {
            panic!("span={}, remote={}: must split", comparison.span(), 2_000);
        };
        assert_eq!(
            stride.get(),
            expected_stride,
            "fingerprint limb {limb}: expected stride {expected_stride}"
        );
    }
}
