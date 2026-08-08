// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Property-based / generative tests.
//!
//! These exercise the two invariants that matter most for a reconciliation
//! library and that fixed-seed example tests cannot cover exhaustively:
//!
//! 1. The hand-rolled B-tree (`FingerprintTreeMap`) behaves like a `BTreeMap` oracle for
//!    every random `insert`/`remove`/`get`/`range` sequence, and its internal
//!    [`FingerprintTreeMap::check_invariants`] holds after *every* mutation (this is where
//!    the `TODO` rebalancing edge cases in `rsos/src/fingerprint_tree_map.rs` would surface).
//! 2. Any two stores converge to identical state after running the full diff
//!    loop, the returned diff ranges equal the true symmetric difference of the
//!    key sets, and convergence survives reordered, duplicated and dropped
//!    messages — modelling the lossy UDP transport.

use std::collections::BTreeMap;
use std::ops::Bound;

use proptest::prelude::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use rbsr::{diff_round, start_diff, DiffRange, RangeAggregate};
use rsos::{lift, Fingerprint, FingerprintTreeMap};

// ---------------------------------------------------------------------------
// Property 1: FingerprintTreeMap is observationally equivalent to a BTreeMap oracle, and
// every internal invariant holds after every single mutation.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    Insert(u8, u16),
    Remove(u8),
    Get(u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    // Small key space (u8) so that `remove`/`get` actually hit existing keys
    // often, and node splits/merges are exercised with modest sequence lengths.
    prop_oneof![
        (any::<u8>(), any::<u16>()).prop_map(|(k, v)| Op::Insert(k, v)),
        any::<u8>().prop_map(Op::Remove),
        any::<u8>().prop_map(Op::Get),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn fingerprint_tree_map_matches_btreemap_oracle(ops in prop::collection::vec(op_strategy(), 0..400)) {
        let mut tree: FingerprintTreeMap<u8, u16> = FingerprintTreeMap::new();
        let mut oracle: BTreeMap<u8, u16> = BTreeMap::new();

        for op in ops {
            match op {
                Op::Insert(k, v) => {
                    prop_assert_eq!(tree.insert(k, v), oracle.insert(k, v));
                }
                Op::Remove(k) => {
                    prop_assert_eq!(tree.remove(&k), oracle.remove(&k));
                }
                Op::Get(k) => {
                    prop_assert_eq!(tree.get(&k), oracle.get(&k));
                }
            }
            // The core safety net: structural, ordering, height, size and hash
            // caches must all hold after *every* mutation. Panics here would be
            // the rebalancing bugs the issue is worried about.
            tree.check_invariants();
            prop_assert_eq!(tree.len(), oracle.len());
        }

        // Full-range iteration yields the exact sorted oracle contents.
        let got: Vec<(u8, u16)> = tree.range(&..).map(|(k, v)| (*k, *v)).collect();
        let want: Vec<(u8, u16)> = oracle.iter().map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(got, want);

        // The cumulated fingerprint equals the sum of the per-element lifts
        // (and is order-independent), matching the diff protocol's fingerprint.
        let expected_fingerprint = oracle
            .iter()
            .fold(Fingerprint::ZERO, |acc, (k, v)| acc + lift(k, v));
        prop_assert_eq!(tree.aggregate(&..).fingerprint(), expected_fingerprint);
    }

    #[test]
    fn fingerprint_tree_map_range_queries_match_oracle(
        entries in prop::collection::vec((any::<u8>(), any::<u16>()), 0..200),
        lo in any::<u8>(),
        hi in any::<u8>(),
    ) {
        let mut tree: FingerprintTreeMap<u8, u16> = FingerprintTreeMap::new();
        let mut oracle: BTreeMap<u8, u16> = BTreeMap::new();
        for (k, v) in entries {
            tree.insert(k, v);
            oracle.insert(k, v);
        }
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let range = (Bound::Included(lo), Bound::Excluded(hi));

        let got: Vec<(u8, u16)> = tree.range(&range).map(|(k, v)| (*k, *v)).collect();
        let want: Vec<(u8, u16)> = oracle.range(range).map(|(k, v)| (*k, *v)).collect();
        prop_assert_eq!(&got, &want);

        // Range fingerprint is consistent with iterating the same range.
        let expected = want
            .iter()
            .fold(Fingerprint::ZERO, |acc, (k, v)| acc + lift(k, v));
        prop_assert_eq!(tree.aggregate(&range).fingerprint(), expected);
    }
}

// ---------------------------------------------------------------------------
// Property 2: convergence of the diff protocol.
//
// We model a universe of (key, value) pairs and give each store an arbitrary
// subset. Shared keys carry identical values (the diff algorithm reconciles the
// *set* of keys; per-key conflict resolution is the job of `ReplicatedMap`'s
// last-write-wins layer, not of the raw `FingerprintTreeMap`). The true symmetric
// difference of the key sets is therefore well defined and we assert the
// protocol discovers exactly it.
// ---------------------------------------------------------------------------

type Tree = FingerprintTreeMap<u64, u64>;

/// One full diff exchange, optionally perturbing the in-flight message vectors
/// each round with `perturb` to model an adversarial transport (reordering and
/// duplication). Returns `(a_owes, b_owes)`: the ranges `a` must send to `b` and
/// vice-versa.
fn run_diff(
    a: &Tree,
    b: &Tree,
    perturb: &mut dyn FnMut(&mut Vec<RangeAggregate<u64>>),
) -> (Vec<DiffRange<u64>>, Vec<DiffRange<u64>>) {
    let mut a_diffs = Vec::new();
    let mut b_diffs = Vec::new();
    let mut a_seg = start_diff(a);
    let mut b_seg = Vec::new();

    let mut guard = 0;
    while !a_seg.is_empty() {
        perturb(&mut a_seg);
        diff_round(b, std::mem::take(&mut a_seg), &mut b_seg, &mut b_diffs);
        perturb(&mut b_seg);
        diff_round(a, std::mem::take(&mut b_seg), &mut a_seg, &mut a_diffs);

        guard += 1;
        // Bounded number of refinement rounds: the protocol fans out by 16 per
        // round, so even with duplication this terminates quickly.
        assert!(guard < 100_000, "diff loop failed to terminate");
    }
    (a_diffs, b_diffs)
}

/// Collect the (key, value) pairs that `tree` holds inside any of `ranges`.
fn items_in(tree: &Tree, ranges: &[DiffRange<u64>]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = ranges
        .iter()
        .flat_map(|r| tree.range(r).map(|(k, v)| (*k, *v)).collect::<Vec<_>>())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Keys of `tree` covered by `ranges`.
fn keys_in(tree: &Tree, ranges: &[DiffRange<u64>]) -> Vec<u64> {
    items_in(tree, ranges).into_iter().map(|(k, _)| k).collect()
}

fn sorted_items(tree: &Tree) -> Vec<(u64, u64)> {
    tree.range(&..).map(|(k, v)| (*k, *v)).collect()
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
fn apply_full(a: &mut Tree, b: &mut Tree, a_diffs: &[DiffRange<u64>], b_diffs: &[DiffRange<u64>]) {
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

    /// Dropped messages: each lossy cycle delivers updates in only one direction
    /// (the other direction's updates are dropped), modelling lost UDP
    /// datagrams. The protocol must never corrupt state (invariants hold
    /// throughout) and must converge once a complete exchange finally gets
    /// through — exactly the real transport's retransmission guarantee.
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
}

// ---------------------------------------------------------------------------
// Property 3: the canonical encoding behind `lift`.
//
// `lift` no longer derives its bytes from `std::hash::Hash` but from `rsos`'s own canonical serde
// encoding (`rsos::encoding`), so the two properties above now exercise that path on every run.
// These pin what the encoding itself owes the protocol, over a *rich* generatively explored value
// type rather than the `u8`/`u16` pairs above: distinct elements never collide (injectivity — what
// a range fingerprint's soundness rests on), and unordered containers summarize independently of
// their iteration order.
// ---------------------------------------------------------------------------

/// A value shape reaching most arms of the encoding at once: an enum with several variant kinds, a
/// length-prefixed string, an optional, and a nested sequence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
enum Shape {
    Unit,
    Num(u32),
    Pair(u16, u16),
    Text { s: String, tail: Option<u8> },
    Nested(Vec<Vec<u8>>),
}

fn shape_strategy() -> impl Strategy<Value = Shape> {
    prop_oneof![
        Just(Shape::Unit),
        any::<u32>().prop_map(Shape::Num),
        (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Shape::Pair(a, b)),
        ("[a-c]{0,8}", any::<Option<u8>>()).prop_map(|(s, tail)| Shape::Text { s, tail }),
        prop::collection::vec(prop::collection::vec(any::<u8>(), 0..4), 0..4)
            .prop_map(Shape::Nested),
    ]
}

proptest! {
    /// Injectivity: two elements collide in fingerprint only if they *are* the same element. Not a
    /// claim about BLAKE3 — this checks the encoding does not erase a difference before BLAKE3
    /// ever sees it, which is precisely the framing bug an unprefixed encoding would have.
    #[test]
    fn lift_is_injective_over_rich_values(
        ka in any::<u32>(),
        kb in any::<u32>(),
        va in shape_strategy(),
        vb in shape_strategy(),
    ) {
        let same_element = ka == kb && va == vb;
        prop_assert_eq!(lift(&ka, &va) == lift(&kb, &vb), same_element);
    }

    /// An unordered map summarizes by content, not by iteration order — and agrees with the
    /// ordered map holding the same entries. Neither was expressible under the old `Hash` bound,
    /// which `HashMap` does not satisfy at all.
    #[test]
    fn unordered_maps_lift_independently_of_iteration_order(
        entries in prop::collection::vec((any::<u16>(), shape_strategy()), 0..24),
    ) {
        use std::collections::{BTreeMap, HashMap};

        // Deduplicate first, so the three collections below genuinely hold the same content
        // regardless of the direction they are built in.
        let ordered: BTreeMap<u16, Shape> = entries.into_iter().collect();
        let forward: HashMap<u16, Shape> =
            ordered.iter().map(|(k, v)| (*k, v.clone())).collect();
        let backward: HashMap<u16, Shape> =
            ordered.iter().rev().map(|(k, v)| (*k, v.clone())).collect();

        prop_assert_eq!(lift(&0u8, &forward), lift(&0u8, &backward));
        prop_assert_eq!(lift(&0u8, &forward), lift(&0u8, &ordered));
    }
}
