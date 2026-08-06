// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Anti-entropy protocol mechanism (Range-Based Set Reconciliation).
//!
//! [`start_diff`] and [`diff_round`] are free functions over the concrete [`HRTree`]; they are
//! the *how* of reconciliation, an implementation detail of the domain, not part of the crate's
//! public surface (see `ARCHITECTURE.md` §3.7). The whole module is `pub(crate)`, so although the
//! items below are declared `pub`, they are unreachable through the public path — the gated
//! [`crate::testing`] seam re-exports exactly the few the integration oracles need. The range-hash
//! queries the algorithm relies on ([`HRTree::hash`], [`HRTree::insertion_position`],
//! [`HRTree::key_at`], [`HRTree::len`]) are inherent methods on `HRTree`.

use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::fingerprint::Fingerprint;
use crate::hrtree::HRTree;

/// The start bound of a [`HashSegment`] range, as this protocol actually emits it: `Included` or
/// `Unbounded`, never `Excluded`.
///
/// `HashSegment` is deserialized straight off the wire (see the module docs on `diff_round`'s
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

/// The end bound of a [`HashSegment`] range: `Excluded` or `Unbounded`, never `Included`. See
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

/// A [`HashSegment`]'s range: `(StartBound<K>, EndBound<K>)`, wrapped in a local tuple struct
/// (rather than a bare tuple) so it can implement the foreign [`RangeBounds`] trait directly —
/// Rust's orphan rules require the outermost type to be local, and a plain tuple never qualifies.
/// This lets a segment's range feed straight into [`HRTree::hash`] / [`HRTree::get_range`] like
/// any other `RangeBounds<K>`, with no intermediate conversion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SegmentRange<K>(StartBound<K>, EndBound<K>);

impl<K> SegmentRange<K> {
    fn new(start: StartBound<K>, end: EndBound<K>) -> Self {
        SegmentRange(start, end)
    }
}

impl<K> RangeBounds<K> for SegmentRange<K> {
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

/// Represents the elements of the collection in the given key range. The `hash` and `size`
/// fields allow testing whether two segments represent the same elements.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HashSegment<K> {
    range: SegmentRange<K>,
    hash: Fingerprint,
    size: usize,
}

pub type DiffRange<K> = (Bound<K>, Bound<K>);

/// A [`HashSegment`]'s range, checked against a concrete [`HRTree`]: the bound *shapes* are
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
        tree: &HRTree<K, V>,
    ) -> Result<Self, InvertedRange> {
        let start_index = match &start {
            StartBound::Unbounded => 0,
            StartBound::Included(key) => tree.insertion_position(key),
        };
        let end_index = match &end {
            EndBound::Unbounded => tree.len(),
            EndBound::Excluded(key) => tree.insertion_position(key),
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
/// the root segment `{(−∞, +∞), global hash, size}` that bootstraps a reconciliation.
pub fn start_diff<K, V>(tree: &HRTree<K, V>) -> Vec<HashSegment<K>>
where
    K: std::hash::Hash + Ord,
    V: std::hash::Hash,
{
    vec![HashSegment {
        range: SegmentRange::new(StartBound::Unbounded, EndBound::Unbounded),
        hash: tree.hash(&..),
        size: tree.len(),
    }]
}

/// Refines set differences (a range of keys along with the accumulated hash) into smaller sets.
///
/// When sets are determined to contain the same elements, they are removed from the output.
/// When sets are determined to only contain differing elements, the corresponding elements are
/// listed as `differences`. In other cases, the set must be refined and sent back to the peer
/// for further analysis.
pub fn diff_round<K, V>(
    tree: &HRTree<K, V>,
    in_comparison: Vec<HashSegment<K>>,
    out_comparison: &mut Vec<HashSegment<K>>,
    differences: &mut Vec<DiffRange<K>>,
) where
    K: Clone + std::hash::Hash + Ord,
    V: std::hash::Hash,
{
    for segment in in_comparison {
        let HashSegment {
            range: SegmentRange(start, end),
            hash,
            size,
        } = segment;
        // Safe on any bound combination — including a not-yet-validated inverted range —
        // because `HRTree::hash` walks the tree via point/range comparisons, never index
        // arithmetic (see its doc comment).
        let local_hash = tree.hash(&SegmentRange::new(start.clone(), end.clone()));
        // The bound *shapes* are already guaranteed by `StartBound`/`EndBound`: a peer sending
        // anything else fails to deserialize before `diff_round` ever runs (see their doc
        // comments). The one remaining way a wire segment can be malformed is an inverted range
        // (`start_index > end_index`, e.g. `Included(100)..Excluded(5)`) — undetectable without a
        // concrete tree, hence the fallible constructor rather than a static property of the wire
        // type. Dropping it here avoids the underflow/out-of-bounds `key_at` that trusting the
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
        // NOTE: decisions about emptiness and equality are made on the exact
        // `size`/`local_size`, never on `hash`/`local_hash`. A range fingerprint
        // combines per-element hashes by addition modulo 2²⁵⁶ (see
        // `crate::fingerprint`), so a *non-empty* range can legitimately fingerprint
        // to `ZERO`; using `hash == ZERO` as an "empty" sentinel (or `hash ==
        // local_hash` alone as "equal") would alias such ranges and cause silent,
        // permanent divergence.
        if hash == local_hash && size == local_size {
            continue;
        } else if size == 0 {
            differences.push((start_bound.into(), end_bound.into()));
            continue;
        } else if local_size == 0 {
            // present on remote; bounce back to the remote
            out_comparison.push(HashSegment {
                range: SegmentRange::new(start_bound, end_bound),
                hash: Fingerprint::ZERO,
                size: 0,
            });
            continue;
        } else if size == 1 && local_size == 1 {
            // ask the remote to send us the conflicting item
            out_comparison.push(HashSegment {
                range: SegmentRange::new(start_bound.clone(), end_bound.clone()),
                hash: Fingerprint::ZERO,
                size: 0,
            });
            // send the conflicting item to the remote
            differences.push((start_bound.into(), end_bound.into()));
        } else if local_size == 1 {
            // not enough information; bounce back to the remote
            out_comparison.push(HashSegment {
                range: SegmentRange::new(start_bound, end_bound),
                hash: local_hash,
                size: local_size,
            });
        } else {
            // NOTE: end_index - start_index ≥ 2
            let step = 1.max((end_index - start_index) / 16);
            let mut cur_bound = start_bound;
            let mut cur_index = start_index;
            loop {
                let next_index = cur_index + step;
                if next_index >= end_index {
                    let range = SegmentRange::new(cur_bound, end_bound);
                    out_comparison.push(HashSegment {
                        hash: tree.hash(&range),
                        range,
                        size: end_index - cur_index,
                    });
                    break;
                } else {
                    let next_key = tree.key_at(next_index);
                    let range = SegmentRange::new(cur_bound, EndBound::Excluded(next_key.clone()));
                    out_comparison.push(HashSegment {
                        hash: tree.hash(&range),
                        range,
                        size: next_index - cur_index,
                    });
                    cur_bound = StartBound::Included(next_key.clone());
                    cur_index = next_index;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a real `HRTree` over the given (distinct, unsorted-ok) `i32` keys. The values are
    /// irrelevant to the protocol mechanism — `diff_round` only ever queries key positions,
    /// the range fingerprint and the size — so we store a constant.
    fn tree(keys: &[i32]) -> HRTree<i32, i32> {
        HRTree::from_iter(keys.iter().map(|&k| (k, 0)))
    }

    /// Run a single crafted segment through `diff_round` and return whatever it produced.
    fn round(
        store: &HRTree<i32, i32>,
        segment: HashSegment<i32>,
    ) -> (Vec<HashSegment<i32>>, Vec<DiffRange<i32>>) {
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
    // `HashSegment { range: (StartBound::Excluded(_), _), .. }` is not an expression this crate
    // can write, let alone a peer deserialize. The illegal state is unrepresentable, so there is
    // nothing left to test at this level — see `StartBound`/`EndBound`'s doc comments.

    /// An inverted range (`start_index > end_index`) used to underflow `end_index -
    /// start_index` (panic in debug, huge `usize` then out-of-bounds `key_at` in release). It
    /// must be dropped instead. Unlike the bound-shape cases above, this one *is* still
    /// representable on the wire (both bounds are individually legal shapes) and can only be
    /// detected against a concrete tree, so it stays a runtime check (`BoundedRange::parse`).
    #[test]
    fn inverted_range_is_dropped_not_panicking() {
        let store = tree(&[10, 20, 30]);
        let segment = HashSegment {
            // start_index = insertion_position(100) = 3, end_index = insertion_position(5) = 0
            range: SegmentRange::new(StartBound::Included(100), EndBound::Excluded(5)),
            hash: Fingerprint([1, 0, 0, 0]),
            size: 1,
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
        let segment = HashSegment {
            range: SegmentRange::new(StartBound::Unbounded, EndBound::Unbounded),
            hash: Fingerprint::ZERO,
            size: 0,
        };
        let (_out_comparison, differences) = round(&store, segment);
        assert_eq!(differences, vec![(Bound::Unbounded, Bound::Unbounded)]);
    }

    // ----- Emptiness and equality are decided on `size`, never on the -----
    // range fingerprint. A range fingerprint combines per-element hashes additively, so a
    // non-empty range can legitimately fingerprint to `ZERO` and two different ranges can
    // fingerprint equally. The segment fields below are exactly what such a colliding (or
    // hostile) peer puts on the wire; we drive them straight through `diff_round`.

    /// Headline counterexample. A *non-empty* peer range that fingerprints to `ZERO`
    /// (e.g. two elements whose per-element hashes cancel) is advertised against our empty
    /// tree, which also fingerprints to `ZERO`. The hashes match (`ZERO == ZERO`) but the
    /// sizes differ (`2 != 0`). The buggy code short-circuited on the first `hash ==
    /// local_hash` check and concluded "in sync", silently losing the peer's two elements.
    /// With the size-based decision we must instead bounce the range back so the peer sends
    /// us its content.
    #[test]
    fn nonempty_zero_hash_vs_empty_is_not_in_sync() {
        let store = tree(&[]); // empty: local_hash == ZERO, local_size == 0
        let segment = HashSegment {
            range: SegmentRange::new(StartBound::Unbounded, EndBound::Unbounded),
            hash: Fingerprint::ZERO, // collides with our empty fingerprint ...
            size: 2,                 // ... but the peer is *not* empty
        };
        let (out_comparison, differences) = round(&store, segment);
        // Must not be swallowed as "in sync": we bounce an empty segment back so the peer
        // sends us the divergent items it holds.
        assert!(differences.is_empty());
        assert_eq!(out_comparison.len(), 1);
        assert_eq!(
            out_comparison[0],
            HashSegment {
                range: SegmentRange::new(StartBound::Unbounded, EndBound::Unbounded),
                hash: Fingerprint::ZERO,
                size: 0,
            }
        );
    }

    /// Dual: equal fingerprints with equal sizes over a *non-empty* range are correctly
    /// concluded in sync (the size check does not produce false differences). We advertise
    /// the tree's own real fingerprint and size back to it.
    #[test]
    fn matching_hash_and_size_is_in_sync() {
        let store = tree(&[10, 20, 30]);
        let segment = HashSegment {
            range: SegmentRange::new(StartBound::Unbounded, EndBound::Unbounded),
            hash: store.hash(&..),
            size: store.len(),
        };
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
    }

    /// Branch: equal fingerprints but *different* sizes over a non-empty range must not
    /// be mistaken for "in sync"; the range is refined instead. We feed the tree's own
    /// fingerprint with a deliberately wrong (larger) size, forcing the fan-out branch.
    #[test]
    fn matching_hash_but_wrong_size_is_refined() {
        let store = tree(&[10, 20, 30, 40, 50]);
        let segment = HashSegment {
            range: SegmentRange::new(StartBound::Unbounded, EndBound::Unbounded),
            hash: store.hash(&..), // hashes collide ...
            size: store.len() + 7, // ... but the advertised size is wrong
        };
        let (out_comparison, differences) = round(&store, segment);
        // Not concluded in sync: the range is subdivided and bounced back for refinement.
        assert!(!out_comparison.is_empty());
        assert!(differences.is_empty());
    }
}
