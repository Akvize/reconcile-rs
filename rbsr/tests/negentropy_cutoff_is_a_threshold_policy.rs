// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! #468: pins the identification `EnumerateBelowThreshold`'s rustdoc rests on — Negentropy's
//! enumeration cutoff **is** this crate's `t`, at `t = 2b - 1`.
//!
//! Its `splitRange` carries one size-based rule, `numElems < buckets * 2` ships an IdList and
//! anything wider splits into `buckets` children, where [`EnumerateBelowThreshold`] enumerates at
//! `span <= t`. That equivalence is what lets `benches/README.md`'s Negentropy anchor attribute the
//! measured descent gap (fewer ranges in fewer messages at the same nominal `b`) to the cutoff
//! rather than to the fan-out, and what lets `t = 2b` in the sweep stand for what they ship. Prose
//! is not evidence a rule still holds, so it is checked here instead of read.
//!
//! Both claims are decided by exhaustive enumeration rather than sampling: the rule is a
//! comparison against a small integer, so every span up to a multiple of `b` covers it exactly,
//! with no seed and nothing left to a draw (`.claude/rules/tests.md`).

#![forbid(unsafe_code)]

use rbsr::{Comparison, Decision, EnumerateBelowThreshold, FanOut, RefinementPolicy, SplitStride};
use rsos::{Aggregate, Fingerprint};

/// The branching factors checked: Negentropy's own, the floor, and one in between.
const FAN_OUTS: [usize; 3] = [2, 5, 16];

/// A range of `span` local elements that cannot be mistaken for an agreeing one — the remote side
/// differs in size, so no fingerprint coincidence can turn the decision into a `Skip`.
///
/// Only `span` varies: both cutoffs read the local count alone (`numElems` there, `|X ∩ [l, u)|`
/// here), so the remote side is held at one element more throughout.
fn mismatch(span: usize) -> Comparison {
    Comparison::new(
        Aggregate::new(span, Fingerprint([1, 0, 0, 0])),
        Aggregate::new(span + 1, Fingerprint([2, 0, 0, 0])),
        0,
    )
}

/// Whether Negentropy's `splitRange` would ship an IdList for a range of `span` elements at
/// `buckets = b`: `numElems < buckets * 2`, transcribed rather than paraphrased.
fn negentropy_ships_an_idlist(span: usize, b: usize) -> bool {
    span < b * 2
}

#[test]
fn threshold_at_two_b_minus_one_reproduces_negentropys_cutoff_decision_for_decision() {
    for b in FAN_OUTS {
        let fan_out = FanOut::new(b);
        let policy = EnumerateBelowThreshold::new(2 * b - 1, fan_out);
        for span in 0..8 * b {
            let enumerates = policy.decide(mismatch(span)) == Decision::Enumerate;
            assert_eq!(
                enumerates,
                negentropy_ships_an_idlist(span, b),
                "b={b}, span={span}: this crate enumerates={enumerates}, negentropy ships an \
                 IdList={}",
                negentropy_ships_an_idlist(span, b),
            );
        }
    }
}

/// The paper's `t = 2b` is one rung wider than what Negentropy ships, and the rustdoc says the two
/// can only part on a single span. Anything more would make `t = 2b` a stand-in for their cutoff
/// only by coincidence of the workload.
#[test]
fn the_paper_and_negentropy_cutoffs_part_on_exactly_one_span() {
    for b in FAN_OUTS {
        let fan_out = FanOut::new(b);
        let negentropy = EnumerateBelowThreshold::new(2 * b - 1, fan_out);
        let paper = EnumerateBelowThreshold::new(2 * b, fan_out);
        let parting: Vec<usize> = (0..8 * b)
            .filter(|&span| negentropy.decide(mismatch(span)) != paper.decide(mismatch(span)))
            .collect();
        assert_eq!(
            parting,
            vec![2 * b],
            "b={b}: the two cutoffs must differ on the span of exactly 2b and nowhere else"
        );
        // And they part the way round the `t` ordering demands: the wider threshold enumerates,
        // the narrower one splits that same span into `b` children of `2` elements each.
        assert_eq!(
            negentropy.decide(mismatch(2 * b)),
            Decision::Split(SplitStride::for_fan_out(2 * b, fan_out))
        );
        assert_eq!(paper.decide(mismatch(2 * b)), Decision::Enumerate);
    }
}
