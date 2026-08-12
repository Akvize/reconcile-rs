// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The protocol driver: [`initial_ranges`], [`protocol_round`], and the [`RangeAggregate`] wire
//! type they exchange — generic over [`RsosView`], never over a concrete store.

use std::ops::{AddAssign, Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use rsos::Aggregate;

use crate::policy::{Comparison, Decision, FanOut, FixedFanOut, RefinementPolicy};
use crate::rsos_view::RsosView;

/// The refinement policy [`protocol_round`] applies. Costs: `benches/protocol.rs`; the evidence
/// for this default: `PROGRESS.md`.
const DEFAULT_POLICY: FixedFanOut = FixedFanOut::new(FanOut::NEGENTROPY);

/// The start bound of a [`RangeAggregate`] range: `Included` or `Unbounded`, never `Excluded`.
///
/// Narrower than `std::ops::Bound<K>` so a peer sending the third shape fails to deserialize
/// rather than reaching a runtime check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum StartBound<K> {
    Unbounded,
    Included(K),
}

/// The end bound of a [`RangeAggregate`] range: `Excluded` or `Unbounded`, never `Included`. See
/// [`StartBound`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum EndBound<K> {
    Unbounded,
    Excluded(K),
}

impl<K> From<StartBound<K>> for Bound<K> {
    fn from(bound: StartBound<K>) -> Bound<K> {
        match bound {
            StartBound::Unbounded => Bound::Unbounded,
            StartBound::Included(key) => Bound::Included(key),
        }
    }
}

impl<K> From<EndBound<K>> for Bound<K> {
    fn from(bound: EndBound<K>) -> Bound<K> {
        match bound {
            EndBound::Unbounded => Bound::Unbounded,
            EndBound::Excluded(key) => Bound::Excluded(key),
        }
    }
}

/// A [`RangeAggregate`]'s range. A local tuple struct, not a bare tuple, so it can implement the
/// foreign [`RangeBounds`] and feed [`RsosView::aggregate`] directly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct KeyRange<K>(StartBound<K>, EndBound<K>);

impl<K> KeyRange<K> {
    fn new(start: StartBound<K>, end: EndBound<K>) -> Self {
        KeyRange(start, end)
    }
}

impl<K> RangeBounds<K> for KeyRange<K> {
    fn start_bound(&self) -> Bound<&K> {
        match &self.0 {
            StartBound::Unbounded => Bound::Unbounded,
            StartBound::Included(key) => Bound::Included(key),
        }
    }

    fn end_bound(&self) -> Bound<&K> {
        match &self.1 {
            EndBound::Unbounded => Bound::Unbounded,
            EndBound::Excluded(key) => Bound::Excluded(key),
        }
    }
}

/// A `KeyRange` paired with the [`Aggregate`] over it: the unit the RBSR protocol exchanges.
///
/// The wire type. bincode inlines the nested `Aggregate` in field declaration order, so
/// [`Aggregate`]'s own field order is load-bearing here; the bytes are pinned by a golden vector
/// in `reconcile`'s `tests/wire_format.rs`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RangeAggregate<K> {
    range: KeyRange<K>,
    aggregate: Aggregate,
}

/// Test-only seam for out-of-crate wire-format oracles, behind `internal-testing`: builds a
/// segment with *chosen* bounds, which [`initial_ranges`] alone never produces.
///
/// `None` is unbounded, `Some(k)` is `Included(k)` on the start and `Excluded(k)` on the end —
/// an excluded start or included end stays unspellable.
#[cfg(feature = "internal-testing")]
impl<K> RangeAggregate<K> {
    pub fn for_testing(start: Option<K>, end: Option<K>, aggregate: Aggregate) -> Self {
        RangeAggregate {
            range: KeyRange::new(
                start.map_or(StartBound::Unbounded, StartBound::Included),
                end.map_or(EndBound::Unbounded, EndBound::Excluded),
            ),
            aggregate,
        }
    }
}

/// A range whose contents this peer must send explicitly: the **IDLIST** outcome, to be fed to
/// [`rsos::Rsos::enumerate`] by the caller — the driver itself never enumerates.
///
/// A bare pair of [`Bound`]s, not the narrowed wire types: this is a local output, never sent.
pub type EnumerationRange<K> = (Bound<K>, Bound<K>);

/// A [`RangeAggregate`]'s range checked against a concrete local set. Bound shapes are already
/// guaranteed by [`StartBound`]/[`EndBound`], so the one remaining malformation — an inverted
/// range — needs a set to be detected, hence a fallible constructor.
struct BoundedRange<K> {
    start: StartBound<K>,
    end: EndBound<K>,
    start_index: usize,
    end_index: usize,
}

/// The one way [`BoundedRange::parse`] can fail: the segment's start position is after its end
/// position in the set it was checked against.
struct InvertedRange;

impl<K> BoundedRange<K> {
    /// Absolute positions from [`RsosView::rank`], which the fan-out steps through with
    /// [`RsosView::select`] — an aggregate gives the count, not the positions.
    fn parse<B: RsosView<K>>(
        start: StartBound<K>,
        end: EndBound<K>,
        local: &B,
    ) -> Result<Self, InvertedRange> {
        let start_index = match &start {
            StartBound::Unbounded => 0,
            StartBound::Included(key) => local.rank(key),
        };
        let end_index = match &end {
            EndBound::Unbounded => local.size(),
            EndBound::Excluded(key) => local.rank(key),
        };
        if end_index < start_index {
            return Err(InvertedRange);
        }
        Ok(BoundedRange {
            start,
            end,
            start_index,
            end_index,
        })
    }
}

/// The initial family of **active ranges**: one [`RangeAggregate`] `{(−∞, +∞), A(whole store)}`.
///
/// The outer range is fixed to the whole universe here; [`protocol_round`] never assumes it, so
/// partial reconciliation needs only a different starting family.
pub fn initial_ranges<K, B: RsosView<K>>(local: &B) -> Vec<RangeAggregate<K>> {
    vec![RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: local.aggregate(..),
    }]
}

/// What one [`protocol_round`] did, tallied where the decisions are taken — the output vectors
/// cannot be diffed for it, since one range can appear in both and a dropped segment in neither.
///
/// [`AddAssign`] accumulates a whole reconciliation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RoundOutcome {
    skipped: usize,
    enumerated: usize,
    split: usize,
    children: usize,
    dropped_malformed: usize,
}

impl RoundOutcome {
    /// Ranges resolved outright (SKIP): written to neither output.
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// Ranges handed back for explicit enumeration (IDLIST).
    pub const fn enumerated(&self) -> usize {
        self.enumerated
    }

    /// Ranges refined (SPLIT). This counts *parents*, not children — see
    /// [`children`](Self::children).
    pub const fn split(&self) -> usize {
        self.split
    }

    /// Child ranges written to `child_ranges` across every outcome — what travels in the next
    /// batch, and what a policy sees through [`Comparison::children_emitted`].
    pub const fn children(&self) -> usize {
        self.children
    }

    /// Segments dropped without being answered because their bounds inverted once resolved against
    /// the local set. Non-zero means a peer is sending malformed input.
    pub const fn dropped_malformed(&self) -> usize {
        self.dropped_malformed
    }
}

impl AddAssign for RoundOutcome {
    fn add_assign(&mut self, other: RoundOutcome) {
        self.skipped += other.skipped;
        self.enumerated += other.enumerated;
        self.split += other.split;
        self.children += other.children;
        self.dropped_malformed += other.dropped_malformed;
    }
}

/// One **protocol round** under this crate's default refinement policy: classify every active range this peer was
/// asked to answer as SKIP, IDLIST or SPLIT.
///
/// A range's fate is read exhaustively off the outputs: `child_ranges` (SPLIT),
/// `enumeration_ranges` (IDLIST), **both** (an IDLIST against a non-empty peer range, which also
/// bounces the parent back), or neither (SKIP, or dropped as malformed and counted in
/// [`RoundOutcome::dropped_malformed`]).
///
/// The rule is a [`RefinementPolicy`], swappable through [`protocol_round_with_policy`] without a
/// protocol break. Whatever the policy, the driver keeps `ARCHITECTURE.md` §5 invariant 9: a
/// SPLIT's children are pairwise disjoint with union the parent.
pub fn protocol_round<K, B: RsosView<K>>(
    local: &B,
    active_ranges: Vec<RangeAggregate<K>>,
    child_ranges: &mut Vec<RangeAggregate<K>>,
    enumeration_ranges: &mut Vec<EnumerationRange<K>>,
) -> RoundOutcome
where
    K: Clone,
{
    protocol_round_with_policy(
        local,
        &DEFAULT_POLICY,
        active_ranges,
        child_ranges,
        enumeration_ranges,
    )
}

/// [`protocol_round`] with its default rule replaced.
///
/// The policy chooses the outcome and the split width, nothing else: bounds validation, rank
/// arithmetic, `select` cuts and the partition invariant stay here. `?Sized`, so
/// `&dyn RefinementPolicy` works.
pub fn protocol_round_with_policy<K, B, P>(
    local: &B,
    policy: &P,
    active_ranges: Vec<RangeAggregate<K>>,
    child_ranges: &mut Vec<RangeAggregate<K>>,
    enumeration_ranges: &mut Vec<EnumerationRange<K>>,
) -> RoundOutcome
where
    K: Clone,
    B: RsosView<K>,
    P: RefinementPolicy + ?Sized,
{
    let mut outcome = RoundOutcome::default();
    for segment in active_ranges {
        let RangeAggregate {
            range: KeyRange(start, end),
            aggregate: remote,
        } = segment;
        // Safe before validation: `aggregate` compares, never indexes, so an inverted range is
        // simply empty.
        let local_aggregate = local.aggregate(KeyRange::new(start.clone(), end.clone()));
        // Dropping an inverted range here avoids an underflow and an out-of-bounds `select`.
        let bounded = match BoundedRange::parse(start, end, local) {
            Ok(bounded) => bounded,
            Err(InvertedRange) => {
                debug!("dropping segment with inverted range");
                outcome.dropped_malformed += 1;
                continue;
            }
        };
        let start_index = bounded.start_index;
        let end_index = bounded.end_index;
        let BoundedRange {
            start: start_bound,
            end: end_bound,
            ..
        } = bounded;
        // The policy never sees the bounds, so it cannot decide key-dependently.
        let comparison = Comparison::new(local_aggregate, remote, outcome.children);
        match policy.decide(comparison) {
            Decision::Skip => {
                outcome.skipped += 1;
            }
            Decision::Enumerate => {
                // IDLIST is one-directional: a non-empty peer range is bounced back advertised as
                // empty so the peer enumerates its side too.
                outcome.enumerated += 1;
                if remote.size() != 0 {
                    child_ranges.push(RangeAggregate {
                        range: KeyRange::new(start_bound.clone(), end_bound.clone()),
                        aggregate: Aggregate::ZERO,
                    });
                    outcome.children += 1;
                }
                enumeration_ranges.push((start_bound.into(), end_bound.into()));
            }
            Decision::Split(stride) => {
                outcome.split += 1;
                let stride = stride.get();
                let mut cur_bound = start_bound;
                let mut cur_index = start_index;
                loop {
                    let next_index = cur_index + stride;
                    if next_index >= end_index {
                        let range = KeyRange::new(cur_bound, end_bound);
                        // An uncut child *is* the parent, whose aggregate is already in hand.
                        let aggregate = if cur_index == start_index {
                            local_aggregate
                        } else {
                            local.aggregate(range.clone())
                        };
                        child_ranges.push(RangeAggregate { range, aggregate });
                        outcome.children += 1;
                        break;
                    } else {
                        let next_key = local.select(next_index).clone();
                        let range = KeyRange::new(cur_bound, EndBound::Excluded(next_key.clone()));
                        let aggregate = local.aggregate(range.clone());
                        child_ranges.push(RangeAggregate { range, aggregate });
                        outcome.children += 1;
                        cur_bound = StartBound::Included(next_key);
                        cur_index = next_index;
                    }
                }
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    use rsos::{Fingerprint, FingerprintTreeMap};

    use crate::policy::{EnumerateBelowThreshold, SqrtFanOut};

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

    /// The guard must not reject well-formed segments.
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

    /// `ARCHITECTURE.md` §5 invariant 9, under any policy.
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
                protocol_round_with_policy(
                    responder,
                    policy,
                    active,
                    &mut children,
                    &mut enumerations,
                );
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
        ]
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
}
