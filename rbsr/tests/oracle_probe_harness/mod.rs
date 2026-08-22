// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Shared scaffolding for the two #356 follow-up measurement modules
//! (`joint_progress_and_the_oracle_coupling_confound.rs`,
//! `the_union_bounds_effective_multiplier.rs`): a reduced-width store, the instance generator, and
//! a driver that **proves** a stall instead of inferring one from a round cap.
//!
//! Shared rather than duplicated because it is no longer the "small test scaffolding" this crate
//! duplicates by precedent — [`drive`]'s visited-state bookkeeping is the load-bearing part of both
//! modules' method, and two copies of it would be two things to keep in agreement.

// Each integration test binary compiles this module separately and uses a different subset of it.
#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::{Bound, RangeBounds};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use rbsr::{
    initial_ranges, protocol_round_with_policy, Comparison, Decision, EnumerationRange,
    RangeAggregate, RefinementPolicy, RsosView,
};
use rsos::{digest, Aggregate, Fingerprint};

/// Round cap. Only reached by a drive that is neither settled nor *proved* stalled — see
/// [`Termination::RoundCap`], reported as its own bucket rather than folded into
/// "non-terminating".
pub const MAX_ROUNDS: usize = 512;

/// Universe size, matching `oracle_dependent_split_vs_the_union_bound.rs` so the numbers are
/// comparable.
pub const DRIVE_STORE_SIZE: usize = 512;

pub fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// A store summarizing with `Σ mod 2^width` instead of `Σ mod 2^256`.
pub struct NarrowStore {
    pub width: u32,
    pub keys: Vec<u64>,
}

impl NarrowStore {
    pub fn new(width: u32, mut keys: Vec<u64>) -> NarrowStore {
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

    pub fn keys_in(&self, range: &EnumerationRange<u64>) -> Vec<u64> {
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

/// A non-adversarial, count-balanced difference of `swap_size` elements over an `n`-key universe.
pub fn balanced_swap(rng: &mut StdRng, n: usize, swap_size: usize) -> (Vec<u64>, Vec<u64>) {
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

// ---------------------------------------------------------------------------------------------
// The driver, with stalls proved rather than inferred
// ---------------------------------------------------------------------------------------------

/// Algorithm 1's `t` in front of any split rule: **IDLIST at `span <= t`, delegate otherwise.**
///
/// Written here rather than in the `cfg(reconcile_internal_testing)` probe policies because it needs nothing crate-private —
/// which is the point it makes: `EnumerateBelowThreshold`'s cutoff is expressible over the public
/// [`Comparison`] API, so pairing it with a probe stride is something any policy author can do,
/// and the result is that the stall disappears.
pub struct EnumerateBelow<P> {
    pub threshold: usize,
    pub inner: P,
}

impl<P: RefinementPolicy> RefinementPolicy for EnumerateBelow<P> {
    fn decide(&self, comparison: Comparison) -> Decision {
        if comparison.agrees() {
            Decision::Skip
        } else if comparison.span() <= self.threshold {
            Decision::Enumerate
        } else {
            self.inner.decide(comparison)
        }
    }
}

/// How a drive ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Termination {
    /// The active family emptied: a fixed point.
    Settled,
    /// A `(parity, active family)` state recurred. The drive is a deterministic map on that
    /// state, so this is a **proof** the drive never terminates, with the cycle's length.
    Stalled { cycle_length: usize },
    /// Neither, within [`MAX_ROUNDS`] — the honest "no verdict" bucket. A non-empty count here
    /// means the round cap, not the mechanism, decided the outcome.
    RoundCap,
}

/// One drive's outcome.
pub struct Drive<K> {
    pub termination: Termination,
    pub enumerated: Vec<EnumerationRange<K>>,
    pub comparisons: u64,
    pub rounds: usize,
}

/// The visited-state table: `(responder parity, state hash) -> (round, exact state)`. The exact
/// state is kept so a hash collision cannot manufacture a false stall.
type VisitedStates = HashMap<(bool, u64), Vec<(usize, Vec<RangeAggregate<u64>>)>>;

/// A cheap hash of an active family, used only to *index* the visited-state table; every hit is
/// confirmed with `==`, so a hash collision cannot manufacture a false stall.
pub fn state_hash(active: &[RangeAggregate<u64>]) -> u64 {
    let mut hasher = DefaultHasher::new();
    // Every field #289 made public, so a state that hashes equal really is the same state; the
    // exact confirmation below then makes a hash collision harmless rather than merely unlikely.
    for segment in active {
        segment.start_bound().hash(&mut hasher);
        segment.end_bound().hash(&mut hasher);
        let aggregate = segment.aggregate();
        aggregate.size().hash(&mut hasher);
        aggregate.fingerprint().0.hash(&mut hasher);
    }
    hasher.finish()
}

/// Reconcile `a` against `b` under `policy`, alternating which peer answers, until the active
/// family empties, a state recurs, or [`MAX_ROUNDS`] rounds pass.
pub fn drive<P: RefinementPolicy>(a: &NarrowStore, b: &NarrowStore, policy: &P) -> Drive<u64> {
    drive_pair(a, b, policy, policy)
}

/// [`drive`] with a **policy per peer**. A refinement policy is a purely local choice this crate
/// never negotiates (`ARCHITECTURE.md` §3.1), so the two sides can disagree — and progress is a
/// *joint* property, which makes "does one bad peer suffice?" a different question from "do two?".
pub fn drive_pair<A: RefinementPolicy, B: RefinementPolicy>(
    a: &NarrowStore,
    b: &NarrowStore,
    policy_a: &A,
    policy_b: &B,
) -> Drive<u64> {
    let mut active = initial_ranges(a);
    let (mut responder, mut advertiser) = (b, a);
    let mut enumerated = Vec::new();
    let mut comparisons = 0u64;
    let mut rounds = 0;
    let mut responder_is_b = true;

    // parity -> hash -> the states already seen at that parity, kept for exact confirmation.
    let mut seen: VisitedStates = HashMap::new();

    let termination = loop {
        if active.is_empty() {
            break Termination::Settled;
        }
        if rounds >= MAX_ROUNDS {
            break Termination::RoundCap;
        }
        let key = (responder_is_b, state_hash(&active));
        let bucket = seen.entry(key).or_default();
        if let Some((first_round, _)) = bucket.iter().find(|(_, state)| *state == active) {
            break Termination::Stalled {
                cycle_length: rounds - first_round,
            };
        }
        bucket.push((rounds, active.clone()));

        let mut children = Vec::new();
        let mut enumerations = Vec::new();
        // `responder_is_b` still names the answering peer, so it selects that peer's own policy.
        let outcome = if responder_is_b {
            protocol_round_with_policy(
                responder,
                policy_b,
                active,
                &mut children,
                &mut enumerations,
            )
        } else {
            protocol_round_with_policy(
                responder,
                policy_a,
                active,
                &mut children,
                &mut enumerations,
            )
        };
        comparisons += (outcome.skipped()
            + outcome.enumerated()
            + outcome.split()
            + outcome.dropped_malformed()) as u64;
        enumerated.append(&mut enumerations);
        active = children;
        rounds += 1;
        std::mem::swap(&mut responder, &mut advertiser);
        responder_is_b = !responder_is_b;
    };

    Drive {
        termination,
        enumerated,
        comparisons,
        rounds,
    }
}

/// A policy that answers exactly like `inner` while tallying what each comparison *was*.
///
/// The two counts the union bound needs, both read off the public [`Comparison`] API:
///
/// - **`comparisons`** — every range the drive classified: the multiplier
///   `oracle_dependent_split_vs_the_union_bound.rs` unions over.
/// - **`collision_capable`** — the ranges where the two sides advertised the *same size* while
///   disagreeing. [`Comparison::agrees`] tests the whole [`Aggregate`], so a range whose sizes
///   differ is refused at **every** width and no fingerprint collision can turn it into a SKIP.
///   Only these contribute to a false convergence.
///
/// Counted at full width (`w = 64`), where a collision is negligible, so "disagreeing" means
/// genuinely different contents. For a rank-cut policy that is sound: the descent is a function
/// of the data alone, so it is the same descent a narrow run takes, right up to the first
/// collision.
pub struct Observing<P> {
    pub inner: P,
    pub comparisons: std::cell::Cell<u64>,
    pub collision_capable: std::cell::Cell<u64>,
    /// `Split` decisions with `stride >= span` at `span > 1`: the ones that emit one child equal
    /// to the parent instead of refining.
    ///
    /// A **policy-level** count, so it is unchanged by whether the driver lets such a decision
    /// through. Against an unguarded driver it is the stall mechanism; against a driver that
    /// converts these to IDLIST (#420 / PR #459) it is the *price* of that guard — one forced
    /// enumeration per occurrence.
    pub non_progressing: std::cell::Cell<u64>,
}

impl<P> Observing<P> {
    pub fn new(inner: P) -> Observing<P> {
        Observing {
            inner,
            comparisons: std::cell::Cell::new(0),
            collision_capable: std::cell::Cell::new(0),
            non_progressing: std::cell::Cell::new(0),
        }
    }
}

impl<P: RefinementPolicy> RefinementPolicy for Observing<P> {
    fn decide(&self, comparison: Comparison) -> Decision {
        self.comparisons.set(self.comparisons.get() + 1);
        if !comparison.agrees() && comparison.span() == comparison.remote_size() {
            self.collision_capable.set(self.collision_capable.get() + 1);
        }
        let decision = self.inner.decide(comparison);
        if let Decision::Split(stride) = decision {
            if comparison.span() > 1 && stride.get() >= comparison.span() {
                self.non_progressing.set(self.non_progressing.get() + 1);
            }
        }
        decision
    }
}

// ---------------------------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------------------------

/// Two-sided 99% Wilson score interval for `successes` out of `trials`.
pub fn wilson_99_ci(successes: u64, trials: u64) -> (f64, f64) {
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

/// One policy's outcome over `trials` independently seeded instances at one width.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tally {
    pub trials: u64,
    pub settled: u64,
    pub stalled: u64,
    pub round_cap: u64,
    pub false_convergence: u64,
    pub comparisons: u64,
    pub rounds: u64,
}

impl Tally {
    pub fn mean_comparisons(&self) -> f64 {
        if self.settled == 0 {
            0.0
        } else {
            self.comparisons as f64 / self.settled as f64
        }
    }
}

/// `a`'s and `b`'s keys that appear in exactly one of the two.
pub fn symmetric_difference(a: &[u64], b: &[u64]) -> HashSet<u64> {
    let a: HashSet<u64> = a.iter().copied().collect();
    let b: HashSet<u64> = b.iter().copied().collect();
    a.symmetric_difference(&b).copied().collect()
}

/// [`measure`], with the policy wrapped in [`Observing`] so the run also reports the two union-bound
/// multipliers **at the width being measured**.
///
/// Measuring them at the measured width rather than at `w = 64` is required for an oracle-coupled
/// policy and merely harmless for a rank-cut one, which is itself the point: a rank-cut policy
/// descends the same ranges at every width, so one measurement transfers; an oracle-coupled
/// policy's index set is a *different* random variable at each width, so nothing transfers. The
/// residual bias is that a comparison resolved by an actual collision is booked as agreeing rather
/// than as collision-capable, which undercounts by the very rate under measurement — second order
/// at every width reported here.
pub fn measure_observed<P: RefinementPolicy>(
    width: u32,
    trials: u64,
    swap_size: usize,
    policy: P,
) -> (Tally, f64) {
    let observing = Observing::new(policy);
    let tally = measure_d(width, trials, swap_size, &observing, |_, _, _| {});
    let capable = observing.collision_capable.get() as f64;
    (tally, capable / trials.max(1) as f64)
}

/// Run `trials` drives of `policy` at `width`, recording each trial's verdict.
pub fn measure<P: RefinementPolicy>(
    width: u32,
    trials: u64,
    policy: &P,
    per_trial: impl FnMut(u64, Termination, bool),
) -> Tally {
    measure_d(width, trials, 1, policy, per_trial)
}

/// [`measure`] over a `swap_size`-element difference instead of a single swapped key.
///
/// The difference size is load-bearing for the soundness half, not a free parameter: only a range
/// holding an *equal count* on both sides can falsely agree, so at `swap_size = 1` the outer range
/// is essentially the only collision-capable comparison a drive makes — and it is compared before
/// any split decision, which makes the rate policy-independent by construction.
pub fn measure_d<P: RefinementPolicy>(
    width: u32,
    trials: u64,
    swap_size: usize,
    policy: &P,
    mut per_trial: impl FnMut(u64, Termination, bool),
) -> Tally {
    let mut tally = Tally {
        trials,
        ..Tally::default()
    };
    for trial in 0..trials {
        let mut rng = StdRng::seed_from_u64(trial);
        let (a_keys, b_keys) = balanced_swap(&mut rng, DRIVE_STORE_SIZE, swap_size);
        let diff = symmetric_difference(&a_keys, &b_keys);
        let a = NarrowStore::new(width, a_keys);
        let b = NarrowStore::new(width, b_keys);

        let result = drive(&a, &b, policy);
        tally.rounds += result.rounds as u64;
        let mut missed = false;
        match result.termination {
            Termination::Settled => {
                tally.settled += 1;
                tally.comparisons += result.comparisons;
                let found: HashSet<u64> = result
                    .enumerated
                    .iter()
                    .flat_map(|r| a.keys_in(r).into_iter().chain(b.keys_in(r)))
                    .collect();
                missed = !diff.is_subset(&found);
                if missed {
                    tally.false_convergence += 1;
                }
            }
            Termination::Stalled { .. } => tally.stalled += 1,
            Termination::RoundCap => tally.round_cap += 1,
        }
        per_trial(trial, result.termination, missed);
    }
    tally
}

pub fn report_termination(label: &str, width: u32, t: Tally) {
    let pct = |n: u64| 100.0 * n as f64 / t.trials as f64;
    println!(
        "{label:<34} w={width:<3} settled {:>7}/{:<7} ({:>6.2}%)  proved-stalled {:>7} ({:>6.2}%)  \
         round-cap {:>6} ({:>5.2}%)  mean rounds {:>6.1}",
        t.settled,
        t.trials,
        pct(t.settled),
        t.stalled,
        pct(t.stalled),
        t.round_cap,
        pct(t.round_cap),
        t.rounds as f64 / t.trials as f64,
    );
}

pub fn report_soundness(label: &str, width: u32, t: Tally) {
    if t.settled == 0 {
        println!("{label:<34} w={width:<3} no settled drive — nothing to report");
        return;
    }
    let (lo, hi) = wilson_99_ci(t.false_convergence, t.settled);
    let scale = (1u64 << width) as f64;
    println!(
        "{label:<34} w={width:<3} {:>6} events / {:>7} settled  rate={:.4e} \
         99% CI=[{lo:.3e}, {hi:.3e}]  loose bound={:.4e} (mean cmp {:.2})",
        t.false_convergence,
        t.settled,
        t.false_convergence as f64 / t.settled as f64,
        t.mean_comparisons() / scale,
        t.mean_comparisons(),
    );
}
