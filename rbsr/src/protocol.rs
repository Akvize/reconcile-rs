// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The Range-Based Set Reconciliation protocol driver itself: [`initial_ranges`],
//! [`protocol_round`], and the [`RangeAggregate`] wire type they exchange.
//!
//! Both functions are free functions generic over [`RsosView`], the four read-only RSOS operations
//! the driver needs (`size`/`aggregate`/`rank`/`select`) — never over a concrete data structure.
//! See the crate root documentation for the algorithm, the paper-vocabulary correspondence table,
//! and the citations; [`protocol_round`]'s own documentation for the three protocol outcomes; and
//! [`RefinementPolicy`] for the decision rules that choose between them.
//!
//! This module is private — everything public here is re-exported from the crate root, so any
//! documentation a *user* needs belongs on an item, not in this header, which rustdoc never
//! renders.

use std::ops::{AddAssign, Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use rsos::Aggregate;

use crate::policy::{Comparison, Decision, FanOut, FixedFanOut, RefinementPolicy};
use crate::rsos_view::RsosView;

/// The refinement policy [`protocol_round`] applies: the paper's constant branching factor at
/// Negentropy's `b = 16`, with this crate's enumeration cutoffs.
///
/// Named rather than spelled inline so "what does this crate do by default" is one `grep`, and so
/// the switch away from [`SqrtFanOut`](crate::SqrtFanOut) — measured in `benches/protocol.rs`,
/// decided in [#257](https://github.com/Akvize/reconcile-rs/issues/257) — is a one-line diff if it
/// ever needs revisiting.
const DEFAULT_POLICY: FixedFanOut = FixedFanOut::new(FanOut::NEGENTROPY);

/// The start bound of a [`RangeAggregate`] range, as this protocol actually emits it: `Included` or
/// `Unbounded`, never `Excluded`.
///
/// `RangeAggregate` is deserialized straight off the wire (see the module docs on `protocol_round`'s
/// former validation). Narrowing this from `std::ops::Bound<K>` (three variants) to the two
/// shapes the protocol produces makes the third shape (`Excluded`) **unrepresentable**: a peer
/// sending it fails to deserialize — the same "malformed, drop the datagram" path
/// as any other corrupt input — instead of reaching a runtime check inside `protocol_round`.
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

/// A [`RangeAggregate`]'s range: `(StartBound<K>, EndBound<K>)`, wrapped in a local tuple struct
/// (rather than a bare tuple) so it can implement the foreign [`RangeBounds`] trait directly —
/// Rust's orphan rules require the outermost type to be local, and a plain tuple never qualifies.
/// This lets a segment's range feed straight into [`RsosView::aggregate`] like any other
/// `RangeBounds<K>`, with no intermediate conversion.
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

/// A `KeyRange` paired with the [`Aggregate`] of the elements it covers: what one peer
/// advertises about one range of its store, and the unit the RBSR protocol exchanges.
///
/// The two halves used to be spelled out here as `hash: Fingerprint` + `size: usize`, described by
/// this very doc comment as "allow testing whether two segments represent the same elements" —
/// which is the definition of Def. 3.5's bundled aggregate `A(S) = (|S|, Σ(S))`. They are one
/// [`Aggregate`] now, the same value [`RsosView::aggregate`] already returns, so a segment can no
/// longer be built with a count and a fingerprint that describe different sets.
///
/// # Wire compatibility
///
/// The encoding is **unchanged** by that collapse. bincode writes struct fields sequentially with
/// no framing or field names, so the nested `Aggregate` is inlined and
/// `{range, aggregate: {fingerprint, size}}` is byte-for-byte the old `{range, hash, size}` —
/// which is why [`Aggregate`] declares `fingerprint` before `size` (see its own note). The exact
/// bytes are pinned by a golden vector in `reconcile`'s `tests/wire_format.rs`, built through
/// `RangeAggregate::for_testing` (the `internal-testing` seam below, hence not linked here — it
/// does not exist without that feature) — the codec lives in the adapter layer, so the byte-level
/// test lives there too rather than dragging a codec dependency into this crate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RangeAggregate<K> {
    range: KeyRange<K>,
    aggregate: Aggregate,
}

/// Test-only seam for wire-format oracles living outside this crate, behind `internal-testing`
/// (the same pattern `reconcile::testing` uses, and never reachable from non-test code).
///
/// `RangeAggregate`'s fields are private and `KeyRange`/`StartBound`/`EndBound` are `pub(crate)`,
/// so a segment with *chosen* bounds cannot otherwise be built from another crate —
/// and chosen bounds are the point: a golden vector has to exercise `Included`/`Excluded`, which
/// [`initial_ranges`] alone never produces.
///
/// The bound *shapes* stay unrepresentable-if-wrong by construction, exactly as on the wire:
/// `None` means unbounded, `Some(k)` means `Included(k)` on the start and `Excluded(k)` on the
/// end. There is deliberately no way to spell an excluded start or an included end.
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

/// A range whose contents this peer must send explicitly — the paper's **IDLIST** outcome, where
/// "one peer explicitly sends the ordered contents of its local subset on `[l, u)`" (§4 of
/// arXiv:2603.19820).
///
/// The name lines up with Def. 3.9's `Enumerate(l, u)` operation, which is precisely what a caller
/// does with one of these: [`rsos::Rsos::enumerate`] returns the ordered contents of the range, and
/// those are what travel to the peer. [`RsosView`] deliberately omits `Enumerate` — the driver
/// itself never enumerates, it only *names* the ranges the caller must.
///
/// It is a bare pair of [`Bound`]s rather than the narrowed start/end bound types this module uses
/// on the wire, because it never crosses the wire: it is an *output* handed back to the local
/// caller, to be fed straight into a `RangeBounds`-taking range query.
pub type EnumerationRange<K> = (Bound<K>, Bound<K>);

/// A [`RangeAggregate`]'s range, checked against a concrete local set: the bound *shapes* are
/// already guaranteed by [`StartBound`]/[`EndBound`] (unrepresentable otherwise), so the only
/// remaining way a wire segment can be malformed is an inverted range (`start_index >
/// end_index`) — which can only be detected against a specific set, hence this being a fallible
/// constructor rather than a static property of the wire type.
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
    /// The two indices come from [`RsosView::rank`] of the two bound *keys* — genuinely
    /// rank-of-a-key, not a range aggregate: `aggregate` would hand back the range's element count
    /// but not the absolute positions the fan-out below steps through with
    /// [`RsosView::select`].
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

/// The initial family of **active ranges** that bootstraps a reconciliation: a single
/// [`RangeAggregate`] `{(−∞, +∞), A(whole store)}`, ready to be sent to a peer's
/// [`protocol_round`].
///
/// This is the paper's "one application-defined **outer range** covering the part of the universe
/// to be reconciled" (§4 of arXiv:2603.19820), with the application definition fixed here to the
/// whole universe — this crate reconciles entire stores, so the outer range is always
/// `(−∞, +∞)`. Everything downstream stays general: [`protocol_round`] never assumes an unbounded
/// range, so a caller wanting partial (prefix/subspace) reconciliation would only need a different
/// starting family, not a different round.
///
/// The fingerprint and the size come from a single [`RsosView::aggregate`] over `..` — Def. 3.5's
/// bundled `A(S) = (|S|, Σ(S))`, one traversal for both halves, so they cannot describe different
/// sets.
pub fn initial_ranges<K, B: RsosView<K>>(local: &B) -> Vec<RangeAggregate<K>> {
    vec![RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: local.aggregate(..),
    }]
}

/// What one [`protocol_round`] did, tallied where the decisions are taken rather than inferred
/// from the output vectors.
///
/// Three of the five numbers a caller could *almost* reconstruct by diffing the output `Vec`
/// lengths — almost, because one range can appear in both outputs. The fourth cannot be
/// reconstructed at all: a malformed segment is dropped with a bare `debug!`, which is a `tracing`
/// event, so a consumer on `log` or with no subscriber installed sees literally nothing. Returning
/// the tally is what makes "how many segments did that peer send me that did not parse" an
/// answerable question, and what lets `benches/protocol.rs` report per-round classification counts
/// for a policy rather than guessing them.
///
/// Adding across rounds is [`AddAssign`], so a driver can accumulate a whole reconciliation into
/// one value.
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

    /// Child ranges written to `child_ranges`, across every outcome: the SPLIT fan-outs plus the
    /// one-per-range bounce an IDLIST adds when the peer's range is non-empty.
    ///
    /// This is the quantity that travels in the next batch, so it is the one a datagram budget is
    /// spent in — the same count a policy sees through [`Comparison::children_emitted`].
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

/// One **protocol round** under this crate's default refinement policy, [`FixedFanOut`] at
/// `b = 16`: apply the responder step to every active range this peer was asked to answer,
/// classifying each as SKIP, IDLIST or SPLIT.
///
/// "A complete protocol round consists of each peer applying Algorithm 1 to the active ranges it is
/// asked to answer" (§4 of arXiv:2603.19820). `active_ranges` is that batch — what the peer sent,
/// each range paired with the peer's [`Aggregate`] over it. For each one, this peer recomputes its
/// own aggregate and takes exactly one of the three outcomes:
///
/// - **SKIP** — the two aggregates agree, so the range is *resolved*: it is written to neither
///   output and simply disappears from the active family.
/// - **IDLIST** — the range is pushed to `enumeration_ranges`. The caller is expected to enumerate
///   its local contents there (Def. 3.9's `Enumerate(l, u)`) and send them to the peer.
/// - **SPLIT** — the range is replaced by a balanced family of **child ranges**, each pushed to
///   `child_ranges` with this peer's aggregate over it, to be bounced back to the peer and answered
///   in the next round. The cuts are chosen by rank via [`RsosView::select`], so the children are
///   pairwise disjoint and their union is the parent range.
///
/// Reading a range's fate off the outputs is exhaustive: it is in `child_ranges` (SPLIT), in
/// `enumeration_ranges` (IDLIST), in **both** (an IDLIST against a peer that holds something here,
/// which also bounces the range back so the peer enumerates its side), or in neither (SKIP).
/// Malformed input joins the last of those: a range whose bounds invert once resolved against the
/// local set is dropped rather than answered, and counted in the returned
/// [`RoundOutcome`](RoundOutcome::dropped_malformed).
///
/// # Which rule decides, and how to change it
///
/// *Which* of the three outcomes a range takes is not fixed by this function. It is a
/// [`RefinementPolicy`] — a purely local decision, never negotiated on the wire — and this entry
/// point pins it to [`FixedFanOut`] at [`FanOut::NEGENTROPY`], which leaves exactly one deviation
/// from Algorithm 1:
///
/// - **no enumeration threshold `t`.** The paper takes IDLIST whenever `|X ∩ [l, u)| ≤ t` for a
///   fixed parameter `t` (its experiments use `t = 32`); this crate uses four hand-picked special
///   cases instead, listed on [`SqrtFanOut`](crate::SqrtFanOut). They are not a byte-saving:
///   enumerating a range of up
///   to `t` elements ships values the peer mostly already holds, which is why
///   [`EnumerateBelowThreshold`](crate::EnumerateBelowThreshold) — Algorithm 1 as written, for
///   anyone who wants it — is not the default either.
/// - **the branching factor is the paper's.** `SPLITBYRANK(O_X, l, u, b)` produces a `b`-balanced
///   partition for a constant `b`, and `b = 16` is Negentropy's, arXiv:2603.19820 §6's comparison
///   point, and the value `benches/protocol.rs`'s sweep settles on. Because `b` is constant, the
///   paper's local-cost bound `T_loc = O(hL + bhI + K)` describes this crate again.
///
/// Until 2026-08 the default was [`SqrtFanOut`](crate::SqrtFanOut), whose fan-out grew as `⌊√m⌋`;
/// it is still shipped,
/// and the measurement that retired it is on its own documentation. Use
/// [`protocol_round_with_policy`] to run it, or any other rule — the wire type carries no policy,
/// so two peers running different ones still converge, and a cluster can therefore be migrated one
/// node at a time.
///
/// What the driver keeps regardless of policy is Proposition 4.1's requirement: a SPLIT's children
/// are pairwise disjoint and their union is the parent range.
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

/// One **protocol round** under a caller-supplied [`RefinementPolicy`] — [`protocol_round`] with
/// its default rule replaced.
///
/// The policy chooses between SKIP, IDLIST and SPLIT, and how wide a SPLIT cuts. It chooses nothing
/// else: the bounds validation, the rank arithmetic, the `select` cuts and the partition invariant
/// stay here, so no policy can produce children that overlap or leave a gap. See
/// [`RefinementPolicy`] for what a policy may and may not assume, and why swapping one is not a
/// protocol break.
///
/// `policy` is `?Sized`, so `&dyn RefinementPolicy` works as well as a concrete type — useful for a
/// benchmark or a runtime-configured driver that holds several.
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
        // One bundled `Aggregate` (Def. 3.5) for both halves of what this range looks like
        // locally: its element count *and* its fingerprint, in a single traversal. Safe on any
        // bound combination — including a not-yet-validated inverted range — because `aggregate`
        // walks via point/range comparisons, never index arithmetic (an inverted range simply
        // aggregates to the empty set).
        let local_aggregate = local.aggregate(KeyRange::new(start.clone(), end.clone()));
        // The bound *shapes* are already guaranteed by `StartBound`/`EndBound`: a peer sending
        // anything else fails to deserialize before `protocol_round` ever runs (see their doc
        // comments). The one remaining way a wire segment can be malformed is an inverted range
        // (`start_index > end_index`, e.g. `Included(100)..Excluded(5)`) — undetectable without a
        // concrete local set, hence the fallible constructor rather than a static property of the
        // wire type. Dropping it here avoids the underflow/out-of-bounds `select` that trusting
        // the arithmetic would cause.
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
        // The policy sees the two aggregates and the round's running child count, and nothing else
        // — notably not the bounds, so it cannot make a key-dependent decision the peer would
        // disagree with. `Comparison::span()` is `local_aggregate.size()`, which is the same
        // quantity as `end_index - start_index`: `rank` counts the keys strictly below a bound, so
        // the difference of the two ranks *is* the number of keys in `[start, end)`. It is read
        // from the bundled aggregate rather than from that subtraction so the count and the
        // fingerprint it is compared alongside come from one and the same traversal.
        //
        // NOTE: emptiness and equality are decided on the exact element counts, never on the
        // fingerprints alone — see `Comparison::agrees`, which owns that comparison so no policy
        // has to re-derive it.
        let comparison = Comparison::new(local_aggregate, remote, outcome.children);
        match policy.decide(comparison) {
            Decision::Skip => {
                // SKIP: the range is resolved. It leaves the active family by appearing in neither
                // output.
                outcome.skipped += 1;
            }
            Decision::Enumerate => {
                // IDLIST: hand the range to the caller to enumerate and ship. If the peer holds
                // anything here, it owes us its side too — and this crate's IDLIST is
                // one-directional, so the only way to ask for it without a wire change is to bounce
                // the parent back advertised as *empty*, which makes the peer take this very branch
                // on its side. Deriving that from the peer's advertised size rather than from the
                // policy's return is what keeps the unsound variant ("enumerate and resolve, while
                // the peer still holds elements we never asked for") unrepresentable.
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
                // SPLIT: replace the parent by a balanced family of child ranges, cut by *rank* via
                // `select` (Algorithm 2's `SPLITBYRANK`). Whatever stride the policy chose, the
                // children are consecutive, pairwise disjoint and cover the parent exactly, which
                // is what Proposition 4.1's correctness argument needs.
                outcome.split += 1;
                let stride = stride.get();
                let mut cur_bound = start_bound;
                let mut cur_index = start_index;
                loop {
                    let next_index = cur_index + stride;
                    if next_index >= end_index {
                        let range = KeyRange::new(cur_bound, end_bound);
                        // One bundled aggregate per emitted sub-range: its count is the same
                        // `end_index - cur_index` the index arithmetic would give (see the
                        // rank-difference note above), read from the same traversal as the
                        // fingerprint it ships with.
                        //
                        // Nothing was cut before this child exactly when `cur_index` never moved,
                        // and then the child *is* the parent — whose aggregate is already in hand,
                        // so the degenerate split a policy uses to bounce a range back costs no
                        // second traversal.
                        //
                        // Otherwise the clone is because `aggregate` takes its range by value (so a
                        // range built from runtime bounds is expressible at all) and the same range
                        // is then moved into the emitted `RangeAggregate`. It costs at most two key
                        // clones per emitted child, alongside the `next_key` clones the fan-out
                        // already performs.
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

    /// Build a real `FingerprintTreeMap` over the given (distinct, unsorted-ok) `i32` keys. A plain
    /// `i32` value stands in for whatever a caller actually stores: values are irrelevant to the
    /// mechanism — `protocol_round` only ever queries key positions, the range fingerprint and the
    /// size, and `RsosView` does not even name a value type.
    fn tree(keys: &[i32]) -> FingerprintTreeMap<i32, i32> {
        FingerprintTreeMap::from_iter(keys.iter().map(|&k| (k, 0)))
    }

    /// Run a single crafted segment through `protocol_round` and return whatever it produced. Generic
    /// over `RsosView` rather than taking the concrete tree: exercising the algorithm through the
    /// trait is what keeps it honest that nothing here needs a specific backend (or a value type).
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

    // An `Excluded` start bound or an `Included` end bound used to be reachable from the wire and
    // required a runtime check (dropped, not panicking). They no longer compile: `StartBound` has
    // no `Excluded` variant and `EndBound` has no `Included` variant, so
    // `RangeAggregate { range: (StartBound::Excluded(_), _), .. }` is not an expression this crate
    // can write, let alone a peer deserialize. The illegal state is unrepresentable, so there is
    // nothing left to test at this level — see `StartBound`/`EndBound`'s doc comments.

    /// An inverted range (`start_index > end_index`) used to underflow `end_index -
    /// start_index` (panic in debug, huge `usize` then out-of-bounds `select` in release). It
    /// must be dropped instead. Unlike the bound-shape cases above, this one *is* still
    /// representable on the wire (both bounds are individually legal shapes) and can only be
    /// detected against a concrete tree, so it stays a runtime check (`BoundedRange::parse`).
    #[test]
    fn inverted_range_is_dropped_not_panicking() {
        let store = tree(&[10, 20, 30]);
        let segment = RangeAggregate {
            // start_index = rank(100) = 3, end_index = rank(5) = 0
            range: KeyRange::new(StartBound::Included(100), EndBound::Excluded(5)),
            aggregate: Aggregate::new(1, Fingerprint([1, 0, 0, 0])),
        };
        let (child_ranges, enumeration_ranges) = round(&store, segment);
        assert!(child_ranges.is_empty());
        assert!(enumeration_ranges.is_empty());
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

    // ----- Emptiness and equality are decided on `size`, never on the -----
    // range fingerprint. A range fingerprint combines per-element lifts additively, so a
    // non-empty range can legitimately fingerprint to `ZERO` and two different ranges can
    // fingerprint equally. The segment fields below are exactly what such a colliding (or
    // hostile) peer puts on the wire; we drive them straight through `protocol_round`.

    /// Headline counterexample. A *non-empty* peer range that fingerprints to `ZERO`
    /// (e.g. two elements whose per-element lifts cancel) is advertised against our empty
    /// tree, which also fingerprints to `ZERO`. The fingerprints match (`ZERO == ZERO`) but
    /// the sizes differ (`2 != 0`). The buggy code short-circuited on the fingerprint
    /// comparison alone and concluded "in sync", silently losing the peer's two elements.
    /// With the size-based decision we must instead bounce the range back so the peer sends
    /// us its content.
    #[test]
    fn nonempty_zero_fingerprint_vs_empty_is_not_in_sync() {
        let store = tree(&[]); // empty: local fingerprint == ZERO, local size == 0
        let segment = RangeAggregate {
            range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
            // Fingerprint collides with our empty one ... but the peer is *not* empty.
            aggregate: Aggregate::new(2, Fingerprint::ZERO),
        };
        let (child_ranges, enumeration_ranges) = round(&store, segment);
        // Must not be swallowed as "in sync": we bounce an empty segment back so the peer
        // sends us the elements.
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

    /// The complementary direction: a genuinely identical range must still be
    /// concluded in sync (the size check does not produce false enumeration_ranges). We advertise
    /// the tree's own real fingerprint and size back to it.
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

    // ----- The fan-out rule itself -----
    // The number of children a SPLIT emits is what goes on the wire, so the fan-out rule *is* this
    // crate's communication cost. The tests below pin it so that changing it is a deliberate act
    // with a failing test attached, not a silent shift in every cluster's bandwidth profile.
    // `benches/protocol.rs` reports the same quantity in bytes, over realistic store sizes.
    //
    // One crafted segment covers all of them: the peer advertises the same element count as the
    // local store (so neither the empty-remote nor the single-element cutoff fires) with a
    // fingerprint that cannot match, which is exactly the "both sides non-empty, aggregates differ"
    // case that splits.
    fn splitting_segment(m: usize) -> RangeAggregate<i32> {
        RangeAggregate {
            range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
            aggregate: Aggregate::new(m, Fingerprint([7, 0, 0, 0])),
        }
    }

    /// The **default** fan-out is a constant: a SPLIT emits at most `b = 16` children whatever the
    /// range's size. Changing `DEFAULT_POLICY` fails here.
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

    /// And the rule the default replaced in 2026-08 (#257) is still available and still `Θ(√m)`:
    /// `SqrtFanOut` is shipped for anyone who wants the historical bandwidth/round profile back,
    /// so its behaviour is pinned too rather than left to rot.
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

    /// The children of a SPLIT partition the parent exactly: consecutive, non-overlapping, and
    /// spanning the whole parent range. This is what Proposition 4.1's correctness argument needs,
    /// and it holds independently of *how many* children the fan-out rule chooses — so it kept
    /// holding across the 2026-08 default change, and must keep holding through any future one.
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
                // The previous child's excluded end is the next child's included start: adjacent,
                // disjoint, no gap.
                (EndBound::Excluded(end), StartBound::Included(start)) => assert_eq!(end, start),
                other => panic!("children are not adjacent: {other:?}"),
            }
        }
    }

    // ----- Convergence, under every policy and under mixed pairs -----
    // The tests above pin the *default* rule. These pin what must hold for **any** rule: that a
    // full reconciliation driven through `protocol_round_with_policy` terminates and leaves the two
    // stores holding the same elements. `benches/protocol.rs` measures what each policy costs to get
    // there; here we only care that it gets there at all.

    /// Drive a full reconciliation between two stores until no active range is left, applying every
    /// IDLIST the way `reconcile`'s engine does — enumerate the responder's contents over the range
    /// and insert them into the peer.
    ///
    /// The two policies are supplied separately so a **mixed** pair can be driven: the refinement
    /// policy is a local decision and never crosses the wire, so two peers running different rules
    /// must still converge. Returns the one-way message count, purely as evidence of progress.
    fn drive(
        a: &mut FingerprintTreeMap<i32, i32>,
        b: &mut FingerprintTreeMap<i32, i32>,
        a_policy: &dyn RefinementPolicy,
        b_policy: &dyn RefinementPolicy,
    ) -> usize {
        let mut active = initial_ranges(&*a);
        // `initial_ranges` came from `a`, so `b` answers first and the responder alternates.
        let mut responder_is_b = true;
        let mut messages = 0;
        while !active.is_empty() {
            messages += 1;
            let mut children = Vec::new();
            let mut enumerations = Vec::new();
            // The responder answers under its own policy, then the ranges it asked to enumerate are
            // materialized before the borrow ends, so the peer can be mutated below.
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

    /// Build the two stores, reconcile them under the given pair of policies, and assert they end
    /// up holding exactly the union — same keys, same values, same whole-store aggregate.
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
        // The aggregate is what the protocol itself compares, so agreeing on it is the property a
        // *next* round would observe: two stores that agree here exchange nothing at all.
        assert_eq!(a.aggregate(..), b.aggregate(..));
    }

    /// The corpora exercise the three shapes that pull the policies apart: a difference scattered
    /// across the whole key space, a difference clustered into one contiguous block, and the
    /// degenerate ends (one side empty, both sides empty, one differing element).
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

    /// The seam's headline property: the policy is a **local** decision, so two peers running
    /// different ones still converge. If this ever fails, the policy has leaked into the wire
    /// contract and swapping one is no longer free.
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

    /// The classification tally must add up to the ranges that were answered, and must attribute
    /// the malformed one rather than silently swallowing it — the count is the only signal a
    /// consumer without a `tracing` subscriber ever gets.
    #[test]
    fn round_outcome_accounts_for_every_segment() {
        let store = tree(&[10, 20, 30, 40, 50]);
        let mut child_ranges = Vec::new();
        let mut enumeration_ranges = Vec::new();
        let outcome = protocol_round(
            &store,
            vec![
                // SKIP: our own aggregate, advertised back at us.
                RangeAggregate {
                    range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                    aggregate: store.aggregate(..),
                },
                // IDLIST: the peer holds nothing over the whole range.
                RangeAggregate {
                    range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                    aggregate: Aggregate::ZERO,
                },
                // SPLIT: same size, different fingerprint.
                RangeAggregate {
                    range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                    aggregate: Aggregate::new(5, Fingerprint([7, 0, 0, 0])),
                },
                // Dropped: inverted once resolved against the local set.
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
        // The IDLIST was against an empty peer range, so it bounced nothing back: every child came
        // from the SPLIT.
        assert_eq!(outcome.children(), child_ranges.len());
        assert_eq!(enumeration_ranges.len(), outcome.enumerated());
    }

    /// And the adversarial middle case: matching fingerprints with mismatched sizes must not
    /// be mistaken for "in sync"; the range is refined instead. We feed the tree's own
    /// fingerprint with a deliberately wrong (larger) size, forcing the fan-out branch.
    #[test]
    fn matching_fingerprint_but_wrong_size_is_refined() {
        let store = tree(&[10, 20, 30, 40, 50]);
        let segment = RangeAggregate {
            range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
            // Fingerprints collide ... but the advertised size is wrong.
            aggregate: Aggregate::new(store.len() + 7, store.aggregate(..).fingerprint()),
        };
        let (child_ranges, enumeration_ranges) = round(&store, segment);
        // Not concluded in sync: the range is subdivided and bounced back for refinement.
        assert!(!child_ranges.is_empty());
        assert!(enumeration_ranges.is_empty());
    }
}
