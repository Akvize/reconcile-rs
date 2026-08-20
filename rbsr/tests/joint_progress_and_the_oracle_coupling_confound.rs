// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! #356 follow-up: `oracle_dependent_split_vs_the_union_bound.rs` reports that
//! [`FingerprintDerivedSplit`] fails to reach a fixed point on ~99.5% of drives and reads that as
//! "coupling the split boundary to the summary oracle breaks termination". That reading is not
//! supported by that experiment, because the probe changed **two** things at once against its
//! [`FixedFanOut`] control:
//!
//! 1. its stride is read off the local **fingerprint** (oracle-coupled), and
//! 2. its stride is drawn from a fixed `1..=32` support **independent of the span**.
//!
//! Only (2) appears in the stall mechanism. This module runs the missing cells of that 2×2 —
//! the `cfg(reconcile_internal_testing)` probe policies's four probes — and separates three questions the original conflated:
//!
//! - **Is oracle coupling necessary for the stall?** [`ConstantStrideSplit`] and
//!   [`SpanHashedStrideSplit`] are oracle-*independent* (a constant, and a function of the span:
//!   both "functions of the data alone", which is exactly what the soundness union bound requires)
//!   yet span-independent in magnitude — see `termination_is_decided_by_span_relativity`.
//! - **Is oracle coupling sufficient for it?** [`SpanRelativeFingerprintSplit`] reads the same
//!   fingerprint limb but reduces it `mod (span − 1)`, so every SPLIT still refines.
//! - **Is the stall a property of RBSR, or of this crate's enumeration cutoffs?**
//!   `shared_cutoffs` enumerates only at `span <= 1`; Algorithm 1 of arXiv:2603.19820 as
//!   published enumerates at `span <= t`. `EnumerateBelow` re-runs the oracle-coupled probe under
//!   the paper's own cutoff — see `the_stall_needs_a_split_region_the_stride_can_cover`.
//!
//! **Driver-side guard (#420, landed).** `protocol_round_with_policy` now converts a `Split` with
//! `stride >= span` at `span > 1` into an `Enumerate`, so no policy can stall a drive through this
//! crate's own driver any more, and the pre-guard settle rates quoted above are history (`SOTA.md`
//! §2.3 records them as such). That guard keys on **span-relativity**, which is this module's whole
//! point arrived at independently — a guard keyed on "reads the fingerprint" would have caught
//! neither oracle-independent probe.
//!
//! So the live quantity here is the **policy-level** one, which the guard does not touch: how many
//! `Split` decisions a policy returns that would not narrow their range. Pre-guard that count was
//! the stall; post-guard it is the stall's price, one forced IDLIST each. Every claim below is
//! stated in it, and `the_progress_guard_settles_the_two_cases_its_own_tests_miss` pins the guard
//! against the two configurations #420's own tests do not cover.
//!
//! Two method upgrades over the original module:
//!
//! - **A stall is *proved*, not inferred from a round cap.** A drive is a deterministic map on
//!   `(responder parity, active family)`; [`drive`] records every state it visits, so a repeat is
//!   a proof the drive never terminates, rather than evidence it had not yet at round 128.
//! - **The false-convergence rate is measured where it is observable** (`w ∈ 8..=16`) and fitted,
//!   rather than bounded by a zero-event run at `w ∈ {16, 24}`. The fitted multiplier is the
//!   *effective* number of collision-capable comparisons per drive, which is what the union bound
//!   should be stated over — `mean(comparisons)` counts every comparison, and a comparison whose
//!   two sides differ in `size` cannot falsely agree at any width.
//!
//! Run with:
//! `cargo test --release -p rbsr --test joint_progress_and_the_oracle_coupling_confound -- --ignored --nocapture`

#![forbid(unsafe_code)]
#![cfg(reconcile_internal_testing)]

mod oracle_probe_harness;

use rand::rngs::StdRng;
use rand::SeedableRng;

use rbsr::{
    ConstantStrideSplit, FingerprintDerivedSplit, FixedFanOut, RefinementPolicy,
    SpanHashedStrideSplit, SpanRelativeFingerprintSplit, STRIDE_SPREAD,
};

use oracle_probe_harness::*;

// ---------------------------------------------------------------------------------------------
// Scaffolding, duplicated from `oracle_dependent_split_vs_the_union_bound.rs` per this crate's
// precedent for small test scaffolding (that module duplicates it from
// `aggregate_and_truncation_collision_rates.rs` for the same reason).
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// The measurements
// ---------------------------------------------------------------------------------------------

/// Trials per cell. The rates these separate are all far from 0 and 1, so this is sized for a
/// tight interval on each, not for a rare event.
const TRIALS: u64 = 20_000;

/// Non-progressing splits per drive, their share of all comparisons, and the settle rate, for one
/// policy pair. The first is policy-level and therefore the same against a guarded or an unguarded
/// driver; the third is what the guard buys.
fn cost<A: RefinementPolicy, B: RefinementPolicy>(
    width: u32,
    trials: u64,
    swap_size: usize,
    store: usize,
    policy_a: A,
    policy_b: B,
) -> (f64, f64, f64) {
    let (a_obs, b_obs) = (Observing::new(policy_a), Observing::new(policy_b));
    let mut settled = 0u64;
    for trial in 0..trials {
        let mut rng = StdRng::seed_from_u64(trial);
        let (a_keys, b_keys) = balanced_swap(&mut rng, store, swap_size);
        let a = NarrowStore::new(width, a_keys);
        let b = NarrowStore::new(width, b_keys);
        if drive_pair(&a, &b, &a_obs, &b_obs).termination == Termination::Settled {
            settled += 1;
        }
    }
    let non_progressing = (a_obs.non_progressing.get() + b_obs.non_progressing.get()) as f64;
    let comparisons = (a_obs.comparisons.get() + b_obs.comparisons.get()).max(1) as f64;
    (
        non_progressing / trials as f64,
        non_progressing / comparisons,
        100.0 * settled as f64 / trials as f64,
    )
}

/// One policy against itself.
fn solo_cost<P: RefinementPolicy + Copy>(width: u32, trials: u64, policy: P) -> (f64, f64, f64) {
    cost(width, trials, 1, DRIVE_STORE_SIZE, policy, policy)
}

fn row(label: &str, (per_drive, share, settled): (f64, f64, f64)) {
    println!(
        "   {label:<34} | {per_drive:>11.3} | {:>11.1}% | {settled:>8.2}%",
        100.0 * share
    );
}

fn header(what: &str) {
    println!(
        "   {:<34} | {:>11} | {:>12} | {:>9}",
        what, "per drive", "% of splits", "settled"
    );
}

/// **The confound, in the quantity #420's guard keys on.** Non-progressing `Split` decisions —
/// pre-guard the stall, post-guard one forced IDLIST each — sorted over the 2×2 of (reads the
/// fingerprint) × (stride relative to the span).
#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn non_progress_is_decided_by_span_relativity_not_by_oracle_coupling() {
    let w = 16;
    println!(
        "\n-- non-progressing SPLITs, {TRIALS} drives per policy, n={DRIVE_STORE_SIZE}, d=1 --"
    );
    header("policy");
    println!("   -- stride relative to the span --");
    row(
        "  FixedFanOut(16) [shipped]",
        solo_cost(w, TRIALS, FixedFanOut::default()),
    );
    row(
        "  SpanRelativeFingerprintSplit  ORACLE",
        solo_cost(w, TRIALS, SpanRelativeFingerprintSplit),
    );
    println!("   -- stride independent of the span --");
    row(
        "  SpanHashedStrideSplit",
        solo_cost(w, TRIALS, SpanHashedStrideSplit),
    );
    for stride in [2usize, 8, STRIDE_SPREAD as usize] {
        row(
            &format!("  ConstantStrideSplit({stride})"),
            solo_cost(w, TRIALS, ConstantStrideSplit::per_child(stride)),
        );
    }
    row(
        "  FingerprintDerivedSplit       ORACLE",
        solo_cost(w, TRIALS, FingerprintDerivedSplit),
    );
    println!(
        "\n   The two policies that refine every split cost the guard nothing, one of them while\n   \
         reading the fingerprint. The rest are in one band whether they read it or not."
    );
}

/// **The threshold that removes the decision.** The same oracle-coupled probe under Algorithm 1's
/// published `t` instead of `shared_cutoffs`' `span <= 1`.
#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn the_enumeration_threshold_that_removes_the_non_progressing_split() {
    let w = 16;
    println!("\n-- oracle-coupled stride under Algorithm 1's `t`, {TRIALS} drives --");
    header("threshold");
    for threshold in [0usize, 1, 8, 31, 32, 64] {
        let policy = EnumerateBelow {
            threshold,
            inner: FingerprintDerivedSplit,
        };
        row(
            &format!("  EnumerateBelow(t={threshold}) + fp stride"),
            cost(w, TRIALS, 1, DRIVE_STORE_SIZE, &policy, &policy),
        );
    }
    println!(
        "\n   Zero from t = {STRIDE_SPREAD} on, where the split region `span > t` can no longer\n   \
         overlap the stride's `1..={STRIDE_SPREAD}` support."
    );
}

/// **One deviant peer, and instance shape.** A policy is never negotiated, so the mixed pair is the
/// realistic deployment case; and a drive's exposure grows with the number of differing ranges.
#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn the_cost_under_mixed_peers_and_instance_shape() {
    let (w, shipped, deviant) = (16, FixedFanOut::default(), FingerprintDerivedSplit);
    println!("\n-- mixed policy pairs, {TRIALS} drives, n={DRIVE_STORE_SIZE}, d=1 --");
    header("peer A / peer B");
    row(
        "  fp stride / fp stride",
        cost(w, TRIALS, 1, DRIVE_STORE_SIZE, deviant, deviant),
    );
    row(
        "  fp stride / FixedFanOut",
        cost(w, TRIALS, 1, DRIVE_STORE_SIZE, deviant, shipped),
    );
    row(
        "  FixedFanOut / fp stride",
        cost(w, TRIALS, 1, DRIVE_STORE_SIZE, shipped, deviant),
    );
    row(
        "  const(32) / FixedFanOut",
        cost(
            w,
            TRIALS,
            1,
            DRIVE_STORE_SIZE,
            ConstantStrideSplit::per_child(32),
            shipped,
        ),
    );

    println!("\n-- instance shape, oracle-coupled probe, 5,000 drives per cell --");
    header("n / d");
    for store in [64usize, 512, 4_096] {
        for d in [1usize, 4, 16, 64] {
            row(
                &format!("  n={store}, d={d}"),
                cost(w, 5_000, d, store, deviant, deviant),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Non-ignored properties — these assert, they do not print
// ---------------------------------------------------------------------------------------------

/// [`drive`]'s verdict must be *reproducible*: it is only a proof of non-termination because the
/// drive is a deterministic map on its state, and that determinism is what this pins.
#[test]
fn a_proved_stall_is_deterministic() {
    for seed in 0..64u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let (a_keys, b_keys) = balanced_swap(&mut rng, 64, 1);
        let a = NarrowStore::new(16, a_keys);
        let b = NarrowStore::new(16, b_keys);
        let first = drive(&a, &b, &FingerprintDerivedSplit).termination;
        let second = drive(&a, &b, &FingerprintDerivedSplit).termination;
        assert_eq!(
            first, second,
            "seed {seed}: the same drive reached two verdicts"
        );
    }
}

/// **The confound, asserted.** The quantity that decided termination before #420's guard, and
/// prices it after, is non-progressing `Split` decisions — and it sorts on whether the stride is
/// relative to the span, not on whether the policy reads the fingerprint.
///
/// Policy-level, so it holds identically against a guarded and an unguarded driver: `Observing`
/// counts what the policy *returned*, before the driver decides what to do with it.
#[test]
fn non_progress_sorts_on_span_relativity_not_on_reading_the_fingerprint() {
    let reads_fingerprint = solo_cost(16, 256, FingerprintDerivedSplit).0;
    let ignores_it = solo_cost(
        16,
        256,
        ConstantStrideSplit::per_child(STRIDE_SPREAD as usize),
    )
    .0;
    let also_ignores_it = solo_cost(16, 256, SpanHashedStrideSplit).0;
    for (label, cost) in [
        ("FingerprintDerivedSplit", reads_fingerprint),
        ("ConstantStrideSplit(32)", ignores_it),
        ("SpanHashedStrideSplit", also_ignores_it),
    ] {
        assert!(
            cost > 1.0,
            "{label}: a span-independent stride must return non-progressing splits; got {cost} \
             per drive"
        );
    }
    // The two policies whose stride is relative to the span return none at all -- including the
    // one that reads the very fingerprint the soundness bound is stated over.
    for (label, cost) in [
        (
            "FixedFanOut(16)",
            solo_cost(16, 256, FixedFanOut::default()).0,
        ),
        (
            "SpanRelativeFingerprintSplit",
            solo_cost(16, 256, SpanRelativeFingerprintSplit).0,
        ),
    ] {
        assert_eq!(
            cost, 0.0,
            "{label}: a span-relative stride refines every split, so the guard must never fire"
        );
    }
    // Reading the fingerprint is not what separates them: an oracle-independent stride drawn from
    // the same support is in the same band as the oracle-coupled one.
    let ratio = reads_fingerprint / ignores_it;
    assert!(
        (0.5..2.0).contains(&ratio),
        "the oracle-coupled and constant strides must sit in the same band, not an order apart; \
         {reads_fingerprint} vs {ignores_it}"
    );
}

/// **Oracle coupling is not sufficient for non-progress.** `1 + limb mod (span − 1)` reads the
/// same fingerprint limb [`FingerprintDerivedSplit`] does — the cut points an execution chooses are
/// still a function of the oracle the collision probability is stated over — yet lands in
/// `1..span−1`, so every `Split` refines.
///
/// Asserted on the drive *and* on the policy: settling alone would be uninformative now that the
/// driver guards, so this also pins that the guard never had to fire.
#[test]
fn an_oracle_coupled_span_relative_stride_settles_without_the_guard_firing() {
    for seed in 0..256u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let (a_keys, b_keys) = balanced_swap(&mut rng, DRIVE_STORE_SIZE, 1);
        let a = NarrowStore::new(16, a_keys);
        let b = NarrowStore::new(16, b_keys);
        let observed = Observing::new(SpanRelativeFingerprintSplit);
        assert_eq!(
            drive(&a, &b, &observed).termination,
            Termination::Settled,
            "seed {seed}: a span-relative stride refines every SPLIT, so the drive must settle"
        );
        assert_eq!(
            observed.non_progressing.get(),
            0,
            "seed {seed}: it must settle on its own merits, with no split for the guard to convert"
        );
    }
}

/// **The failure was a property of the enumeration cutoff, not of RBSR.** `shared_cutoffs`
/// enumerates only at `span <= 1`; Algorithm 1 of arXiv:2603.19820 as published enumerates at
/// `span <= t`. Since the probe's stride support is `1..=STRIDE_SPREAD`, a `t` at or above that
/// support leaves it no span it can fail to refine — the split region is `span > t >= stride`.
///
/// Pinned on the policy, so it says the threshold removes the *decision* rather than that the
/// driver's guard cleans up after it.
#[test]
fn the_papers_own_enumeration_threshold_removes_the_non_progressing_split() {
    let policy = EnumerateBelow {
        threshold: STRIDE_SPREAD as usize,
        inner: FingerprintDerivedSplit,
    };
    for seed in 0..256u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let (a_keys, b_keys) = balanced_swap(&mut rng, DRIVE_STORE_SIZE, 1);
        let a = NarrowStore::new(16, a_keys);
        let b = NarrowStore::new(16, b_keys);
        let observed = Observing::new(&policy);
        assert_eq!(
            drive(&a, &b, &observed).termination,
            Termination::Settled,
            "seed {seed}: t = {STRIDE_SPREAD} leaves the stride no span it can fail to refine"
        );
        assert_eq!(
            observed.non_progressing.get(),
            0,
            "seed {seed}: t = {STRIDE_SPREAD} must leave no non-progressing split to guard"
        );
    }
}

/// **The guard, pinned against what its own tests miss.** #420 covers the shipped policies and one
/// adversarial `NeverNarrows`, both with the same policy on each peer. Neither of this module's
/// two findings is in that set: a non-refining policy that reads *no* fingerprint, and a **mixed**
/// pair where only one peer deviates — which matters because a policy is never negotiated, and
/// because `shared_cutoffs` answers a lone local element with a deliberate non-progressing
/// `Split(ONE)` that assumes the peer will cut instead.
#[test]
fn the_progress_guard_settles_the_two_cases_its_own_tests_miss() {
    let shipped = FixedFanOut::default();
    let deviant = ConstantStrideSplit::per_child(STRIDE_SPREAD as usize);
    for seed in 0..256u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let (a_keys, b_keys) = balanced_swap(&mut rng, DRIVE_STORE_SIZE, 1);
        let a = NarrowStore::new(16, a_keys);
        let b = NarrowStore::new(16, b_keys);
        for (case, termination) in [
            (
                "oracle-independent constant stride",
                drive(&a, &b, &deviant).termination,
            ),
            (
                "oracle-independent span-hashed stride",
                drive(&a, &b, &SpanHashedStrideSplit).termination,
            ),
            (
                "deviant peer A only",
                drive_pair(&a, &b, &deviant, &shipped).termination,
            ),
            (
                "deviant peer B only",
                drive_pair(&a, &b, &shipped, &deviant).termination,
            ),
        ] {
            assert_eq!(
                termination,
                Termination::Settled,
                "seed {seed} ({case}): the driver's progress guard must settle this"
            );
        }
    }
}

/// Byte-sequence determinism (`SOTA.md` §4.4), matching the sibling modules.
#[test]
fn same_seed_reproduces_byte_identical_trials() {
    fn run(seed: u64) -> Vec<u8> {
        let mut rng = StdRng::seed_from_u64(seed);
        let (a_keys, b_keys) = balanced_swap(&mut rng, 32, 1);
        let mut bytes = Vec::new();
        for key in a_keys.iter().chain(b_keys.iter()) {
            bytes.extend_from_slice(&key.to_le_bytes());
        }
        bytes
    }
    for seed in 0..8u64 {
        assert_eq!(run(seed), run(seed), "seed {seed}");
    }
}
