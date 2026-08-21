// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ops::RangeBounds;

use super::*;

use rsos::{Fingerprint, FingerprintTreeMap};

use crate::policy::{EnumerateBelowThreshold, SplitStride, SqrtFanOut};

/// A real `FingerprintTreeMap` over the given keys.
fn tree(keys: &[i32]) -> FingerprintTreeMap<i32, i32> {
    FingerprintTreeMap::from_iter(keys.iter().map(|&k| (k, 0)))
}

/// Run one crafted segment through `protocol_round`, via the trait rather than the tree.
fn round<B: RsosView<i32>>(
    store: &B,
    segment: RangeAggregate<i32>,
) -> (Vec<RangeAggregate<i32>>, Vec<EnumerationRange<i32>>) {
    let mut child_ranges = Vec::new();
    let mut enumeration_ranges = Vec::new();
    protocol_round(
        store,
        vec![segment],
        &mut child_ranges,
        &mut enumeration_ranges,
    );
    (child_ranges, enumeration_ranges)
}

// ----- Malformed wire segments -----
// Bad bound *shapes* are unrepresentable (`StartBound`/`EndBound`); only inversion is left.

/// An inverted range must be dropped, not answered: it would underflow and then `select` out
/// of bounds.
#[test]
fn inverted_range_is_dropped_not_panicking() {
    let store = tree(&[10, 20, 30]);
    let segment = RangeAggregate {
        range: KeyRange::new(StartBound::Included(100), EndBound::Excluded(5)),
        aggregate: Aggregate::new(1, Fingerprint([1, 0, 0, 0])),
    };
    let (child_ranges, enumeration_ranges) = round(&store, segment);
    assert!(child_ranges.is_empty());
    assert!(enumeration_ranges.is_empty());
}

// ----- Contract-violating backends -----

/// Breaks [`RsosView`]'s rank-within-store law: `rank` is the key's own magnitude, unbounded
/// by `size()`.
///
/// Hand-written rather than blanket-derived — the third-party shape the crate root advertises
/// (remote, lazy, cached), and the one the blanket impl does not cover. `select` indexes a
/// `Vec`, so it panics out of bounds; that is the trap the driver must not spring.
struct UnclampedRank {
    keys: Vec<i32>,
}

impl RsosView<i32> for UnclampedRank {
    fn size(&self) -> usize {
        self.keys.len()
    }

    fn aggregate<R: RangeBounds<i32>>(&self, _range: R) -> Aggregate {
        // Never equal to what the peer advertises below, so the driver reaches SPLIT.
        Aggregate::new(self.keys.len(), Fingerprint([7, 0, 0, 0]))
    }

    fn rank(&self, z: &i32) -> usize {
        *z as usize
    }

    fn select(&self, r: usize) -> &i32 {
        &self.keys[r]
    }
}

/// Worked example behind `no_backend_answer_can_drive_the_protocol_out_of_bounds` (the property
/// oracle). Returning at all is the assertion; `!is_empty()` keeps it failing if the bound is
/// ever "fixed" by dropping such segments instead, which is a different bug, not a milder one.
#[test]
fn backend_with_unclamped_rank_is_defended_against_not_trusted() {
    let store = UnclampedRank {
        keys: vec![10, 20, 30],
    };
    let segment = RangeAggregate {
        // rank(1000) = 1000 against size() = 3. Unclamped, the fan-out reaches `select(3)`.
        range: KeyRange::new(StartBound::Unbounded, EndBound::Excluded(1000)),
        aggregate: Aggregate::new(1, Fingerprint([1, 0, 0, 0])),
    };
    let (child_ranges, _) = round(&store, segment);
    assert!(!child_ranges.is_empty());
}

/// The guard must not reject legitimate segments: a well-formed range still produces the
/// normal output (here: an empty peer range, so our whole tree is reported as a difference).
#[test]
fn wellformed_segment_still_processed() {
    let store = tree(&[10, 20, 30]);
    let segment = RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: Aggregate::ZERO,
    };
    let (_child_ranges, enumeration_ranges) = round(&store, segment);
    assert_eq!(
        enumeration_ranges,
        vec![(Bound::Unbounded, Bound::Unbounded)]
    );
}

// ----- Emptiness and equality decided on `size`, never the fingerprint alone -----

/// A non-empty peer range fingerprinting to `ZERO` against our empty tree: same
/// fingerprint, different size. Must be bounced back, not concluded in sync.
#[test]
fn nonempty_zero_fingerprint_vs_empty_is_not_in_sync() {
    let store = tree(&[]); // empty: local fingerprint == ZERO, local size == 0
    let segment = RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: Aggregate::new(2, Fingerprint::ZERO),
    };
    let (child_ranges, enumeration_ranges) = round(&store, segment);
    assert!(enumeration_ranges.is_empty());
    assert_eq!(child_ranges.len(), 1);
    assert_eq!(
        child_ranges[0],
        RangeAggregate {
            range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
            aggregate: Aggregate::ZERO,
        }
    );
}

/// A genuinely identical range must still be concluded in sync.
#[test]
fn matching_fingerprint_and_size_is_in_sync() {
    let store = tree(&[10, 20, 30]);
    let segment = RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: store.aggregate(..),
    };
    let (child_ranges, enumeration_ranges) = round(&store, segment);
    assert!(child_ranges.is_empty());
    assert!(enumeration_ranges.is_empty());
}

// ----- The fan-out rule: this crate's communication cost, pinned -----
fn splitting_segment(m: usize) -> RangeAggregate<i32> {
    RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: Aggregate::new(m, Fingerprint([7, 0, 0, 0])),
    }
}

/// The default fan-out is a constant `b` whatever the range's size.
#[test]
fn default_split_fan_out_is_constant_at_sixteen() {
    for m in [100usize, 400, 2_500, 250_000] {
        let store = tree(&(0..m as i32).collect::<Vec<_>>());
        let (child_ranges, enumeration_ranges) = round(&store, splitting_segment(m));
        assert!(enumeration_ranges.is_empty());
        assert!(
            child_ranges.len() <= FanOut::NEGENTROPY.get(),
            "m={m}: SPLIT emitted {} children, expected at most b={} \
             (a size-dependent fan-out would grow with m)",
            child_ranges.len(),
            FanOut::NEGENTROPY.get()
        );
        assert!(child_ranges.len() > 1, "m={m}: the split must refine");
    }
}

/// `SqrtFanOut` is public API, so its cut positions are a contract.
#[test]
fn sqrt_fan_out_is_still_the_square_root_of_the_range_size() {
    for m in [100usize, 400, 2_500] {
        let store = tree(&(0..m as i32).collect::<Vec<_>>());
        let mut child_ranges = Vec::new();
        let mut enumeration_ranges = Vec::new();
        protocol_round_with_policy(
            &store,
            &SqrtFanOut,
            vec![splitting_segment(m)],
            &mut child_ranges,
            &mut enumeration_ranges,
        );
        assert!(enumeration_ranges.is_empty());
        let root = (m as f64).sqrt() as usize;
        assert!(
            child_ranges.len() >= root / 2 && child_ranges.len() <= root * 2,
            "m={m}: SPLIT emitted {} children, expected ~√m = {root}",
            child_ranges.len()
        );
    }
}

/// `ARCHITECTURE.md` §5 invariant 10, under any policy.
#[test]
fn split_children_partition_the_parent_range() {
    let store = tree(&(0..400).collect::<Vec<_>>());
    let (child_ranges, _) = round(&store, splitting_segment(400));
    assert!(child_ranges.len() > 1);

    let first = &child_ranges[0].range;
    assert_eq!(first.0, StartBound::Unbounded, "partition must start at −∞");
    let last = &child_ranges[child_ranges.len() - 1].range;
    assert_eq!(last.1, EndBound::Unbounded, "partition must end at +∞");

    for pair in child_ranges.windows(2) {
        let (left, right) = (&pair[0].range, &pair[1].range);
        match (&left.1, &right.0) {
            (EndBound::Excluded(end), StartBound::Included(start)) => assert_eq!(end, start),
            other => panic!("children are not adjacent: {other:?}"),
        }
    }
}

// ----- Convergence, under every policy and under mixed pairs -----

/// Reconcile two stores to a fixed point, applying IDLISTs the way `reconcile`'s engine does.
/// Policies are supplied separately so a mixed pair can be driven. Returns the message count.
fn drive(
    a: &mut FingerprintTreeMap<i32, i32>,
    b: &mut FingerprintTreeMap<i32, i32>,
    a_policy: &dyn RefinementPolicy,
    b_policy: &dyn RefinementPolicy,
) -> usize {
    let mut active = initial_ranges(&*a);
    let mut responder_is_b = true;
    let mut messages = 0;
    while !active.is_empty() {
        messages += 1;
        let mut children = Vec::new();
        let mut enumerations = Vec::new();
        let items: Vec<(i32, i32)> = {
            let (responder, policy) = if responder_is_b {
                (&*b, b_policy)
            } else {
                (&*a, a_policy)
            };
            protocol_round_with_policy(responder, policy, active, &mut children, &mut enumerations);
            enumerations
                .into_iter()
                .flat_map(|range| {
                    responder
                        .range(range)
                        .map(|(k, v)| (*k, *v))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let receiver = if responder_is_b { &mut *a } else { &mut *b };
        for (key, value) in items {
            receiver.insert(key, value);
        }
        active = children;
        responder_is_b = !responder_is_b;
        assert!(
            messages < 10_000,
            "reconciliation failed to converge — the refinement is not shrinking"
        );
    }
    messages
}

/// Reconcile two stores under a pair of policies; both must end holding exactly the union.
fn assert_converges(
    keys_a: &[i32],
    keys_b: &[i32],
    a_policy: &dyn RefinementPolicy,
    b_policy: &dyn RefinementPolicy,
) {
    let (mut a, mut b) = (tree(keys_a), tree(keys_b));
    drive(&mut a, &mut b, a_policy, b_policy);

    let mut union: Vec<i32> = keys_a.iter().chain(keys_b).copied().collect();
    union.sort_unstable();
    union.dedup();
    let contents =
        |t: &FingerprintTreeMap<i32, i32>| t.range(..).map(|(k, _)| *k).collect::<Vec<_>>();
    assert_eq!(contents(&a), union, "a did not converge on the union");
    assert_eq!(contents(&b), union, "b did not converge on the union");
    assert_eq!(a.aggregate(..), b.aggregate(..));
}

/// Scattered, clustered and degenerate differences — the shapes that pull policies apart.
fn corpora() -> Vec<(&'static str, Vec<i32>, Vec<i32>)> {
    let full: Vec<i32> = (0..500).collect();
    vec![
        ("both empty", vec![], vec![]),
        ("one side empty", full.clone(), vec![]),
        ("identical", full.clone(), full.clone()),
        (
            "one differing element",
            full.clone(),
            full.iter().copied().filter(|k| *k != 250).collect(),
        ),
        (
            "scattered differences",
            full.clone(),
            full.iter().copied().filter(|k| k % 37 != 0).collect(),
        ),
        (
            "clustered differences",
            full.clone(),
            full.iter()
                .copied()
                .filter(|k| !(200..250).contains(k))
                .collect(),
        ),
        (
            "disjoint halves",
            full.iter().copied().filter(|k| k % 2 == 0).collect(),
            full.iter().copied().filter(|k| k % 2 == 1).collect(),
        ),
    ]
}

/// A policy that behaves like [`FixedFanOut`] except it never actually narrows a range once a
/// real cut is possible (`span() > 1`) — it asks for a stride wider than any span instead.
/// `ARCHITECTURE.md` §5 invariant 13 (#420): included in [`policies`] so the driver's guard,
/// not this policy's own hygiene, is what the convergence matrix below is proving. Without
/// that guard this would hang exactly like #356's `FingerprintDerivedSplit` probe.
#[derive(Clone, Copy, Debug, Default)]
struct NeverNarrows;

impl RefinementPolicy for NeverNarrows {
    fn decide(&self, comparison: Comparison) -> Decision {
        match FixedFanOut::default().decide(comparison) {
            Decision::Split(_) if comparison.span() > 1 => {
                Decision::Split(SplitStride::per_child(usize::MAX))
            }
            other => other,
        }
    }
}

fn policies() -> Vec<(&'static str, Box<dyn RefinementPolicy>)> {
    vec![
        ("SqrtFanOut", Box::new(SqrtFanOut)),
        ("FixedFanOut(2)", Box::new(FixedFanOut::new(FanOut::BINARY))),
        (
            "FixedFanOut(16)",
            Box::new(FixedFanOut::new(FanOut::NEGENTROPY)),
        ),
        (
            "EnumerateBelow(t=32,b=16)",
            Box::new(EnumerateBelowThreshold::PAPER),
        ),
        (
            "EnumerateBelow(t=1,b=2)",
            Box::new(EnumerateBelowThreshold::new(1, FanOut::BINARY)),
        ),
        ("NeverNarrows", Box::new(NeverNarrows)),
    ]
}

/// `ARCHITECTURE.md` §5 invariant 13 (#420), isolated to one round: a policy asking for a
/// stride that would not narrow a `span() > 1` range must not reach the fan-out loop as a
/// `Split` at all — it is answered as an `Enumerate`, counted and bounced back exactly like a
/// policy that had returned `Enumerate` itself.
#[test]
fn non_progressing_split_is_converted_to_enumerate() {
    let store = tree(&(0..10).collect::<Vec<_>>()); // span = 10, so span() > 1
    let segment = RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: Aggregate::new(5, Fingerprint([9, 9, 9, 9])), // non-empty, disagrees
    };
    let mut child_ranges = Vec::new();
    let mut enumeration_ranges = Vec::new();
    let outcome = protocol_round_with_policy(
        &store,
        &NeverNarrows,
        vec![segment],
        &mut child_ranges,
        &mut enumeration_ranges,
    );
    assert_eq!(outcome.split(), 0, "must not be counted as a SPLIT");
    assert_eq!(
        outcome.enumerated(),
        1,
        "must be counted as an IDLIST instead"
    );
    assert_eq!(enumeration_ranges.len(), 1);
    // The peer's range was non-empty, so IDLIST's one-directional bounce-back applies here
    // exactly as it would for a policy that had returned `Decision::Enumerate` directly.
    assert_eq!(child_ranges.len(), 1);
    assert_eq!(child_ranges[0].aggregate, Aggregate::ZERO);
}

#[test]
fn every_policy_reconciles_every_corpus() {
    for (policy_name, policy) in policies() {
        for (corpus, keys_a, keys_b) in corpora() {
            println!("{policy_name} / {corpus}");
            assert_converges(&keys_a, &keys_b, policy.as_ref(), policy.as_ref());
        }
    }
}

/// Peers running different policies must converge; otherwise the policy has leaked onto the
/// wire.
#[test]
fn peers_running_different_policies_still_converge() {
    for (a_name, a_policy) in policies() {
        for (b_name, b_policy) in policies() {
            for (corpus, keys_a, keys_b) in corpora() {
                println!("{a_name} vs {b_name} / {corpus}");
                assert_converges(&keys_a, &keys_b, a_policy.as_ref(), b_policy.as_ref());
            }
        }
    }
}

/// The tally must add up and must attribute the malformed range, not swallow it.
#[test]
fn round_outcome_accounts_for_every_segment() {
    let store = tree(&[10, 20, 30, 40, 50]);
    let mut child_ranges = Vec::new();
    let mut enumeration_ranges = Vec::new();
    let outcome = protocol_round(
        &store,
        vec![
            RangeAggregate {
                range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                aggregate: store.aggregate(..),
            },
            RangeAggregate {
                range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                aggregate: Aggregate::ZERO,
            },
            RangeAggregate {
                range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                aggregate: Aggregate::new(5, Fingerprint([7, 0, 0, 0])),
            },
            RangeAggregate {
                range: KeyRange::new(StartBound::Included(100), EndBound::Excluded(5)),
                aggregate: Aggregate::new(1, Fingerprint([1, 0, 0, 0])),
            },
        ],
        &mut child_ranges,
        &mut enumeration_ranges,
    );
    assert_eq!(outcome.skipped(), 1);
    assert_eq!(outcome.enumerated(), 1);
    assert_eq!(outcome.split(), 1);
    assert_eq!(outcome.dropped_malformed(), 1);
    assert_eq!(outcome.children(), child_ranges.len());
    assert_eq!(enumeration_ranges.len(), outcome.enumerated());
}

/// `RoundOutcome` is otherwise only ever constructed fresh by `protocol_round` (never combined
/// with `+=` internally), so its `AddAssign` — accumulating a whole reconciliation across rounds
/// — needs its own direct witness, not just the fresh-construction totals above.
#[test]
fn add_assign_sums_every_field() {
    let mut a = RoundOutcome {
        skipped: 1,
        enumerated: 2,
        split: 3,
        children: 4,
        dropped_malformed: 5,
    };
    let b = RoundOutcome {
        skipped: 10,
        enumerated: 20,
        split: 30,
        children: 40,
        dropped_malformed: 50,
    };
    a += b;
    assert_eq!(a.skipped(), 11);
    assert_eq!(a.enumerated(), 22);
    assert_eq!(a.split(), 33);
    assert_eq!(a.children(), 44);
    assert_eq!(a.dropped_malformed(), 55);
}

/// Matching fingerprints with mismatched sizes must refine, not conclude in sync.
#[test]
fn matching_fingerprint_but_wrong_size_is_refined() {
    let store = tree(&[10, 20, 30, 40, 50]);
    let segment = RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: Aggregate::new(store.len() + 7, store.aggregate(..).fingerprint()),
    };
    let (child_ranges, enumeration_ranges) = round(&store, segment);
    assert!(!child_ranges.is_empty());
    assert!(enumeration_ranges.is_empty());
}
