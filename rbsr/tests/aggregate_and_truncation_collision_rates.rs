// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Arms A and B of [#355] — a *measured* (not merely predicted) false-convergence rate for each of
//! the two layers `wagner_false_convergence.rs`'s module docs and
//! `pure_deletion_is_never_falsely_skipped.rs` separate: L1 (aggregate collision, width `w`) and L2
//! (comparison-map collision, only nonzero for a hypothetical truncating map — `f_p = id`, the
//! shipped map, predicts L2 = 0, pinned by `pure_deletion_is_never_falsely_skipped.rs`).
//!
//! ## Scope actually run here
//!
//! | | swept over | why it stops there |
//! |---|---|---|
//! | Arm A rate (L1) | `w ∈ {16, 24}` | `w = 32` needs ≈`4·10⁹` lift evals for one event ([#355]'s cost formula) — a cluster job |
//! | Arm B rate (L2) | `τ ∈ {16, 24}`, real unreduced `w = 256` | same formula, `τ = 32` |
//! | `n`-scaling discriminator | `n ∈ {20, 200, 2 000}` at the shipped width/`τ` of 16 | the issue's `{10⁴,10⁶,10⁸}` scales the *content size of every trial*, not just the trial count — a `10⁸`-element store is cluster-scale on its own, independent of how many trials sample it |
//!
//! `w = 32`, `τ = 32`, and the issue's literal `n` values remain open — this module narrows scope,
//! it does not close [#355].
//!
//! **The `n`-scaling sweep here is necessarily inconclusive on the one thing it is meant to
//! discriminate.** Both arms measure a *single* top-level comparison, so `n` only changes what one
//! comparison is computed over — it never changes how many ranges get compared. The predicted
//! divergence (L1 flat vs. L2 rising in `log n`) is a claim about a *whole reconciliation*'s
//! union-bounded event count across the `O(log n)` ranges a real drive compares, which this sweep
//! does not exercise. What it below actually shows — both arms' single-comparison rate holding
//! roughly constant across `n` — is consistent with L1's prediction and silent on L2's: measuring
//! the real rise needs a multi-round drive at each `n`, deferred with the rest of the cluster-scale
//! work above.
//!
//! ## Method
//!
//! Both arms measure a **single** top-level comparison
//! (`wagner_false_convergence.rs`'s `declares_convergence` pattern), not a multi-round drive: a
//! drive's cost is dominated by store size, exactly the axis the `n`-scaling sweep holds fixed per
//! configuration — the single-comparison rate is the quantity the cost formula prices.
//!
//! **Arm A** compares two same-size stores related by a random, non-adversarial one-element swap
//! through [`rsos::digest`] reduced mod `2^w` (the homomorphism `wagner_false_convergence.rs`'s
//! `NarrowStore` uses) — the *balanced* case `rbsr/tests/balance_under_position_map.rs` marks as
//! practically dangerous, since count agreement alone cannot save it.
//!
//! **Arm B** has no width to reduce: `rsos`'s shipped combiner has no pluggable comparison map, so
//! nothing in the driver ever computes `trunc_τ(H(aggregate))`. To measure it at all,
//! [`HashCompareStore`] folds a genuinely-differing `(size, Σ)` pair (a pure-deletion difference, at
//! the real, unreduced width — `H` never sees a narrowed `Σ`, so L1 is invisible by construction, not
//! by extrapolation) through `H = rsos::digest` and republishes the low `τ` bits as *both* halves of
//! the [`Aggregate`] the driver sees. Driver-level equality on that doctored pair is then exactly
//! equality of the `τ`-bit digest, so any resulting SKIP is an L2 event with no L1 admixture.
//!
//! **Trial counts** target a few dozen events at the shipped width and fewer (not zero) at the next
//! one, staying inside this module's compute budget — see `EXPECTED_EVENTS`. Every trial is
//! independently seeded from a recorded counter (`StdRng::seed_from_u64`), matching
//! `pure_deletion_is_never_falsely_skipped.rs`; [`same_seed_reproduces_byte_identical_trials`] pins
//! that determinism, the harness property `SOTA.md` §4.4 asks for.
//!
//! Results are *reported*, not asserted against the prediction: a handful of observed events gives a
//! wide interval, and gating a security-relevant measurement on a noisy count would make this module
//! flaky rather than informative. Run with:
//! `cargo test --release -p rbsr --test aggregate_and_truncation_collision_rates -- --ignored --nocapture`
//!
//! [#355]: https://github.com/Akvize/reconcile-rs/issues/355

#![forbid(unsafe_code)]

use std::ops::{Bound, RangeBounds};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use rbsr::{initial_ranges, protocol_round, RangeAggregate, RsosView};
use rsos::{digest, Aggregate, Fingerprint};

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

/// One top-level comparison: does `b` SKIP against `a`'s advertisement?
fn declares_convergence<K: Clone, S: RsosView<K>>(a: &S, b: &S) -> bool {
    let active: Vec<RangeAggregate<K>> = initial_ranges(a);
    let mut children = Vec::new();
    let mut enumerations = Vec::new();
    let outcome = protocol_round(b, active, &mut children, &mut enumerations);
    children.is_empty() && enumerations.is_empty() && outcome.skipped() == 1
}

// -------------------------------------------------------------------------------------------
// Arm A — the aggregate layer (L1)
// -------------------------------------------------------------------------------------------

/// A store summarizing with `Σ mod 2^width` instead of `Σ mod 2^256` —
/// `wagner_false_convergence.rs`'s `NarrowStore`, duplicated rather than shared: this file's own
/// precedent for small test scaffolding, matching `pure_deletion_is_never_falsely_skipped.rs`.
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

/// A non-adversarial, count-balanced difference of `swap_size` elements over an `n`-key universe:
/// remove `swap_size` random keys, add `swap_size` different random keys — the practically
/// dangerous case `rbsr/tests/balance_under_position_map.rs` marks (every range balanced, so count
/// agreement never intervenes).
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

fn arm_a_events_at(width: u32, n: usize, trials: u64) -> (u64, u64) {
    let mut events = 0u64;
    for trial in 0..trials {
        let mut rng = StdRng::seed_from_u64(trial);
        let (a_keys, b_keys) = balanced_swap(&mut rng, n, 1);
        let a = NarrowStore::new(width, a_keys);
        let b = NarrowStore::new(width, b_keys);
        if declares_convergence(&a, &b) {
            events += 1;
        }
    }
    (events, trials)
}

// -------------------------------------------------------------------------------------------
// Arm B — the truncation layer (L2)
// -------------------------------------------------------------------------------------------

/// A store whose [`RsosView::aggregate`] republishes `trunc_τ(H(size, Σ))` as *both* halves of the
/// [`Aggregate`] the driver sees, `H` the real, unreduced [`rsos::digest`] over the true `(size, Σ)`
/// pair — see the module docs for why this is how a hypothetical truncating comparison map is
/// measured against the real, unmodified driver.
struct HashCompareStore {
    tau: u32,
    keys: Vec<u64>,
}

impl HashCompareStore {
    fn new(tau: u32, mut keys: Vec<u64>) -> HashCompareStore {
        keys.sort_unstable();
        keys.dedup();
        HashCompareStore { tau, keys }
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
}

impl RsosView<u64> for HashCompareStore {
    fn size(&self) -> usize {
        self.keys.len()
    }

    fn aggregate<R: RangeBounds<u64>>(&self, range: R) -> Aggregate {
        let (start, end) = self.span(&range);
        let slice = &self.keys[start..end];
        let size = slice.len();
        let sum = slice
            .iter()
            .fold(Fingerprint::ZERO, |acc, key| acc.combine(digest(key)));
        let comparison_value = digest(&(size, sum)).0[0] & mask(self.tau);
        Aggregate::new(
            comparison_value as usize,
            Fingerprint([comparison_value, 0, 0, 0]),
        )
    }

    fn rank(&self, z: &u64) -> usize {
        self.keys.partition_point(|x| x < z)
    }

    fn select(&self, r: usize) -> &u64 {
        &self.keys[r]
    }
}

/// A pure-deletion difference (`Y = X ∖ {one key}`): the real `(size, Σ)` genuinely differs
/// whenever the universe is non-trivial, which is the precondition L2's definition assumes.
fn pure_deletion(rng: &mut StdRng, n: usize) -> Option<(Vec<u64>, Vec<u64>)> {
    let mut universe: Vec<u64> = (0..n).map(|_| rng.gen()).collect();
    universe.sort_unstable();
    universe.dedup();
    if universe.is_empty() {
        return None;
    }
    let removed_idx = rng.gen_range(0..universe.len());
    let mut deleted = universe.clone();
    deleted.swap_remove(removed_idx);
    Some((universe, deleted))
}

fn arm_b_events_at(tau: u32, n: usize, trials: u64) -> (u64, u64) {
    let mut events = 0u64;
    for trial in 0..trials {
        let mut rng = StdRng::seed_from_u64(trial);
        let Some((universe, deleted)) = pure_deletion(&mut rng, n) else {
            continue;
        };
        let a = HashCompareStore::new(tau, universe);
        let b = HashCompareStore::new(tau, deleted);
        if declares_convergence(&a, &b) {
            events += 1;
        }
    }
    (events, trials)
}

// -------------------------------------------------------------------------------------------
// Rate measurements
// -------------------------------------------------------------------------------------------

/// `(width_or_tau, target expected events)`. Trial count is derived as `target << width`, so the
/// shipped width gets a solid sample and the next one gets a smaller but non-zero one, keeping this
/// module's total run time in the minutes, not the cluster-job range `w`/`τ` = 32 would need.
const EXPECTED_EVENTS: [(u32, u64); 2] = [(16, 100), (24, 5)];

/// Universe size for the plain rate measurements — small enough that `trials` (in the tens to
/// hundreds of millions at the wider configuration) stays affordable; the claim does not depend on
/// scale, only the `n`-scaling sweep below varies it deliberately.
const RATE_STORE_SIZE: usize = 16;

#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn arm_a_aggregate_collision_rate() {
    for (width, target_events) in EXPECTED_EVENTS {
        let trials = target_events << width;
        let (events, trials) = arm_a_events_at(width, RATE_STORE_SIZE, trials);
        let (lo, hi) = wilson_99_ci(events, trials);
        let predicted = 1.0 / (1u64 << width) as f64;
        println!(
            "Arm A (L1) w={width}: {events}/{trials} events, 99% CI=[{lo:.3e}, {hi:.3e}], \
             predicted 2^-{width}={predicted:.3e}"
        );
    }
}

#[test]
#[ignore = "prints a measured rate; run explicitly — see this module's docs for the invocation"]
fn arm_b_comparison_map_collision_rate() {
    for (tau, target_events) in EXPECTED_EVENTS {
        let trials = target_events << tau;
        let (events, trials) = arm_b_events_at(tau, RATE_STORE_SIZE, trials);
        let (lo, hi) = wilson_99_ci(events, trials);
        let predicted = 1.0 / (1u64 << tau) as f64;
        println!(
            "Arm B (L2) tau={tau}: {events}/{trials} events, 99% CI=[{lo:.3e}, {hi:.3e}], \
             predicted 2^-{tau}={predicted:.3e}"
        );
    }
}

#[test]
#[ignore = "prints a measured sweep; run explicitly — see this module's docs for the invocation"]
fn n_scaling_discriminator() {
    const NS: [usize; 3] = [20, 200, 2_000];
    const WIDTH: u32 = 16;
    const TARGET_EVENTS: u64 = 8;
    let trials = TARGET_EVENTS << WIDTH;

    println!("Arm A (L1) at w={WIDTH}, fixed swap size 1 — predicted flat in log n:");
    for n in NS {
        let (events, trials) = arm_a_events_at(WIDTH, n, trials);
        let (lo, hi) = wilson_99_ci(events, trials);
        println!("  n={n:>5}: {events}/{trials} events, 99% CI=[{lo:.3e}, {hi:.3e}]");
    }

    println!("Arm B (L2) at tau={WIDTH}, fixed deletion size 1 — predicted to rise in log n:");
    for n in NS {
        let (events, trials) = arm_b_events_at(WIDTH, n, trials);
        let (lo, hi) = wilson_99_ci(events, trials);
        println!("  n={n:>5}: {events}/{trials} events, 99% CI=[{lo:.3e}, {hi:.3e}]");
    }
}

/// Byte-sequence determinism (`SOTA.md` §4.4): the trial generator is a pure function of its seed,
/// so re-running one configuration reproduces an identical byte sequence — the property that makes
/// a disagreement between this reduced arm and a future end-to-end arm attributable to one side or
/// the other, not to nondeterminism in either harness.
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
