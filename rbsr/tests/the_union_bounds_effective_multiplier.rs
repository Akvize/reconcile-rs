// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! #356 follow-up, the soundness half: what multiplier does the skip rule's union bound actually
//! need, and can it be estimated from drives that only settled *because* of a collision?
//!
//! `oracle_dependent_split_vs_the_union_bound.rs` reports `mean(comparisons per drive) × 2⁻ʷ` as
//! "the same quantity the soundness argument bounds", and reads a zero-event control run as
//! confirming it. Two things are wrong with that reading, and this module measures both:
//!
//! - **The multiplier counts comparisons that cannot collide.** [`rbsr::Comparison::agrees`] tests
//!   the whole [`rsos::Aggregate`], size included, so a range whose two sides differ in size is
//!   refused at every width — no fingerprint collision can turn it into a SKIP. Under a
//!   count-balanced difference that is almost every comparison a drive makes.
//! - **The oracle-coupled arm conditions on a collider.** That policy stalls on 99.47% of drives,
//!   and a false SKIP prunes an active range — which is essentially the only way a stalled drive
//!   reaches a fixed point. Conditioning the rate on "settled" therefore selects for the very
//!   event being estimated.
//!
//! Run with:
//! `cargo test --release -p rbsr --test the_union_bounds_effective_multiplier -- --ignored --nocapture`

#![forbid(unsafe_code)]
#![cfg(reconcile_internal_testing)]

mod oracle_probe_harness;

use std::collections::HashSet;

use rand::rngs::StdRng;
use rand::SeedableRng;

use rbsr::RefinementPolicy;
use rbsr::{FingerprintDerivedSplit, FixedFanOut, SpanRelativeFingerprintSplit};

use oracle_probe_harness::*;

/// **The union bound's real multiplier.** The original module reports
/// `mean(comparisons per drive) × 2⁻ʷ` as "the same quantity the soundness argument bounds". Most
/// of those comparisons cannot produce a false SKIP at any width: under a count-balanced
/// difference, a range holding exactly one of the two swapped keys differs in `size`, and
/// [`Comparison::agrees`] tests the whole aggregate. This measures both multipliers — the loose
/// one and the collision-capable one — and then checks which the *observed* rate actually follows,
/// at widths where false convergence is common enough to measure instead of bound.
#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn the_false_convergence_rate_scales_with_a_multiplier_far_below_mean_comparisons() {
    // Ground truth for the two multipliers, taken at full width where a collision is negligible.
    let (mut comparisons, mut capable, mut drives) = (0u64, 0u64, 0u64);
    for trial in 0..20_000u64 {
        let mut rng = StdRng::seed_from_u64(trial);
        let (a_keys, b_keys) = balanced_swap(&mut rng, DRIVE_STORE_SIZE, 1);
        let a = NarrowStore::new(64, a_keys);
        let b = NarrowStore::new(64, b_keys);
        let observed = Observing::new(FixedFanOut::default());
        assert_eq!(drive(&a, &b, &observed).termination, Termination::Settled);
        comparisons += observed.comparisons.get();
        capable += observed.collision_capable.get();
        drives += 1;
    }
    let m_loose = comparisons as f64 / drives as f64;
    let m_tight = capable as f64 / drives as f64;
    println!(
        "\n-- multipliers at full width over {drives} drives --\n   \
         mean comparisons/drive (the reported multiplier): {m_loose:.3}\n   \
         mean collision-CAPABLE comparisons/drive:         {m_tight:.3}   \
         (ratio {:.1}x)",
        m_loose / m_tight
    );

    println!("\n-- rank-cut control, false convergence where it is observable --");
    println!(
        "   {:>5} | {:>10} | {:>11} | {:>11} | {:>11} | {:>13}",
        "w", "events", "observed", "tight pred.", "loose bound", "obs x 2^w"
    );
    for width in [8u32, 9, 10, 11, 12, 13, 14] {
        let trials = 200_000;
        let t = measure(width, trials, &FixedFanOut::default(), |_, _, _| {});
        let scale = (1u64 << width) as f64;
        let observed = t.false_convergence as f64 / t.settled as f64;
        println!(
            "   {width:>5} | {:>10} | {observed:>11.4e} | {:>11.4e} | {:>11.4e} | {:>13.3}",
            t.false_convergence,
            m_tight / scale,
            m_loose / scale,
            observed * scale,
        );
    }
    println!(
        "\n   The rightmost column is the multiplier the data implies. It should sit at the \
         collision-capable\n   count ({m_tight:.2}), not at the reported one ({m_loose:.2})."
    );
}

/// **Why a `d = 1` run cannot see a split policy at all.** #420's guard removed the censoring that
/// made the pre-guard soundness sample uninterpretable, and its own summary reads that as the
/// comparison being "well-powered for the first time". Termination was never what limited it.
///
/// Under a count-balanced difference only a range holding *both* swapped keys can falsely agree —
/// a range holding exactly one differs in `size`, which [`rbsr::Comparison::agrees`] rejects at
/// every width. At `d = 1` that is essentially the outer range alone, and the outer range is
/// compared **before any split decision exists**. So every policy must produce not merely the same
/// *rate* but the same *drives*, which is what this asserts: a set equality, not a rate comparison.
#[test]
fn at_one_element_of_difference_every_policy_posts_the_same_events() {
    // Sized for the pre-push budget (AGENTS.md §3, ~20 s for the whole tier): a narrow width and a
    // small store make events frequent enough to be decisive in a few thousand drives, and each
    // instance is built once and driven by all three policies rather than three times over.
    const TRIALS: u64 = 4_000;
    const WIDTH: u32 = 8;
    const STORE: usize = 128;

    let mut events: [HashSet<u64>; 3] = Default::default();
    for trial in 0..TRIALS {
        let mut rng = StdRng::seed_from_u64(trial);
        let (a_keys, b_keys) = balanced_swap(&mut rng, STORE, 1);
        let diff = symmetric_difference(&a_keys, &b_keys);
        let a = NarrowStore::new(WIDTH, a_keys);
        let b = NarrowStore::new(WIDTH, b_keys);
        let missed = [
            falsely_converges(&a, &b, &diff, FixedFanOut::default()),
            falsely_converges(&a, &b, &diff, FingerprintDerivedSplit),
            falsely_converges(&a, &b, &diff, SpanRelativeFingerprintSplit),
        ];
        for (set, &missed) in events.iter_mut().zip(missed.iter()) {
            if missed {
                set.insert(trial);
            }
        }
    }
    assert!(
        events[0].len() >= 5,
        "w={WIDTH} over {TRIALS} drives must produce events, or this proves nothing; got {}",
        events[0].len()
    );
    for (label, set) in [
        ("oracle-coupled span-independent", &events[1]),
        ("oracle-coupled span-relative", &events[2]),
    ] {
        assert_eq!(
            set, &events[0],
            "{label} must falsely converge on exactly the same drives as the rank-cut control: \
             at d = 1 the deciding comparison precedes every split decision"
        );
    }
}

/// Whether one drive of `policy` over this instance settles without surfacing either differing key.
fn falsely_converges<P: RefinementPolicy>(
    a: &NarrowStore,
    b: &NarrowStore,
    diff: &HashSet<u64>,
    policy: P,
) -> bool {
    let result = drive(a, b, &policy);
    if result.termination != Termination::Settled {
        return false;
    }
    let found: HashSet<u64> = result
        .enumerated
        .iter()
        .flat_map(|r| a.keys_in(r).into_iter().chain(b.keys_in(r)))
        .collect();
    !diff.is_subset(&found)
}

/// **The experiment the original module set out to run.** `oracle_dependent_split_vs_the_union_bound.rs`
/// asked whether coupling the split boundary to the summary oracle inflates the false-convergence
/// rate above what a rank-cut policy achieves. It could not answer that, because its probe stalled
/// on 99.47% of drives and the surviving 0.53% is a collider-selected sample (see
/// `settling_is_caused_by_the_collision_it_is_used_to_measure`).
///
/// [`SpanRelativeFingerprintSplit`] restores the population: `stride = 1 + limb mod (span − 1)`
/// still reads the fingerprint — the *cut points* an execution chooses are still a function of the
/// oracle the collision probability is stated over, so the union bound's premise is violated in
/// exactly the same way — but every SPLIT refines, so every drive settles. Head to head against the
/// rank-cut control at widths where the event is common enough to measure rather than bound.
///
/// The comparison to read is the **ratio** column: observed rate over
/// `mean(collision-capable comparisons) × 2⁻ʷ`. A rank-cut policy should sit at 1. An excess for
/// the oracle-coupled policy — the effect the union bound's premise exists to rule out — is a ratio
/// above it.
#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn a_span_relative_oracle_coupled_stride_measured_against_the_rank_cut_control() {
    let trials = 50_000;
    println!("\n-- soundness head to head, {trials} drives per cell --");
    println!(
        "   {:<30} | {:>3} | {:>2} | {:>7} | {:>6} | {:>11} | {:>10} | {:>6}",
        "policy", "w", "d", "settled", "events", "observed", "capable/drv", "ratio"
    );
    for swap_size in [1usize, 16] {
        for width in [8u32, 12] {
            for (label, (tally, capable)) in [
                (
                    "rank-cut FixedFanOut(16)",
                    measure_observed(width, trials, swap_size, FixedFanOut::default()),
                ),
                (
                    "oracle-coupled span-relative",
                    measure_observed(width, trials, swap_size, SpanRelativeFingerprintSplit),
                ),
            ] {
                let scale = (1u64 << width) as f64;
                let observed = tally.false_convergence as f64 / tally.settled as f64;
                println!(
                    "   {label:<30} | {width:>3} | {swap_size:>2} | {:>7} | {:>6} | {observed:>11.4e} \
                     | {capable:>10.3} | {:>6.3}",
                    tally.settled,
                    tally.false_convergence,
                    observed / (capable / scale),
                );
            }
        }
    }
    println!(
        "\n   At d = 1 the two policies must post the *same* event count: the outer range is the \
         only\n   collision-capable comparison a drive reliably makes, and it is compared before \
         any split\n   decision exists. Only d > 1 puts collision-capable comparisons inside the \
         descent, where a\n   split policy can influence them at all."
    );
}
