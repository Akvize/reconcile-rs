// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Two comparison-map configurations whose predicted false-convergence rate is **exactly zero**
//! under a pure-deletion difference.
//!
//! `rbsr` compares the *whole* [`Aggregate`] — `(count, fingerprint)` — never the fingerprint
//! alone. Under a pure-deletion difference (`Y = X ∖ S`, `S` non-empty), any range that actually
//! holds part of `S` has strictly fewer elements on the `Y` side, so `count-agreement` alone
//! (`rbsr::RsosView`'s docs) forbids the driver from ever declaring that range converged. This is
//! true **whatever** the comparison map does with the fingerprint half — it holds for `f_p = id`
//! (the shipped map) and it holds just as well for the separable map `f_p = (count, Σ mod 2^τ)` at
//! any `τ`, because both configurations still carry the exact count.
//!
//! That makes the predicted event rate not merely small but **zero**, with no hypothesis on the
//! lift: a single observed false convergence — a differing range the driver SKIPs — refutes count
//! exactness outright, and no statistics are needed to reject the hypothesis on one witness. That
//! is why this lives in the standard `cargo test --workspace` gate next to the four Wagner tests in
//! `wagner_false_convergence.rs`, rather than in a bench: a bench reports a rate, this asserts a
//! certainty.
//!
//! **Trial count and seeding.** [`TRIALS`] independent pure-deletion instances per configuration,
//! each a fresh universe of up to a few hundred keys with a non-empty deleted subset, seeded from
//! the trial index (`StdRng::seed_from_u64`) — a recorded counter, not the process RNG, so a
//! failure reproduces from the printed trial number alone. Convergence is driven to a full fixed
//! point (mirroring `rbsr/tests/balance_under_position_map.rs`'s `drive` loop) and the discovered
//! enumeration ranges are checked against the *true* symmetric difference: any key in `S` that
//! never surfaces there is exactly the signature of a false SKIP the type system otherwise makes
//! unreachable, so this test is the regression guard on that unreachability actually holding in the
//! driver's own code, not merely in the `Aggregate` type it compares.
//!
//! **What this does not cover.** A conflict-shaped-difference measurement and a truncation-only
//! measurement, both of which predict a *non-zero* rate and need a two-sided confidence interval
//! plus cluster-scale compute, are separate open work this module does not attempt. Byte-sequence
//! determinism (`SOTA.md` §4.4) applies to that rate measurement, not to the exact-zero claim
//! checked here, so there is no run to reproduce byte-for-byte beyond the seeded RNG above.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::ops::{Bound, RangeBounds};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use rbsr::{initial_ranges, protocol_round, EnumerationRange, RsosView};
use rsos::{lift, Aggregate, Fingerprint, FingerprintTreeMap, Rsos};

/// Independent pure-deletion instances driven per configuration. Large enough that a
/// once-in-a-thousand implementation slip (e.g. a comparison that drops the count half) would
/// almost certainly surface, while staying inside the standard test gate's time budget.
const TRIALS: u64 = 500;

/// Past this many rounds the drive is not converging; the cap turns a hang into a failure rather
/// than a timeout with no diagnostic.
const MAX_ROUNDS: usize = 128;

/// Low `width` bits set. `width` is always in `1..=64` here.
fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

// -------------------------------------------------------------------------------------------
// Configuration 2: f_p = (count, Σ mod 2^τ), the separable map
// -------------------------------------------------------------------------------------------

/// A store summarizing with `Σ mod 2^τ` instead of the shipped `Σ mod 2^256` — [`rsos::lift`]'s
/// low `τ` bits, reduction being the homomorphism `ℤ/2²⁵⁶ → ℤ/2^τ`, so this is the same algebra
/// truncated, not a different construction. `τ` is a free parameter of the configuration under
/// test, not a width the attack depends on (unlike `wagner_false_convergence.rs`'s narrowing,
/// which exists to make a *collision* affordable) — 32 bits is enough to keep list bookkeeping
/// cheap while remaining representative of "any τ".
struct SeparableStore {
    tau: u32,
    keys: Vec<u64>,
}

impl SeparableStore {
    fn new(tau: u32, mut keys: Vec<u64>) -> SeparableStore {
        keys.sort_unstable();
        keys.dedup();
        SeparableStore { tau, keys }
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

    fn keys_in(&self, range: &EnumerationRange<u64>) -> Vec<u64> {
        let (start, end) = self.span(range);
        self.keys[start..end].to_vec()
    }
}

impl RsosView<u64> for SeparableStore {
    fn size(&self) -> usize {
        self.keys.len()
    }

    fn aggregate<R: RangeBounds<u64>>(&self, range: R) -> Aggregate {
        let (start, end) = self.span(&range);
        let slice = &self.keys[start..end];
        let sum = slice.iter().fold(0u64, |acc, &key| {
            let limb = lift(&key, &()).0[0] & mask(self.tau);
            acc.wrapping_add(limb) & mask(self.tau)
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

// -------------------------------------------------------------------------------------------
// Driving the real protocol to a fixed point, and checking what it found against ground truth
// -------------------------------------------------------------------------------------------

/// Reconcile `a` against `b` to a fixed point, alternating which peer answers, collecting every
/// IDLIST range either side was asked to enumerate.
fn drive<K: Clone + Ord, B: RsosView<K>>(
    a: &B,
    b: &B,
) -> (Vec<EnumerationRange<K>>, Vec<EnumerationRange<K>>) {
    let mut active = initial_ranges(a);
    let mut responder = b;
    let mut advertiser = a;
    let mut a_enumerations = Vec::new();
    let mut b_enumerations = Vec::new();
    let mut rounds = 0;

    // `a` always advertises first, so parity tracks which peer just answered.
    let mut responder_is_b = true;
    while !active.is_empty() && rounds < MAX_ROUNDS {
        let mut children = Vec::new();
        let mut enumerations = Vec::new();
        protocol_round(responder, active, &mut children, &mut enumerations);
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
    assert!(rounds < MAX_ROUNDS, "the drive did not reach a fixed point");
    (a_enumerations, b_enumerations)
}

/// A random universe `X` and a non-empty deleted subset `S`, `Y = X ∖ S` — the pure-deletion
/// difference arm C is stated over. Sizes are kept modest so `TRIALS` runs stay fast; the claim
/// does not depend on scale.
fn deletion_instance(rng: &mut StdRng) -> (Vec<u64>, HashSet<u64>) {
    let universe_size = rng.gen_range(2..200);
    let mut universe: Vec<u64> = (0..universe_size).map(|_| rng.gen()).collect();
    universe.sort_unstable();
    universe.dedup();
    assert!(!universe.is_empty());

    let deleted_count = rng.gen_range(1..=universe.len());
    universe.shuffle(rng);
    let deleted: HashSet<u64> = universe[..deleted_count].iter().copied().collect();
    universe.sort_unstable();
    (universe, deleted)
}

/// Every key either peer was asked to hand over explicitly, read back through `keys_of` so the
/// check is against ground truth rather than the driver's own bookkeeping.
fn enumerated_keys<K: Ord + Copy + std::hash::Hash>(
    ranges: &[EnumerationRange<K>],
    keys_of: impl Fn(&EnumerationRange<K>) -> Vec<K>,
) -> HashSet<K> {
    ranges.iter().flat_map(keys_of).collect()
}

/// **Configuration 1 — `f_p = id`, the shipped comparison map.** [`FingerprintTreeMap`] compared
/// through its blanket [`RsosView`] impl: the real `rsos::Rsos::aggregate`, no truncation.
#[test]
fn f_p_id_never_declares_false_convergence_on_a_pure_deletion_difference() {
    for trial in 0..TRIALS {
        let mut rng = StdRng::seed_from_u64(trial);
        let (universe, deleted) = deletion_instance(&mut rng);

        let mut a: FingerprintTreeMap<u64, ()> = FingerprintTreeMap::new();
        let mut b: FingerprintTreeMap<u64, ()> = FingerprintTreeMap::new();
        for &key in &universe {
            a.insert(key, ());
            if !deleted.contains(&key) {
                b.insert(key, ());
            }
        }
        assert_ne!(
            Rsos::size(&a),
            Rsos::size(&b),
            "trial {trial}: a non-empty deletion must unbalance the outer range"
        );

        let (a_enum, b_enum) = drive(&a, &b);
        let keys_of = |range: &EnumerationRange<u64>| -> Vec<u64> {
            a.range(*range)
                .chain(b.range(*range))
                .map(|(k, ())| *k)
                .collect()
        };
        let found = enumerated_keys(&a_enum, keys_of)
            .into_iter()
            .chain(enumerated_keys(&b_enum, keys_of))
            .collect::<HashSet<u64>>();

        for key in &deleted {
            assert!(
                found.contains(key),
                "trial {trial}: f_p=id false-converged — deleted key {key} was never enumerated, \
                 which is only possible if some range containing it was SKIPped despite an \
                 unbalanced count"
            );
        }
    }
}

/// **Configuration 2 — `f_p = (count, Σ mod 2^τ)`, the separable map.** `τ = 32`, representative
/// of "any τ": the claim rests on the count half alone, so no width makes it fail.
#[test]
fn separable_map_never_declares_false_convergence_on_a_pure_deletion_difference() {
    const TAU: u32 = 32;

    for trial in 0..TRIALS {
        let mut rng = StdRng::seed_from_u64(trial);
        let (universe, deleted) = deletion_instance(&mut rng);
        let kept: Vec<u64> = universe
            .iter()
            .copied()
            .filter(|k| !deleted.contains(k))
            .collect();

        let a = SeparableStore::new(TAU, universe.clone());
        let b = SeparableStore::new(TAU, kept);
        assert_ne!(
            a.size(),
            b.size(),
            "trial {trial}: a non-empty deletion must unbalance the outer range"
        );

        let (a_enum, b_enum) = drive(&a, &b);
        let found: HashSet<u64> = a_enum
            .iter()
            .flat_map(|r| a.keys_in(r))
            .chain(b_enum.iter().flat_map(|r| b.keys_in(r)))
            .collect();

        for key in &deleted {
            assert!(
                found.contains(key),
                "trial {trial}: separable map (τ={TAU}) false-converged — deleted key {key} was \
                 never enumerated"
            );
        }
    }
}
