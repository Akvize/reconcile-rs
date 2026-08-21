// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The protocol driver: [`initial_ranges`], [`protocol_round`], and the [`RangeAggregate`] wire
//! type they exchange — generic over [`RsosView`], never over a concrete store.
//!
//! Split across siblings by concern: `bounds` owns `StartBound`/`EndBound`'s conversion to
//! [`std::ops::Bound`] and `KeyRange`'s construction and `RangeBounds` implementation; `rank` owns
//! resolving a wire range against a concrete store — admission and clamping arithmetic, and the
//! one way it can fail on a malformed segment; `range_aggregate` owns [`RangeAggregate`]'s own
//! construction and field readers; `outcome` owns [`RoundOutcome`]'s accessors and its
//! [`AddAssign`](std::ops::AddAssign). This file keeps the public type definitions (their module
//! location is their `cargo public-api`-visible path — see AGENTS.md §11) plus the round-driving
//! logic itself: [`initial_ranges`], [`protocol_round`] and [`protocol_round_with_policy`].

use std::ops::Bound;

use serde::{Deserialize, Serialize};
use tracing::debug;

use rsos::Aggregate;

use crate::policy::{Comparison, Decision, FanOut, FixedFanOut, RefinementPolicy};
use crate::rsos_view::RsosView;

mod bounds;
mod outcome;
mod range_aggregate;
mod rank;

use rank::{BoundedRange, InvertedRange};

/// The refinement policy [`protocol_round`] applies. Costs: `benches/protocol.rs`; the evidence
/// for this default: `SOTA.md` §2.2.
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

/// A [`RangeAggregate`]'s range. A local tuple struct, not a bare tuple, so it can implement the
/// foreign `RangeBounds` and feed [`RsosView::aggregate`] directly.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct KeyRange<K>(StartBound<K>, EndBound<K>);

/// A `KeyRange` paired with the [`Aggregate`] over it: the unit the RBSR protocol exchanges.
///
/// # Wire layout
///
/// bincode inlines both fields positionally, in declaration order, with no length prefix or tag
/// on the struct itself — only its two fields carry framing:
///
/// 1. `range.0` (the start bound): a `u32` variant tag (`0` = `Unbounded`, `1` = `Included`),
///    followed by the key `K`'s own encoding when `Included`.
/// 2. `range.1` (the end bound): the same shape (`0` = `Unbounded`, `1` = `Excluded`).
/// 3. `aggregate`: [`Aggregate`]'s own fields, in *its* declaration order — currently
///    `fingerprint` (four `u64` limbs) then `size` (a `u64`), each bincode's variable-length
///    integer encoding.
///
/// This layout is pinned by a golden vector in `reconcile`'s `tests/wire_format.rs`; reordering
/// any field here or in [`Aggregate`] is a protocol break, not a refactor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RangeAggregate<K> {
    range: KeyRange<K>,
    aggregate: Aggregate,
}

/// A range whose contents this peer must send explicitly: the **IDLIST** outcome, to be fed to
/// [`rsos::Rsos::enumerate`] by the caller — the driver itself never enumerates.
///
/// A bare pair of [`Bound`]s, not the narrowed wire types: this is a local output, never sent.
pub type EnumerationRange<K> = (Bound<K>, Bound<K>);

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
/// [`AddAssign`](std::ops::AddAssign) accumulates a whole reconciliation.
///
/// ```
/// use rsos::FingerprintTreeMap;
/// use rbsr::{initial_ranges, protocol_round};
///
/// // Three active ranges against the same responder `b`, chosen to hit SKIP, IDLIST and SPLIT in
/// // one round: `b` matches itself, an empty store advertises against non-empty `b`, and a
/// // same-sized-but-disjoint `c` mismatches `b`.
/// let mut b = FingerprintTreeMap::new();
/// for i in 0..40 {
///     b.insert(i, i);
/// }
/// let empty: FingerprintTreeMap<i32, i32> = FingerprintTreeMap::new();
/// let mut c = FingerprintTreeMap::new();
/// for i in 0..40 {
///     c.insert(i + 1000, i); // same count as `b`, disjoint keys -> different fingerprint
/// }
///
/// let mut active = initial_ranges(&b);
/// active.extend(initial_ranges(&empty));
/// active.extend(initial_ranges(&c));
///
/// let mut children = Vec::new();
/// let mut enumerations = Vec::new();
/// let outcome = protocol_round(&b, active, &mut children, &mut enumerations);
///
/// assert_eq!(outcome.skipped(), 1); // `b` vs `b`
/// assert_eq!(outcome.enumerated(), 1); // `b` vs `empty`
/// assert_eq!(outcome.split(), 1); // `b` vs `c`
/// assert_eq!(outcome.children(), children.len()); // every SPLIT/bounced child, tallied
/// assert_eq!(outcome.dropped_malformed(), 0); // no inverted range in this round
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RoundOutcome {
    skipped: usize,
    enumerated: usize,
    split: usize,
    children: usize,
    dropped_malformed: usize,
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
/// protocol break. Whatever the policy, the driver keeps `ARCHITECTURE.md` §5 invariant 10: a
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
///
/// A `Split` this policy returns for a range of more than one local element is trusted to narrow
/// it ([`RefinementPolicy`]'s progress law); one that would not is converted to an `Enumerate`
/// instead of reaching the fan-out below, whatever policy produced it (`ARCHITECTURE.md` §5
/// invariant 13, #420) — the driver stays liveness-safe even against a policy that breaks the
/// law, at the cost of an immediate IDLIST for the ranges where it does.
///
/// ```
/// use rsos::FingerprintTreeMap;
/// use rbsr::{initial_ranges, protocol_round_with_policy, SqrtFanOut};
///
/// let mut a = FingerprintTreeMap::new();
/// let mut b = FingerprintTreeMap::new();
/// for i in 0..400 {
///     a.insert(i, i);
///     b.insert(i, i);
/// }
/// b.insert(999, 999); // only `b` has this key, so the outer range mismatches and must split
///
/// let active = initial_ranges(&b);
/// let mut children = Vec::new();
/// let mut enumerations = Vec::new();
/// protocol_round_with_policy(&a, &SqrtFanOut, active, &mut children, &mut enumerations);
///
/// // `SqrtFanOut` cuts `a`'s 400-element span into exactly ⌊√400⌋ = 20 children, unlike the
/// // default `FixedFanOut`, which would cap it at 16 regardless of the span.
/// assert_eq!(children.len(), 20);
/// assert!(enumerations.is_empty());
/// ```
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
            Err(InvertedRange { raw_start, raw_end }) => {
                debug!(
                    "dropping malformed segment: its start ranks after its end in this store \
                     ({raw_start} > {raw_end}), so it covers no keys and cannot be refined"
                );
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
        // The policy never sees the bounds, so it cannot decide key-dependently. `Comparison::span`
        // is read from the bundled aggregate, not from `end_index - start_index` — see
        // `RsosView`'s count-agreement law for why those two agree only for a defended backend.
        let comparison = Comparison::new(local_aggregate, remote, outcome.children);
        let span = comparison.span();
        let decision = match policy.decide(comparison) {
            Decision::Split(stride) if span > 1 && stride.get() >= span => {
                debug!(
                    "policy returned a non-progressing SPLIT (stride {} >= span {span} with \
                     span > 1); forcing IDLIST for this range instead of stalling on it",
                    stride.get()
                );
                Decision::Enumerate
            }
            other => other,
        };
        match decision {
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
                    // `None` means the next cut would reach `end_index`: this child is the last.
                    // `Some` is in bounds for any backend by construction — see `AdmittedRank`.
                    let Some(next_index) = cur_index.cut_before(end_index, stride) else {
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
                    };
                    let next_key = local.select(next_index.get()).clone();
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
    outcome
}

#[cfg(test)]
mod tests;
