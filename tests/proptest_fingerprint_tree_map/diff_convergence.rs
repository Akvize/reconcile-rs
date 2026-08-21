// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Property 2: convergence of the diff protocol. Each store gets an arbitrary subset of one
//! universe, with shared keys carrying identical values — conflict resolution is `ReplicatedMap`'s
//! job — so the symmetric difference is well defined and must be discovered exactly.

use std::collections::BTreeMap;

use proptest::prelude::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use rbsr::{
    initial_ranges, protocol_round, protocol_round_with_policy, EnumerateBelowThreshold,
    EnumerationRange, FanOut, FixedFanOut, RangeAggregate, RefinementPolicy, SqrtFanOut,
};
use rsos::FingerprintTreeMap;

type Tree = FingerprintTreeMap<u64, u64>;

/// One full diff exchange, `perturb` modelling an adversarial transport. Returns the ranges each
/// peer owes the other.
fn run_diff(
    a: &Tree,
    b: &Tree,
    perturb: &mut dyn FnMut(&mut Vec<RangeAggregate<u64>>),
) -> (Vec<EnumerationRange<u64>>, Vec<EnumerationRange<u64>>) {
    let mut a_diffs = Vec::new();
    let mut b_diffs = Vec::new();
    let mut a_seg = initial_ranges(a);
    let mut b_seg = Vec::new();

    let mut guard = 0;
    while !a_seg.is_empty() {
        perturb(&mut a_seg);
        protocol_round(b, std::mem::take(&mut a_seg), &mut b_seg, &mut b_diffs);
        perturb(&mut b_seg);
        protocol_round(a, std::mem::take(&mut b_seg), &mut a_seg, &mut a_diffs);

        guard += 1;
        // Bounded number of refinement rounds: the protocol fans out by 16 per
        // round, so even with duplication this terminates quickly.
        assert!(guard < 100_000, "diff loop failed to terminate");
    }
    (a_diffs, b_diffs)
}

/// The same exchange with each peer running its **own** [`RefinementPolicy`]; the responder's
/// policy applies to each half-round.
fn run_diff_with_policies(
    a: &Tree,
    b: &Tree,
    a_policy: &dyn RefinementPolicy,
    b_policy: &dyn RefinementPolicy,
) -> (Vec<EnumerationRange<u64>>, Vec<EnumerationRange<u64>>) {
    let mut a_diffs = Vec::new();
    let mut b_diffs = Vec::new();
    let mut a_seg = initial_ranges(a);
    let mut b_seg = Vec::new();

    let mut guard = 0;
    while !a_seg.is_empty() {
        protocol_round_with_policy(
            b,
            b_policy,
            std::mem::take(&mut a_seg),
            &mut b_seg,
            &mut b_diffs,
        );
        protocol_round_with_policy(
            a,
            a_policy,
            std::mem::take(&mut b_seg),
            &mut a_seg,
            &mut a_diffs,
        );

        guard += 1;
        assert!(guard < 100_000, "diff loop failed to terminate");
    }
    (a_diffs, b_diffs)
}

/// The shipped policies, indexed so a proptest strategy can pick a pair. Boxed rather than a
/// concrete type because the point is to mix them.
fn policy(index: usize) -> Box<dyn RefinementPolicy> {
    match index {
        0 => Box::new(SqrtFanOut),
        1 => Box::new(FixedFanOut::new(FanOut::BINARY)),
        2 => Box::new(FixedFanOut::new(FanOut::NEGENTROPY)),
        _ => Box::new(EnumerateBelowThreshold::PAPER),
    }
}

/// Collect the (key, value) pairs that `tree` holds inside any of `ranges`.
fn items_in(tree: &Tree, ranges: &[EnumerationRange<u64>]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = ranges
        .iter()
        .flat_map(|r| tree.range(*r).map(|(k, v)| (*k, *v)).collect::<Vec<_>>())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Keys of `tree` covered by `ranges`.
fn keys_in(tree: &Tree, ranges: &[EnumerationRange<u64>]) -> Vec<u64> {
    items_in(tree, ranges).into_iter().map(|(k, _)| k).collect()
}

fn sorted_items(tree: &Tree) -> Vec<(u64, u64)> {
    tree.range(..).map(|(k, v)| (*k, *v)).collect()
}

/// Build two trees from a universe and per-entry membership flags. Returns the
/// trees plus the oracle key sets `(only_a, only_b, union)`.
fn build_pair(
    universe: &[(u64, u64, bool, bool)],
) -> (Tree, Tree, Vec<u64>, Vec<u64>, BTreeMap<u64, u64>) {
    // Deduplicate by key, keeping the first occurrence so shared keys are
    // guaranteed identical values across both trees.
    let mut seen = BTreeMap::new();
    let mut a = FingerprintTreeMap::new();
    let mut b = FingerprintTreeMap::new();
    let mut only_a = Vec::new();
    let mut only_b = Vec::new();
    let mut union = BTreeMap::new();
    for &(k, v, in_a, in_b) in universe {
        if seen.insert(k, v).is_some() {
            continue; // already handled this key
        }
        if in_a {
            a.insert(k, v);
        }
        if in_b {
            b.insert(k, v);
        }
        match (in_a, in_b) {
            (true, false) => only_a.push(k),
            (false, true) => only_b.push(k),
            _ => {}
        }
        if in_a || in_b {
            union.insert(k, v);
        }
    }
    only_a.sort_unstable();
    only_b.sort_unstable();
    (a, b, only_a, only_b, union)
}

fn universe_strategy() -> impl Strategy<Value = Vec<(u64, u64, bool, bool)>> {
    prop::collection::vec(
        (any::<u64>(), any::<u64>(), any::<bool>(), any::<bool>()),
        0..80,
    )
}

/// Apply both sides of a diff result, reconciling the two trees in place.
fn apply_full(
    a: &mut Tree,
    b: &mut Tree,
    a_diffs: &[EnumerationRange<u64>],
    b_diffs: &[EnumerationRange<u64>],
) {
    let a_items = items_in(a, a_diffs);
    let b_items = items_in(b, b_diffs);
    for (k, v) in a_items {
        b.insert(k, v);
    }
    for (k, v) in b_items {
        a.insert(k, v);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Clean channel: a single diff exchange must discover exactly the symmetric
    /// difference of the key sets and reconcile the two trees to the union.
    #[test]
    fn two_trees_converge_and_diff_is_symmetric_difference(universe in universe_strategy()) {
        let (mut a, mut b, only_a, only_b, union) = build_pair(&universe);

        let mut noop = |_: &mut Vec<RangeAggregate<u64>>| {};
        let (a_diffs, b_diffs) = run_diff(&a, &b, &mut noop);

        // The discovered diff ranges cover exactly the symmetric difference.
        prop_assert_eq!(keys_in(&a, &a_diffs), only_a);
        prop_assert_eq!(keys_in(&b, &b_diffs), only_b);

        apply_full(&mut a, &mut b, &a_diffs, &b_diffs);
        a.check_invariants();
        b.check_invariants();

        // Both trees now hold the full union and agree.
        let want: Vec<(u64, u64)> = union.into_iter().collect();
        prop_assert_eq!(sorted_items(&a), want.clone());
        prop_assert_eq!(sorted_items(&b), want);
        prop_assert!(a == b);
    }

    /// Reordered + duplicated messages: an adversarial transport that shuffles
    /// every batch and duplicates one segment per round must not prevent
    /// convergence, and must still find exactly the symmetric difference.
    #[test]
    fn convergence_survives_reordered_and_duplicated_messages(
        universe in universe_strategy(),
        seed in any::<u64>(),
    ) {
        let (mut a, mut b, only_a, only_b, union) = build_pair(&universe);

        let mut rng = StdRng::seed_from_u64(seed);
        let mut perturb = |segs: &mut Vec<RangeAggregate<u64>>| {
            if !segs.is_empty() {
                // Duplicate a single random segment (bounded growth) ...
                let i = rng.gen_range(0..segs.len());
                let dup = segs[i].clone();
                segs.push(dup);
            }
            // ... and deliver the batch out of order.
            segs.shuffle(&mut rng);
        };

        let (a_diffs, b_diffs) = run_diff(&a, &b, &mut perturb);

        prop_assert_eq!(keys_in(&a, &a_diffs), only_a);
        prop_assert_eq!(keys_in(&b, &b_diffs), only_b);

        apply_full(&mut a, &mut b, &a_diffs, &b_diffs);
        a.check_invariants();
        b.check_invariants();

        let want: Vec<(u64, u64)> = union.into_iter().collect();
        prop_assert_eq!(sorted_items(&a), want.clone());
        prop_assert_eq!(sorted_items(&b), want);
        prop_assert!(a == b);
    }

    /// Lossy cycles deliver in one direction only: invariants must hold throughout, and
    /// convergence must follow the first complete exchange.
    #[test]
    fn convergence_is_eventual_despite_dropped_messages(
        universe in universe_strategy(),
        schedule in prop::collection::vec(any::<bool>(), 0..16),
    ) {
        let (mut a, mut b, _only_a, _only_b, union) = build_pair(&universe);

        let mut noop = |_: &mut Vec<RangeAggregate<u64>>| {};
        for deliver_a_to_b in schedule {
            let (a_diffs, b_diffs) = run_diff(&a, &b, &mut noop);
            // Drop one whole direction this cycle.
            if deliver_a_to_b {
                apply_full(&mut a, &mut b, &a_diffs, &[]);
            } else {
                apply_full(&mut a, &mut b, &[], &b_diffs);
            }
            // Partial, lossy application must still leave both trees valid.
            a.check_invariants();
            b.check_invariants();
        }

        // A final, complete exchange (guaranteed retransmission) converges.
        let (a_diffs, b_diffs) = run_diff(&a, &b, &mut noop);
        apply_full(&mut a, &mut b, &a_diffs, &b_diffs);
        a.check_invariants();
        b.check_invariants();

        let want: Vec<(u64, u64)> = union.into_iter().collect();
        prop_assert_eq!(sorted_items(&a), want.clone());
        prop_assert_eq!(sorted_items(&b), want);
        prop_assert!(a == b);
    }

    /// Any refinement policy converges, and so does any mixed pair — the claim that makes the
    /// seam free.
    ///
    /// Weaker than `two_trees_converge_and_diff_is_symmetric_difference`: an enumeration threshold
    /// makes the diff ranges *cover* the symmetric difference rather than equal it. The outcome may
    /// not weaken — both trees hold the union and agree.
    #[test]
    fn convergence_holds_under_any_policy_and_any_mixed_pair(
        universe in universe_strategy(),
        a_index in 0usize..4,
        b_index in 0usize..4,
    ) {
        let (mut a, mut b, only_a, only_b, union) = build_pair(&universe);
        let (a_policy, b_policy) = (policy(a_index), policy(b_index));

        let (a_diffs, b_diffs) =
            run_diff_with_policies(&a, &b, a_policy.as_ref(), b_policy.as_ref());

        // Cover, not equal: everything only one side holds must be inside some enumerated range.
        let a_found = keys_in(&a, &a_diffs);
        let b_found = keys_in(&b, &b_diffs);
        prop_assert!(only_a.iter().all(|k| a_found.contains(k)));
        prop_assert!(only_b.iter().all(|k| b_found.contains(k)));

        apply_full(&mut a, &mut b, &a_diffs, &b_diffs);
        a.check_invariants();
        b.check_invariants();

        let want: Vec<(u64, u64)> = union.into_iter().collect();
        prop_assert_eq!(sorted_items(&a), want.clone());
        prop_assert_eq!(sorted_items(&b), want);
        prop_assert!(a == b);
    }
}
