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

/// `threshold()` reflects the constructed value, not just the degenerate `0 -> 1` case above.
#[test]
fn threshold_accessor_returns_the_constructed_value() {
    assert_eq!(
        EnumerateBelowThreshold::new(32, FanOut::BINARY).threshold(),
        32
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

// ----- #356 probe policies: the 2x2 separating oracle-coupling from span-relativity -----

/// Two aggregates of the given sizes guaranteed not to agree, the local one carrying `limb`.
fn probe_mismatch(local: usize, remote: usize, limb: u64) -> Comparison {
    Comparison::new(
        Aggregate::new(local, Fingerprint([limb, 0, 0, 0])),
        Aggregate::new(remote, Fingerprint([u64::MAX, 9, 9, 9])),
        0,
    )
}

fn stride_of<P: RefinementPolicy>(policy: &P, comparison: Comparison) -> usize {
    let Decision::Split(stride) = policy.decide(comparison) else {
        panic!("span={} must split", comparison.span());
    };
    stride.get()
}

/// Pins the exact formula (`1 + limb % 32`) against known limbs, not just "differs from a
/// rank-cut policy somewhere": the mutation gate needs a witness for `+` rather than `*`, and
/// for `%` rather than `/`.
#[test]
fn fingerprint_derived_stride_is_one_plus_limb_mod_32() {
    // 0 and 32 both reduce to remainder 0 (stride 1, pinning `%` over `/`, which would give 0
    // and 1); 31 and 63 both reduce to 31 (stride 32, pinning `+` over `*`, which would give
    // 0 and 0).
    for (limb, expected) in [(0u64, 1usize), (31, 32), (32, 1), (63, 32)] {
        assert_eq!(
            stride_of(&FingerprintDerivedSplit, probe_mismatch(1_000, 2_000, limb)),
            expected,
            "limb {limb}"
        );
    }
}

/// The defect [`ConstantStrideSplit`] is the control for, stated as a property rather than
/// asserted of one span: a span-independent stride stops refining once the span reaches it.
#[test]
fn span_independent_strides_stop_refining_below_their_own_spread() {
    for span in 2..=STRIDE_SPREAD as usize {
        // A constant at the top of the spread never refines anywhere in this window.
        assert!(
            stride_of(
                &ConstantStrideSplit::per_child(STRIDE_SPREAD as usize),
                probe_mismatch(span, span + 1, 7)
            ) >= span,
            "span={span}: a constant stride of {STRIDE_SPREAD} must not refine it"
        );
    }
    // Both span-independent probes put *some* limb/span in the no-progress region, which is
    // what a span-relative stride makes impossible.
    assert!(
        (0..64u64).any(|limb| stride_of(&FingerprintDerivedSplit, probe_mismatch(4, 5, limb)) >= 4)
    );
    assert!((2..=STRIDE_SPREAD as usize)
        .any(|span| stride_of(&SpanHashedStrideSplit, probe_mismatch(span, span + 1, 7)) >= span));
}

/// Pins [`SpanRelativeFingerprintSplit`]'s exact formula, `1 + limb % (span − 1)`.
///
/// `span_relative_fingerprint_stride_always_refines` below cannot do this on its own: dropping
/// the `+ 1` leaves the stride inside `1..span` too, because `SplitStride::per_child` raises a
/// zero stride to one. Distinguishing the two needs a witness where the remainder is non-zero,
/// so the `+ 1` is observable rather than absorbed by that clamp.
#[test]
fn span_relative_fingerprint_stride_is_one_plus_limb_mod_span_minus_one() {
    // (span, limb, expected): remainder 0 pins that the clamp is not what produces the 1;
    // remainders 5 and 98 pin `+` over `*` and over `%`, each of which would drop the offset.
    for (span, limb, expected) in [
        (100usize, 99u64, 1usize),
        (100, 5, 6),
        (100, 98, 99),
        (3, 1, 2),
    ] {
        assert_eq!(
            stride_of(
                &SpanRelativeFingerprintSplit,
                probe_mismatch(span, span + 1, limb)
            ),
            expected,
            "span={span}, limb={limb}"
        );
    }
}

/// The joint-progress property, as a property over the whole reachable input space rather
/// than a literal: a span-relative stride always cuts at least two children.
#[test]
fn span_relative_fingerprint_stride_always_refines() {
    for span in 2..512usize {
        for limb in [0u64, 1, 7, 31, 32, 1_000_003, u64::MAX / 3, u64::MAX] {
            let stride = stride_of(
                &SpanRelativeFingerprintSplit,
                probe_mismatch(span, span + 1, limb),
            );
            assert!(
                (1..span).contains(&stride),
                "span={span}, limb={limb}: stride {stride} is outside 1..{span}"
            );
            assert!(span.div_ceil(stride) >= 2, "span={span}, limb={limb}");
        }
    }
}

/// The oracle-coupled column must actually read the oracle, or it is not testing what it
/// claims; the oracle-independent column must actually ignore it, same reason.
#[test]
fn only_the_oracle_coupled_column_reacts_to_the_fingerprint() {
    let quiet = probe_mismatch(100, 200, 0);
    let loud = probe_mismatch(100, 200, 17);
    assert_ne!(
        stride_of(&FingerprintDerivedSplit, quiet),
        stride_of(&FingerprintDerivedSplit, loud)
    );
    assert_ne!(
        stride_of(&SpanRelativeFingerprintSplit, quiet),
        stride_of(&SpanRelativeFingerprintSplit, loud)
    );
    assert_eq!(
        stride_of(&SpanHashedStrideSplit, quiet),
        stride_of(&SpanHashedStrideSplit, loud)
    );
    let constant = ConstantStrideSplit::per_child(7);
    assert_eq!(stride_of(&constant, quiet), stride_of(&constant, loud));
    assert_eq!(constant.stride().get(), 7);
}

/// `mix` must scatter *near-uniformly*, not merely reach every value: a control for
/// [`FingerprintDerivedSplit`] has to draw from the same distribution, not just the same
/// support. Asserted as a two-sided bound on every stride's frequency, which is also what
/// pins each operation in `mix` — drop or alter either the multiply or the shift and the
/// whole span range collapses onto one stride.
#[test]
fn the_span_hashed_stride_is_near_uniform_over_its_spread() {
    const SPANS: usize = 10_000;
    let mut counts = [0usize; STRIDE_SPREAD as usize];
    for span in 2..2 + SPANS {
        let stride = stride_of(&SpanHashedStrideSplit, probe_mismatch(span, span + 1, 0));
        assert!(
            (1..=STRIDE_SPREAD as usize).contains(&stride),
            "span={span}: stride {stride} is outside 1..={STRIDE_SPREAD}"
        );
        counts[stride - 1] += 1;
    }
    let expected = SPANS as f64 / STRIDE_SPREAD as f64; // 312.5
                                                        // ±20% of uniform. Measured spread over these spans is 310..=315; every arithmetic
                                                        // mutation of `mix` puts all 10,000 spans on a single stride, i.e. one count at 10,000
                                                        // and the other 31 at zero.
    let (lo, hi) = ((expected * 0.8) as usize, (expected * 1.2) as usize);
    for (index, &count) in counts.iter().enumerate() {
        assert!(
            (lo..=hi).contains(&count),
            "stride {} occurred {count} times over {SPANS} spans, outside {lo}..={hi} — \
             the span-hashed stride is not scattering uniformly",
            index + 1
        );
    }
}
