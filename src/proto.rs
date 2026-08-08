// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Anti-entropy protocol mechanism (Range-Based Set Reconciliation).
//!
//! [`start_diff`] and [`diff_round`] are free functions over the concrete [`FingerprintTreeMap`]; they are
//! the *how* of reconciliation, an implementation detail of the domain, not part of the crate's
//! public surface (see `ARCHITECTURE.md` §3.7). The whole module is `pub(crate)`, so although the
//! items below are declared `pub`, they are unreachable through the public path — the gated
//! [`crate::testing`] seam re-exports exactly the few the integration oracles need. The range-aggregate
//! queries the algorithm relies on ([`FingerprintTreeMap::aggregate`], [`FingerprintTreeMap::rank`],
//! [`FingerprintTreeMap::select`], [`FingerprintTreeMap::len`]) are inherent methods on `FingerprintTreeMap`.

use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use rsos::{Aggregate, FingerprintTreeMap};

/// The start bound of a [`RangeAggregate`] range, as this protocol actually emits it: `Included` or
/// `Unbounded`, never `Excluded`.
///
/// `RangeAggregate` is deserialized straight off the wire (see the module docs on `diff_round`'s
/// former validation). Narrowing this from `std::ops::Bound<K>` (three variants) to the two
/// shapes the protocol produces makes the third shape (`Excluded`) **unrepresentable**: a peer
/// sending it fails to deserialize — the same "malformed, drop the datagram" path
/// `handle_messages` already takes for any other corrupt message — rather than reaching
/// `diff_round` and requiring a runtime check.
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
/// This lets a segment's range feed straight into [`FingerprintTreeMap::aggregate`] / [`FingerprintTreeMap::range`] like
/// any other `RangeBounds<K>`, with no intermediate conversion.
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
/// [`Aggregate`] now, the same value [`FingerprintTreeMap::aggregate`] already returns, so a
/// segment can no longer be built with a count and a fingerprint that describe different sets.
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

/// A [`RangeAggregate`]'s range, checked against a concrete [`FingerprintTreeMap`]: the bound *shapes* are
/// already guaranteed by [`StartBound`]/[`EndBound`] (unrepresentable otherwise), so the only
/// remaining way a wire segment can be malformed is an inverted range (`start_index >
/// end_index`) — which can only be detected against a specific tree, hence this being a fallible
/// constructor rather than a static property of the wire type.
struct BoundedRange<K> {
    start: StartBound<K>,
    end: EndBound<K>,
    start_index: usize,
    end_index: usize,
}

/// The one way [`BoundedRange::parse`] can fail: the segment's start position is after its end
/// position in the tree it was checked against.
struct InvertedRange;

impl<K: std::hash::Hash + Ord> BoundedRange<K> {
    fn parse<V: std::hash::Hash>(
        start: StartBound<K>,
        end: EndBound<K>,
        tree: &FingerprintTreeMap<K, V>,
    ) -> Result<Self, InvertedRange> {
        let start_index = match &start {
            StartBound::Unbounded => 0,
            StartBound::Included(key) => tree.rank(key),
        };
        let end_index = match &end {
            EndBound::Unbounded => tree.len(),
            EndBound::Excluded(key) => tree.rank(key),
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

    /// Number of elements between the two bounds. Infallible: the constructor already guarantees
    /// `end_index >= start_index`.
    fn len(&self) -> usize {
        self.end_index - self.start_index
    }
}

/// Returns a representation of all the elements in the tree that can be sent to [`diff_round`]:
/// the root segment `{(−∞, +∞), A(whole store)}` that bootstraps a reconciliation.
pub fn start_diff<K, V>(tree: &FingerprintTreeMap<K, V>) -> Vec<RangeAggregate<K>>
where
    K: std::hash::Hash + Ord,
    V: std::hash::Hash,
{
    vec![RangeAggregate {
        range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
        // One tree walk gives both halves; they cannot describe different sets.
        aggregate: tree.aggregate(&..),
    }]
}

/// Refines set differences (a range of keys along with its [`Aggregate`]) into smaller sets.
///
/// When sets are determined to contain the same elements, they are removed from the output.
/// When sets are determined to only contain differing elements, the corresponding elements are
/// listed as `differences`. In other cases, the set must be refined and sent back to the peer
/// for further analysis.
pub fn diff_round<K, V>(
    tree: &FingerprintTreeMap<K, V>,
    in_comparison: Vec<RangeAggregate<K>>,
    out_comparison: &mut Vec<RangeAggregate<K>>,
    differences: &mut Vec<DiffRange<K>>,
) where
    K: Clone + std::hash::Hash + Ord,
    V: std::hash::Hash,
{
    for segment in in_comparison {
        let RangeAggregate {
            range: KeyRange(start, end),
            aggregate: remote,
        } = segment;
        // Safe on any bound combination — including a not-yet-validated inverted range —
        // because `FingerprintTreeMap::aggregate` walks the tree via point/range comparisons, never index
        // arithmetic (see its doc comment).
        let local_fingerprint = tree
            .aggregate(&KeyRange::new(start.clone(), end.clone()))
            .fingerprint();
        // The bound *shapes* are already guaranteed by `StartBound`/`EndBound`: a peer sending
        // anything else fails to deserialize before `diff_round` ever runs (see their doc
        // comments). The one remaining way a wire segment can be malformed is an inverted range
        // (`start_index > end_index`, e.g. `Included(100)..Excluded(5)`) — undetectable without a
        // concrete tree, hence the fallible constructor rather than a static property of the wire
        // type. Dropping it here avoids the underflow/out-of-bounds `select` that trusting the
        // arithmetic would cause.
        let bounded = match BoundedRange::parse(start, end, tree) {
            Ok(bounded) => bounded,
            Err(InvertedRange) => {
                debug!("dropping segment with inverted range");
                continue;
            }
        };
        let local_size = bounded.len();
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
        // divergence. This is also why the comparison below is spelled out over both halves
        // rather than as `remote == local_aggregate`: `local_size` comes from the tree's index
        // arithmetic (`BoundedRange`), which is what the branches downstream also use.
        let remote_size = remote.size();
        if remote.fingerprint() == local_fingerprint && remote_size == local_size {
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
                aggregate: Aggregate::new(local_size, local_fingerprint),
            });
        } else {
            // NOTE: end_index - start_index ≥ 2
            let step = 1.max((end_index - start_index) / 16);
            let mut cur_bound = start_bound;
            let mut cur_index = start_index;
            loop {
                let next_index = cur_index + step;
                if next_index >= end_index {
                    let range = KeyRange::new(cur_bound, end_bound);
                    let aggregate =
                        Aggregate::new(end_index - cur_index, tree.aggregate(&range).fingerprint());
                    out_comparison.push(RangeAggregate { range, aggregate });
                    break;
                } else {
                    let next_key = tree.select(next_index);
                    let range = KeyRange::new(cur_bound, EndBound::Excluded(next_key.clone()));
                    let aggregate = Aggregate::new(
                        next_index - cur_index,
                        tree.aggregate(&range).fingerprint(),
                    );
                    out_comparison.push(RangeAggregate { range, aggregate });
                    cur_bound = StartBound::Included(next_key.clone());
                    cur_index = next_index;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rsos::Fingerprint;

    use super::*;

    /// Build a real `FingerprintTreeMap` over the given (distinct, unsorted-ok) `i32` keys. The values are
    /// irrelevant to the protocol mechanism — `diff_round` only ever queries key positions,
    /// the range fingerprint and the size — so we store a constant.
    fn tree(keys: &[i32]) -> FingerprintTreeMap<i32, i32> {
        FingerprintTreeMap::from_iter(keys.iter().map(|&k| (k, 0)))
    }

    /// Run a single crafted segment through `diff_round` and return whatever it produced.
    fn round(
        store: &FingerprintTreeMap<i32, i32>,
        segment: RangeAggregate<i32>,
    ) -> (Vec<RangeAggregate<i32>>, Vec<DiffRange<i32>>) {
        let mut out_comparison = Vec::new();
        let mut differences = Vec::new();
        diff_round(store, vec![segment], &mut out_comparison, &mut differences);
        (out_comparison, differences)
    }

    // ----- Malformed segments from the wire must be dropped, never panic. -----
    //
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

    /// A well-formed `(Unbounded, Unbounded)` segment from an empty peer over our whole,
    /// non-empty range must still be recognised as a difference we owe — the validation
    /// guards do not swallow the legitimate shape the protocol actually uses.
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
        let store = tree(&[]); // empty: local_fingerprint == ZERO, local_size == 0
        let segment = RangeAggregate {
            range: KeyRange::new(StartBound::Unbounded, EndBound::Unbounded),
            // Fingerprint collides with our empty one ... but the peer is *not* empty.
            aggregate: Aggregate::new(2, Fingerprint::ZERO),
        };
        let (out_comparison, differences) = round(&store, segment);
        // Must not be swallowed as "in sync": we bounce an empty segment back so the peer
        // sends us the divergent items it holds.
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

    /// Dual: equal fingerprints with equal sizes over a *non-empty* range are correctly
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

    /// Branch: equal fingerprints but *different* sizes over a non-empty range must not
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
    /// The bytes below were captured from that earlier layout, with this exact
    /// `bincode::DefaultOptions` configuration (`crate::bincode::encode`), before the two fields
    /// were collapsed into one nested `Aggregate`. bincode writes struct fields sequentially with
    /// no framing or field names, so the nested struct is inlined and the encoding is unchanged —
    /// *provided* `Aggregate` declares `fingerprint` before `size`. Reordering those two
    /// declarations breaks this test, which is the point (see `rsos::Aggregate`'s own note).
    ///
    /// Reading the vector: `1` = `StartBound::Included` discriminant, `7` = the start key; `1` =
    /// `EndBound::Excluded` discriminant, `42` = the end key; then the four `u64` fingerprint
    /// limbs as bincode varints; then `251, 44, 1` = the varint for `size == 300`.
    #[test]
    fn wire_format_is_unchanged_by_the_aggregate_collapse() {
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
        crate::bincode::encode(&segment, &mut buf).unwrap();
        assert_eq!(
            buf, GOLDEN,
            "RangeAggregate's wire encoding changed — this is a protocol break, not a refactor"
        );

        let decoded: Vec<RangeAggregate<u32>> = crate::bincode::decode_stream(GOLDEN, 4).unwrap();
        assert_eq!(decoded, vec![segment]);
    }
}
