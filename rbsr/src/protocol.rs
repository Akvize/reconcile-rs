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
//! and the citations; and [`protocol_round`]'s own documentation for the three protocol outcomes
//! and this crate's two deviations from Algorithm 1.
//!
//! This module is private — everything public here is re-exported from the crate root, so any
//! documentation a *user* needs belongs on an item, not in this header, which rustdoc never
//! renders.

use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use rsos::Aggregate;

use crate::rsos_view::RsosView;

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

/// One **protocol round**: apply the responder step to every active range this peer was asked to
/// answer, classifying each as SKIP, IDLIST or SPLIT.
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
/// `enumeration_ranges` (IDLIST), in **both** (the one-element-each case, where the two peers owe
/// each other exactly one element), or in neither (SKIP). Malformed input joins the last of those:
/// a range whose bounds invert once resolved against the local set is dropped rather than
/// answered.
///
/// # Deviations from Algorithm 1
///
/// This crate *instantiates* the protocol; it is not a transcription of Algorithm 1. Two decision
/// rules differ, and neither is a bug — but a reader holding the paper open should know which
/// lines will not match:
///
/// 1. **No enumeration threshold `t`.** The paper takes IDLIST whenever `|X ∩ [l, u)| ≤ t` for a
///    fixed parameter `t` (its experiments use `t = 32`). There is no `t` here. Enumeration is
///    triggered instead by hand-picked special cases: the peer's range is empty, or both sides
///    hold exactly one element (which enumerates *and* bounces the range back, so the exchange is
///    symmetric). A lone local element facing a larger remote range is bounced back with the local
///    aggregate rather than enumerated. The termination argument is the paper's — every
///    mismatching range is either resolved or strictly refined — reached through different
///    cutoffs.
/// 2. **The SPLIT fan-out is not a fixed `b`.** The paper's `SPLITBYRANK(O_X, l, u, b)` produces a
///    `b`-balanced partition for a fixed branching factor `b` (its experiments use Negentropy's
///    `b = 16`). Here the cut step is `⌊√(count)⌋`, so the fan-out grows as `√n` with the range
///    size instead of staying constant. The partition is still balanced by *rank* — cuts are
///    materialized with [`RsosView::select`] exactly as in Algorithm 2 — and the child ranges are
///    still pairwise disjoint with union the parent range, which is all Proposition 4.1's
///    correctness argument uses. What does **not** carry over verbatim is the paper's local-cost
///    bound `T_loc = O(hL + bhI + K)`: its `bhI` term assumes a constant `b`. No complexity claim
///    in this workspace's documentation quotes that bound, so none needed adjusting here — but a
///    future one must not quote it either.
pub fn protocol_round<K, B: RsosView<K>>(
    local: &B,
    active_ranges: Vec<RangeAggregate<K>>,
    child_ranges: &mut Vec<RangeAggregate<K>>,
    enumeration_ranges: &mut Vec<EnumerationRange<K>>,
) where
    K: Clone,
{
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
                continue;
            }
        };
        // `local_size` is the same quantity as `end_index - start_index`: `rank` counts the keys
        // strictly below a bound, so the difference of the two ranks *is* the number of keys in
        // `[start, end)` — which is what `aggregate` counted. It comes from the bundled aggregate
        // rather than from that subtraction so the count and the fingerprint it is compared
        // alongside are read from one and the same traversal.
        let local_size = local_aggregate.size();
        let start_index = bounded.start_index;
        let end_index = bounded.end_index;
        let BoundedRange {
            start: start_bound,
            end: end_bound,
            ..
        } = bounded;
        // NOTE: decisions about emptiness and equality are made on the exact element counts,
        // never on the fingerprints alone. A range fingerprint combines per-element lifts by
        // addition modulo 2²⁵⁶ (see `rsos::fingerprint`), so a *non-empty* range can legitimately
        // fingerprint to `ZERO`; using `Σ(S) == ZERO` as an "empty" sentinel (or matching
        // fingerprints alone as "equal") would alias such ranges and cause silent, permanent
        // divergence.
        let remote_size = remote.size();
        if remote.fingerprint() == local_aggregate.fingerprint() && remote_size == local_size {
            // SKIP: the aggregates agree, so the range is resolved. It leaves the active family by
            // appearing in neither output.
            continue;
        } else if remote_size == 0 {
            // IDLIST: the peer holds nothing here, so everything we hold is the local symmetric
            // difference. Hand the range to the caller to enumerate and ship.
            enumeration_ranges.push((start_bound.into(), end_bound.into()));
            continue;
        } else if local_size == 0 {
            // present on remote; bounce back to the remote — a degenerate SPLIT whose single child
            // *is* the parent range, advertised as empty so the peer takes the IDLIST branch above
            // on its side and enumerates it to us.
            child_ranges.push(RangeAggregate {
                range: KeyRange::new(start_bound, end_bound),
                aggregate: Aggregate::ZERO,
            });
            continue;
        } else if remote_size == 1 && local_size == 1 {
            // Both outcomes at once, because both sides owe each other exactly one element:
            // IDLIST our element to the peer, and bounce the range back advertised as empty so the
            // peer IDLISTs its own element to us.
            child_ranges.push(RangeAggregate {
                range: KeyRange::new(start_bound.clone(), end_bound.clone()),
                aggregate: Aggregate::ZERO,
            });
            enumeration_ranges.push((start_bound.into(), end_bound.into()));
        } else if local_size == 1 {
            // Not enough information to cut: a single local element cannot be split by rank, so
            // re-advertise the range unchanged with our real aggregate and let the peer — which
            // holds more here — do the splitting. Another degenerate SPLIT: one child, equal to
            // the parent.
            child_ranges.push(RangeAggregate {
                range: KeyRange::new(start_bound, end_bound),
                aggregate: local_aggregate,
            });
        } else {
            // SPLIT: replace the parent by a balanced family of child ranges, cut by *rank* via
            // `select` (Algorithm 2's `SPLITBYRANK`). The step is `√(count)` rather than the
            // paper's fixed branching factor `b` — see the module docs' deviation note; the
            // children are still pairwise disjoint with union the parent range, which is what the
            // correctness argument needs.
            // NOTE: end_index - start_index ≥ 2
            let step = ((end_index - start_index) as f32).sqrt() as usize;
            let mut cur_bound = start_bound;
            let mut cur_index = start_index;
            loop {
                let next_index = cur_index + step;
                if next_index >= end_index {
                    let range = KeyRange::new(cur_bound, end_bound);
                    // One bundled aggregate per emitted sub-range: its count is the same
                    // `end_index - cur_index` the index arithmetic would give (see the
                    // rank-difference note above), read from the same traversal as the
                    // fingerprint it ships with.
                    //
                    // The clone is because `aggregate` takes its range by value (so a range built
                    // from runtime bounds is expressible at all) and the same range is then moved
                    // into the emitted `RangeAggregate`. It costs at most two key clones per
                    // emitted child, alongside the `next_key` clones the fan-out already performs.
                    let aggregate = local.aggregate(range.clone());
                    child_ranges.push(RangeAggregate { range, aggregate });
                    break;
                } else {
                    let next_key = local.select(next_index).clone();
                    let range = KeyRange::new(cur_bound, EndBound::Excluded(next_key.clone()));
                    let aggregate = local.aggregate(range.clone());
                    child_ranges.push(RangeAggregate { range, aggregate });
                    cur_bound = StartBound::Included(next_key);
                    cur_index = next_index;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rsos::{Fingerprint, FingerprintTreeMap};

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
