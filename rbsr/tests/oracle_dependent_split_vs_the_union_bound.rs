// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! #356: does a hash-derived split rule break the soundness bound rank-cut refinement is what
//! makes provable?
//!
//! The skip rule's soundness argument unions a per-comparison collision probability
//! (`2⁻ʷ`, [#355]'s arm A) over the ranges one reconciliation compares. That union is legal only
//! because the index set — which ranges get compared — is a deterministic function of the data
//! (cut by rank, `Select`); [`rbsr::Comparison`]'s docs state this as a law, and #352 made a
//! fingerprint-derived decision structurally unspellable from outside the crate for exactly this
//! reason. [`FingerprintDerivedSplit`] is the `internal-testing`-gated counter-example the law
//! anticipates: same enumeration cutoffs as [`FixedFanOut`], but the split stride is read off the
//! *local* fingerprint instead of a constant, so the sequence of ranges an execution compares is
//! now correlated with the very oracle the collision probability is stated over.
//!
//! ## Method
//!
//! Both policies are driven to a full fixed point (not [#355]'s single top-level comparison —
//! this needs the whole recursive descent, since that is what a split policy actually controls),
//! over the same non-adversarial one-element `balanced_swap` difference [#355]'s arm A uses, at
//! the same reduced widths `w ∈ {16, 24}` arm A measured. A false-convergence event is a drive
//! that reaches a fixed point (`active` empty) without ever enumerating a range containing either
//! swapped key — the observable signature of an incorrect SKIP, the same check
//! `pure_deletion_is_never_falsely_skipped.rs` uses.
//!
//! Every drive also counts total range comparisons made (`RoundOutcome::skipped` +
//! `enumerated` + `split` + `dropped_malformed`, summed across every round): the union bound this
//! module reports is `mean(comparisons per drive) × 2⁻ʷ`, the same quantity the soundness
//! argument unions over. [`FixedFanOut`] is the rank-cut control; a rate at or below its own bound
//! is what the argument predicts, and is the baseline an excess for the fingerprint-derived
//! policy would be an excess *of*.
//!
//! Trials are independently seeded from a recorded counter (`StdRng::seed_from_u64`), matching
//! [#355]. Run with:
//! `cargo test --release -p rbsr --features internal-testing --test oracle_dependent_split_vs_the_union_bound -- --ignored --nocapture`
//!
//! ## A second finding this method surfaced unasked: liveness, not just soundness
//!
//! A calibration run found the oracle-dependent policy fails to reach a fixed point within
//! `MAX_ROUNDS` on the overwhelming majority of drives — see `drive_with_policy` and
//! `ORACLE_DEPENDENT_TRIALS`'s docs for the mechanism (its stride is drawn from a fixed range
//! independent of the current span, so a range can land on a content-determined fixed point that
//! never shrinks). That was not what this issue asked to measure, and it makes the intended
//! soundness comparison hard to power — there are too few surviving drives to say much about the
//! false-convergence rate specifically. It is arguably the sharper demonstration of the same
//! underlying claim: making the split boundary a function of the summary oracle does not only
//! risk the soundness bound, it can break the protocol's basic termination guarantee.
//!
//! **#420 update, 2026-08-20**: the mechanism above is exactly what `ARCHITECTURE.md` §5
//! invariant 13 now guards against — `protocol_round_with_policy` converts a non-progressing
//! `Split` (`span() > 1`, stride `>= span()`) into an `Enumerate` before it can hang the driver,
//! whatever policy produced it. Re-run against the guarded driver, [`FingerprintDerivedSplit`]
//! now reaches a fixed point on 200,000/200,000 trials at both widths (up from 1,054 and 1,051
//! respectively). The finding above still describes what the *unguarded* mechanism does — that is
//! the [`RefinementPolicy`] progress law this policy violates — it is just no longer what a drive
//! through this crate's own driver does. With termination no longer the bottleneck, the soundness
//! comparison this module set out to make is well-powered for the first time: at `w=16`,
//! 3/200,000 events, 99% CI `[3.8e-6, 5.9e-5]`, comfortably under the reported bound `1.12e-3`; at
//! `w=24`, 0/200,000 events, 99% CI `[0, 3.3e-5]` against a reported bound of `4.39e-6` — zero
//! events, but (like the rank-cut control's own `w=24` row in #356) a CI built on zero successes
//! at this sample size is too wide to confirm that on its own.
//!
//! [#355]: https://github.com/Akvize/reconcile-rs/issues/355

#![forbid(unsafe_code)]
#![cfg(feature = "internal-testing")]

use std::collections::HashSet;
use std::ops::{Bound, RangeBounds};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use rbsr::{
    initial_ranges, protocol_round_with_policy, EnumerationRange, FingerprintDerivedSplit,
    FixedFanOut, RefinementPolicy, RsosView,
};
use rsos::{digest, Aggregate, Fingerprint};

/// Past this many rounds the drive is not converging; the cap turns a hang into a failure rather
/// than a timeout with no diagnostic — same budget as `pure_deletion_is_never_falsely_skipped.rs`.
const MAX_ROUNDS: usize = 128;

/// Low `width` bits set. `width` is always in `1..=64` here.
fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Two-sided 99% Wilson score interval for `successes` out of `trials`.
fn wilson_99_ci(successes: u64, trials: u64) -> (f64, f64) {
    assert!(trials > 0, "an interval needs at least one trial");
    let n = trials as f64;
    let p = successes as f64 / n;
    const Z: f64 = 2.575_829_303_548_901; // Phi^-1(0.995), the two-sided 99% quantile
    let z2 = Z * Z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = (Z / denom) * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((center - margin).max(0.0), (center + margin).min(1.0))
}

/// A store summarizing with `Σ mod 2^width` instead of `Σ mod 2^256` —
/// `aggregate_and_truncation_collision_rates.rs`'s `NarrowStore`, duplicated rather than shared:
/// this crate's own precedent for small test scaffolding.
struct NarrowStore {
    width: u32,
    keys: Vec<u64>,
}

impl NarrowStore {
    fn new(width: u32, mut keys: Vec<u64>) -> NarrowStore {
        keys.sort_unstable();
        keys.dedup();
        NarrowStore { width, keys }
    }

    fn span<R: RangeBounds<u64>>(&self, range: &R) -> (usize, usize) {
        let start = match range.start_bound() {
            Bound::Unbounded => 0,
            Bound::Included(k) => self.keys.partition_point(|x| x < k),
            Bound::Excluded(k) => self.keys.partition_point(|x| x <= k),
        };
        let end = match range.end_bound() {
            Bound::Unbounded => self.keys.len(),
            Bound::Included(k) => self.keys.partition_point(|x| x <= k),
            Bound::Excluded(k) => self.keys.partition_point(|x| x < k),
        };
        (start, end.max(start))
    }

    /// This store's own keys falling inside `range` — read back to check what a drive actually
    /// surfaced against ground truth, not the driver's own bookkeeping.
    fn keys_in(&self, range: &EnumerationRange<u64>) -> Vec<u64> {
        let (start, end) = self.span(range);
        self.keys[start..end].to_vec()
    }
}

impl RsosView<u64> for NarrowStore {
    fn size(&self) -> usize {
        self.keys.len()
    }

    fn aggregate<R: RangeBounds<u64>>(&self, range: R) -> Aggregate {
        let (start, end) = self.span(&range);
        let slice = &self.keys[start..end];
        let sum = slice.iter().fold(0u64, |acc, &key| {
            let limb = digest(&key).0[0] & mask(self.width);
            acc.wrapping_add(limb) & mask(self.width)
        });
        Aggregate::new(slice.len(), Fingerprint([sum, 0, 0, 0]))
    }

    fn rank(&self, z: &u64) -> usize {
        self.keys.partition_point(|x| x < z)
    }

    fn select(&self, r: usize) -> &u64 {
        &self.keys[r]
    }
}

/// A non-adversarial, count-balanced difference of `swap_size` elements over an `n`-key universe —
/// `aggregate_and_truncation_collision_rates.rs`'s `balanced_swap`, duplicated for the same reason
/// as `NarrowStore` above.
fn balanced_swap(rng: &mut StdRng, n: usize, swap_size: usize) -> (Vec<u64>, Vec<u64>) {
    let mut a: Vec<u64> = (0..n).map(|_| rng.gen()).collect();
    a.sort_unstable();
    a.dedup();
    let swap_size = swap_size.min(a.len());
    let mut b = a.clone();
    for _ in 0..swap_size {
        let idx = rng.gen_range(0..b.len());
        b.swap_remove(idx);
        let mut candidate: u64 = rng.gen();
        while a.contains(&candidate) || b.contains(&candidate) {
            candidate = rng.gen();
        }
        b.push(candidate);
    }
    (a, b)
}

/// One drive's outcome: every enumerated range on each side, plus the total number of range
/// comparisons made across every round — the quantity the soundness argument's union bound is
/// stated over.
type DriveResult<K> = (Vec<EnumerationRange<K>>, Vec<EnumerationRange<K>>, u64);

/// Reconcile `a` against `b` to a fixed point under `policy`, alternating which peer answers
/// (`pure_deletion_is_never_falsely_skipped.rs`'s `drive`, generalized over the policy).
///
/// `None` means `MAX_ROUNDS` was exhausted without reaching a fixed point — a **liveness**
/// failure, not a soundness one, and the reason this returns an `Option` rather than asserting
/// like `pure_deletion_is_never_falsely_skipped.rs`'s `drive` does: every shipped policy makes
/// this unreachable (`Decision::Split`'s docs — a stride at or above `span` still terminates
/// *because the peer refines it*, which every rank-cut policy here does for a small span), but
/// that termination argument assumes progress on at least one side each round, a property this
/// module's whole point is that an oracle-dependent stride is not obliged to keep.
///
/// **#420 update**: since `protocol_round_with_policy` now forces progress on `span() > 1`
/// itself (`ARCHITECTURE.md` §5 invariant 13), `None` is unreachable for *any* policy driven
/// through this crate's own driver, not only the shipped ones — this module's own measurement
/// below is what confirms that empirically for [`FingerprintDerivedSplit`]. Kept as an `Option`
/// regardless: it is what caught #356's finding before that guard existed.
fn drive_with_policy<K: Clone + Ord, B: RsosView<K>, P: RefinementPolicy>(
    a: &B,
    b: &B,
    policy: &P,
) -> Option<DriveResult<K>> {
    let mut active = initial_ranges(a);
    let mut responder = b;
    let mut advertiser = a;
    let mut a_enumerations = Vec::new();
    let mut b_enumerations = Vec::new();
    let mut comparisons = 0u64;
    let mut rounds = 0;

    let mut responder_is_b = true;
    while !active.is_empty() {
        if rounds >= MAX_ROUNDS {
            return None;
        }
        let mut children = Vec::new();
        let mut enumerations = Vec::new();
        let outcome =
            protocol_round_with_policy(responder, policy, active, &mut children, &mut enumerations);
        comparisons += (outcome.skipped()
            + outcome.enumerated()
            + outcome.split()
            + outcome.dropped_malformed()) as u64;
        if responder_is_b {
            b_enumerations.extend(enumerations);
        } else {
            a_enumerations.extend(enumerations);
        }
        active = children;
        rounds += 1;
        std::mem::swap(&mut responder, &mut advertiser);
        responder_is_b = !responder_is_b;
    }
    Some((a_enumerations, b_enumerations, comparisons))
}

/// `a`'s and `b`'s keys that appear in exactly one of the two — the two keys `balanced_swap(_, _,
/// 1)` touches, computed from the generated sets rather than threaded through as extra state.
fn symmetric_difference(a: &[u64], b: &[u64]) -> HashSet<u64> {
    let a: HashSet<u64> = a.iter().copied().collect();
    let b: HashSet<u64> = b.iter().copied().collect();
    a.symmetric_difference(&b).copied().collect()
}

/// One width's measurement for one policy: false-convergence events and non-terminating drives
/// out of `trials`, plus mean comparisons per drive **among the drives that terminated** — a
/// non-terminating drive has no comparison count the union bound's method applies to.
struct Measurement {
    events: u64,
    non_terminating: u64,
    trials: u64,
    mean_comparisons: f64,
}

fn false_convergence_at<P: RefinementPolicy>(
    width: u32,
    n: usize,
    trials: u64,
    policy: &P,
) -> Measurement {
    let mut events = 0u64;
    let mut non_terminating = 0u64;
    let mut total_comparisons = 0u64;
    let mut terminated = 0u64;
    for trial in 0..trials {
        let mut rng = StdRng::seed_from_u64(trial);
        let (a_keys, b_keys) = balanced_swap(&mut rng, n, 1);
        let diff = symmetric_difference(&a_keys, &b_keys);
        let a = NarrowStore::new(width, a_keys);
        let b = NarrowStore::new(width, b_keys);

        let Some((a_enum, b_enum, comparisons)) = drive_with_policy(&a, &b, policy) else {
            non_terminating += 1;
            continue;
        };
        terminated += 1;
        total_comparisons += comparisons;

        let found: HashSet<u64> = a_enum
            .iter()
            .chain(b_enum.iter())
            .flat_map(|r| a.keys_in(r).into_iter().chain(b.keys_in(r)))
            .collect();
        if !diff.is_subset(&found) {
            events += 1;
        }
    }
    Measurement {
        events,
        non_terminating,
        trials,
        mean_comparisons: if terminated == 0 {
            0.0
        } else {
            total_comparisons as f64 / terminated as f64
        },
    }
}

/// `(width, target expected events)` — mirrors `aggregate_and_truncation_collision_rates.rs`'s
/// `EXPECTED_EVENTS`, but sized against this module's *measured* rate rather than the raw `2⁻ʷ`
/// arm A's single-comparison harness predicts: a whole drive's rate is `mean_comparisons × 2⁻ʷ`,
/// roughly `50×` higher here (calibrated at `w=16` under `FixedFanOut`, `DRIVE_STORE_SIZE`
/// elements) — see `trial_count_for`. The shipped width gets a solid sample, the next one a
/// smaller but non-zero one, keeping this module's run time in the minutes rather than the
/// cluster-job range `w = 32` would need.
const EXPECTED_EVENTS: [(u32, u64); 2] = [(16, 60), (24, 3)];

/// Rough comparisons-per-drive at `DRIVE_STORE_SIZE`, measured once under `FixedFanOut` — only
/// used to size `trials` so the target event count above is reached with a sensible sample; the
/// bound each test *reports* comes from that run's own measured `mean_comparisons`, not this
/// constant.
const APPROX_COMPARISONS_PER_DRIVE: f64 = 50.0;

/// Trials needed to expect roughly `target_events` false-convergence events at `width`, given
/// [`APPROX_COMPARISONS_PER_DRIVE`]'s rough per-drive rate `mean_comparisons × 2⁻ʷ`.
fn trial_count_for(width: u32, target_events: u64) -> u64 {
    ((target_events as f64 * (1u64 << width) as f64) / APPROX_COMPARISONS_PER_DRIVE).ceil() as u64
}

/// Universe size for the drive — large enough to force several SPLIT rounds under both policies
/// (`FixedFanOut`'s default `b = 16` alone would resolve a smaller store in one round), small
/// enough that `trials` in the low millions stays affordable.
const DRIVE_STORE_SIZE: usize = 512;

/// Prints one policy's measurement at one width: the false-convergence rate among drives that
/// reached a fixed point (99% CI against the union bound `mean_comparisons × 2⁻ʷ`, that mean
/// itself only over terminating drives), plus the non-termination rate as its own figure — see
/// `drive_with_policy`'s docs for why the two are reported separately.
fn report(label: &str, width: u32, m: Measurement) {
    let settled = m.trials - m.non_terminating;
    let bound = m.mean_comparisons / (1u64 << width) as f64;
    if settled == 0 {
        println!(
            "{label} w={width}: {}/{} drives never reached a fixed point — no false-convergence \
             rate to report",
            m.non_terminating, m.trials
        );
        return;
    }
    let (lo, hi) = wilson_99_ci(m.events, settled);
    println!(
        "{label} w={width}: {}/{settled} events (of {} trials, {} never reached a fixed point), \
         99% CI=[{lo:.3e}, {hi:.3e}], mean comparisons/drive={:.2}, union bound={bound:.3e}",
        m.events, m.trials, m.non_terminating, m.mean_comparisons
    );
}

#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn rank_cut_false_convergence_rate() {
    let policy = FixedFanOut::default();
    for (width, target_events) in EXPECTED_EVENTS {
        let trials = trial_count_for(width, target_events);
        let measurement = false_convergence_at(width, DRIVE_STORE_SIZE, trials, &policy);
        report("rank-cut (FixedFanOut)", width, measurement);
    }
}

/// Fixed trial budget for the oracle-dependent policy, **not** derived from
/// [`trial_count_for`]. That sizing assumes drives converge at roughly `FixedFanOut`'s rate;
/// [`FingerprintDerivedSplit`] overwhelmingly does not (its stride is drawn from a fixed `1..=32`
/// range independent of the current span, so once a range's span drops below 32 — a couple of
/// levels in — a "stride ≥ span" no-progress SPLIT is likely on *both* sides most rounds, and
/// since the stride is a pure function of unchanging content, a range that lands there can loop
/// forever, not just slowly). A calibration run at `w=16` found under 1% of drives reach a fixed
/// point inside `MAX_ROUNDS`. Sized for a meaningful termination-rate CI at that base rate; the
/// width has no effect on it (see this module's docs) and only a handful of trials among the
/// survivors will ever be at risk of a *false-convergence* event on top of that, so this is not
/// sized to catch one.
///
/// **#420 update**: that calibration predates the driver-side progress guard this policy's own
/// mechanism led to (`ARCHITECTURE.md` §5 invariant 13) — re-run against the guarded driver, both
/// widths terminate 200,000/200,000. The count stays fixed at this value regardless: it was never
/// really chosen for termination odds, and now doubles as the sample size the false-convergence
/// comparison itself needs.
const ORACLE_DEPENDENT_TRIALS: u64 = 200_000;

#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn oracle_dependent_split_false_convergence_rate() {
    let policy = FingerprintDerivedSplit;
    for width in [16, 24] {
        let measurement =
            false_convergence_at(width, DRIVE_STORE_SIZE, ORACLE_DEPENDENT_TRIALS, &policy);
        report("oracle-dependent split", width, measurement);
    }
}

/// Byte-sequence determinism (`SOTA.md` §4.4): the trial generator is a pure function of its
/// seed, matching `aggregate_and_truncation_collision_rates.rs`'s
/// `same_seed_reproduces_byte_identical_trials`.
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
        assert_eq!(
            run(seed),
            run(seed),
            "seed {seed}: two runs of the same trial produced different byte sequences"
        );
    }
}

/// A sanity floor: both policies must actually disagree in *how* they split, or the comparison
/// below is vacuous. Not `#[ignore]`d — cheap, and a regression here would silently turn the
/// measured tests above into two copies of the same experiment.
#[test]
fn the_two_policies_choose_different_strides_on_the_same_comparison() {
    use rbsr::Comparison;

    let mut same_stride_always = true;
    for seed in 0..64u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let (a_keys, _) = balanced_swap(&mut rng, DRIVE_STORE_SIZE, 1);
        let a = NarrowStore::new(16, a_keys);
        let local = a.aggregate(..);
        let remote = Aggregate::new(local.size() + 1, Fingerprint([1, 2, 3, 4]));
        let comparison = Comparison::new(local, remote, 0);

        let rbsr::Decision::Split(rank_cut_stride) = FixedFanOut::default().decide(comparison)
        else {
            continue;
        };
        let rbsr::Decision::Split(oracle_stride) = FingerprintDerivedSplit.decide(comparison)
        else {
            continue;
        };
        if rank_cut_stride != oracle_stride {
            same_stride_always = false;
            break;
        }
    }
    assert!(
        !same_stride_always,
        "FixedFanOut and FingerprintDerivedSplit must diverge on at least one comparison, or the \
         drives above are not actually exercising different policies"
    );
}
