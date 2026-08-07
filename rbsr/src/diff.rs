// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The Range-Based Set Reconciliation walk itself: [`start_diff`], [`diff_round`], and the
//! [`RangeAggregate`] wire type they exchange.
//!
//! Both functions are free functions generic over [`RsosView`], the four read-only RSOS operations
//! the walk needs (`size`/`aggregate`/`rank`/`select`) — never over a concrete data structure. See
//! the crate root documentation for the algorithm and its citations.

use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use rsos::Aggregate;

use crate::rsos_view::RsosView;

/// The start bound of a [`RangeAggregate`] range, as this protocol actually emits it: `Included` or
/// `Unbounded`, never `Excluded`.
///
/// `RangeAggregate` is deserialized straight off the wire (see the module docs on `diff_round`'s
/// former validation). Narrowing this from `std::ops::Bound<K>` (three variants) to the two
/// shapes the protocol produces makes the third shape (`Excluded`) **unrepresentable**: a peer
/// sending it fails to deserialize — the same "malformed, drop the datagram" path
/// as any other corrupt input — instead of reaching a runtime check inside `diff_round`.
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
/// advertises about one range of its store, and the unit the RBSR refinement walk exchanges.
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
/// which is why [`Aggregate`] declares `fingerprint` before `size` (see its own note) and why
/// `wire_format_is_unchanged_by_the_aggregate_collapse` below pins the exact bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RangeAggregate<K> {
    range: KeyRange<K>,
    aggregate: Aggregate,
}

pub type DiffRange<K> = (Bound<K>, Bound<K>);

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

/// Returns a representation of all the elements in the local set that can be sent to
/// [`diff_round`]: the root segment `{(−∞, +∞), A(whole store)}` that bootstraps a
/// reconciliation.
///
/// The fingerprint and the size come from a single [`RsosView::aggregate`] over `..` — Def. 3.5's
/// bundled `A(S) = (|S|, Σ(S))`, one traversal for both halves, so they cannot describe different
/// sets.
pub fn start_diff<K, B: RsosView<K>>(local: &B) -> Vec<RangeAggregate<K>> {
    vec![RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        aggregate: local.aggregate(&..),
    }]
}

/// Refines set differences (a range of keys along with its [`Aggregate`]) into smaller sets.
///
/// When sets are determined to contain the same elements, they are removed from the output.
/// When sets are determined to only contain differing elements, the corresponding elements are
/// listed as `differences`. In other cases, the set must be refined and sent back to the peer
/// for further analysis.
pub fn diff_round<K, B: RsosView<K>>(
    local: &B,
    in_comparison: Vec<RangeAggregate<K>>,
    out_comparison: &mut Vec<RangeAggregate<K>>,
    differences: &mut Vec<DiffRange<K>>,
) where
    K: Clone,
{
    for segment in in_comparison {
        let RangeAggregate {
            range: KeyRange(start, end),
            aggregate: remote,
        } = segment;
        // One bundled `Aggregate` (Def. 3.5) for both halves of what this range looks like
        // locally: its element count *and* its fingerprint, in a single traversal. Safe on any
        // bound combination — including a not-yet-validated inverted range — because `aggregate`
        // walks via point/range comparisons, never index arithmetic (an inverted range simply
        // aggregates to the empty set).
        let local_aggregate = local.aggregate(&KeyRange::new(start.clone(), end.clone()));
        // The bound *shapes* are already guaranteed by `StartBound`/`EndBound`: a peer sending
        // anything else fails to deserialize before `diff_round` ever runs (see their doc
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
            continue;
        } else if remote_size == 0 {
            differences.push((start_bound.into(), end_bound.into()));
            continue;
        } else if local_size == 0 {
            // present on remote; bounce back to the remote
            out_comparison.push(RangeAggregate {
                range: KeyRange::new(start_bound, end_bound),
                aggregate: Aggregate::ZERO,
            });
            continue;
        } else if remote_size == 1 && local_size == 1 {
            // ask the remote to send us the conflicting item
            out_comparison.push(RangeAggregate {
                range: KeyRange::new(start_bound.clone(), end_bound.clone()),
                aggregate: Aggregate::ZERO,
            });
            // send the conflicting item to the remote
            differences.push((start_bound.into(), end_bound.into()));
        } else if local_size == 1 {
            // not enough information; bounce back to the remote
            out_comparison.push(RangeAggregate {
                range: KeyRange::new(start_bound, end_bound),
                aggregate: local_aggregate,
            });
        } else {
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
                    let aggregate = local.aggregate(&range);
                    out_comparison.push(RangeAggregate { range, aggregate });
                    break;
                } else {
                    let next_key = local.select(next_index).clone();
                    let range = KeyRange::new(cur_bound, EndBound::Excluded(next_key.clone()));
                    let aggregate = local.aggregate(&range);
                    out_comparison.push(RangeAggregate { range, aggregate });
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
    /// mechanism — `diff_round` only ever queries key positions, the range fingerprint and the
    /// size, and `RsosView` does not even name a value type.
    fn tree(keys: &[i32]) -> FingerprintTreeMap<i32, i32> {
        FingerprintTreeMap::from_iter(keys.iter().map(|&k| (k, 0)))
    }

    /// Run a single crafted segment through `diff_round` and return whatever it produced. Generic
    /// over `RsosView` rather than taking the concrete tree: exercising the algorithm through the
    /// trait is what keeps it honest that nothing here needs a specific backend (or a value type).
    fn round<B: RsosView<i32>>(
        store: &B,
        segment: RangeAggregate<i32>,
    ) -> (Vec<RangeAggregate<i32>>, Vec<DiffRange<i32>>) {
        let mut out_comparison = Vec::new();
        let mut differences = Vec::new();
        diff_round(store, vec![segment], &mut out_comparison, &mut differences);
        (out_comparison, differences)
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
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
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
        let (_out_comparison, differences) = round(&store, segment);
        assert_eq!(differences, vec![(Bound::Unbounded, Bound::Unbounded)]);
    }

    // ----- Emptiness and equality are decided on `size`, never on the -----
    // range fingerprint. A range fingerprint combines per-element lifts additively, so a
    // non-empty range can legitimately fingerprint to `ZERO` and two different ranges can
    // fingerprint equally. The segment fields below are exactly what such a colliding (or
    // hostile) peer puts on the wire; we drive them straight through `diff_round`.

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
        let (out_comparison, differences) = round(&store, segment);
        // Must not be swallowed as "in sync": we bounce an empty segment back so the peer
        // sends us the elements.
        assert!(differences.is_empty());
        assert_eq!(out_comparison.len(), 1);
        assert_eq!(
            out_comparison[0],
            RangeAggregate {
                range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
                aggregate: Aggregate::ZERO,
            }
        );
    }

    /// The complementary direction: a genuinely identical range must still be
    /// concluded in sync (the size check does not produce false differences). We advertise
    /// the tree's own real fingerprint and size back to it.
    #[test]
    fn matching_fingerprint_and_size_is_in_sync() {
        let store = tree(&[10, 20, 30]);
        let segment = RangeAggregate {
            range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
            aggregate: store.aggregate(&..),
        };
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
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
            aggregate: Aggregate::new(store.len() + 7, store.aggregate(&..).fingerprint()),
        };
        let (out_comparison, differences) = round(&store, segment);
        // Not concluded in sync: the range is subdivided and bounced back for refinement.
        assert!(!out_comparison.is_empty());
        assert!(differences.is_empty());
    }

    /// Golden vector: `RangeAggregate`'s encoding must be **byte-for-byte** what the pre-
    /// `Aggregate` layout `{range, hash: Fingerprint, size: usize}` produced, so a node running
    /// this code and a node running the previous release reconcile without either noticing.
    ///
    /// The bytes below were captured from that earlier layout, with the `bincode`
    /// `DefaultOptions` configuration `reconcile`'s wire codec (`reconcile::bincode`) uses, before
    /// the two fields were collapsed into one nested `Aggregate`. bincode writes struct fields
    /// sequentially with no framing or field names, so the nested struct is inlined and the
    /// encoding is unchanged — *provided* `Aggregate` declares `fingerprint` before `size`.
    /// Reordering those two declarations breaks this test, which is the point (see
    /// `rsos::Aggregate`'s own note).
    ///
    /// `bincode` is a **dev**-dependency of this crate and appears nowhere outside this test:
    /// `rbsr` itself owns no encoding (the crate root says so), it only has to keep the type it
    /// hands to `reconcile`'s codec byte-stable. Pinning that here rather than in `reconcile` is
    /// what keeps the check next to the declaration order it is guarding, and `KeyRange`/
    /// `StartBound`/`EndBound` are `pub(crate)` so the vector cannot be built from outside.
    ///
    /// Reading the vector: `1` = `StartBound::Included` discriminant, `7` = the start key; `1` =
    /// `EndBound::Excluded` discriminant, `42` = the end key; then the four `u64` fingerprint
    /// limbs as bincode varints; then `251, 44, 1` = the varint for `size == 300`.
    #[test]
    fn wire_format_is_unchanged_by_the_aggregate_collapse() {
        use ::bincode::{DefaultOptions, Deserializer, Serializer};

        const GOLDEN: &[u8] = &[
            1, 7, 1, 42, 253, 239, 205, 171, 137, 103, 69, 35, 1, 253, 16, 50, 84, 118, 152, 186,
            220, 254, 1, 2, 251, 44, 1,
        ];

        let segment = RangeAggregate {
            range: KeyRange::new(StartBound::Included(7u32), EndBound::Excluded(42u32)),
            aggregate: Aggregate::new(
                300,
                Fingerprint([0x0123456789abcdef, 0xfedcba9876543210, 1, 2]),
            ),
        };

        let mut buf = Vec::new();
        segment
            .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
            .unwrap();
        assert_eq!(
            buf, GOLDEN,
            "RangeAggregate's wire encoding changed — this is a protocol break, not a refactor"
        );

        let mut deserializer = Deserializer::from_slice(GOLDEN, DefaultOptions::new());
        let decoded = RangeAggregate::<u32>::deserialize(&mut deserializer).unwrap();
        assert_eq!(decoded, segment);
    }
}
