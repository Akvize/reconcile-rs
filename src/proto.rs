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

use std::ops::Bound;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::fingerprint::Fingerprint;
use crate::hrtree::HRTree;

/// Represents the elements of the collection in the given key range. The `hash` and `size`
/// fields allow testing whether two segments represent the same elements.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HashSegment<K> {
    range: (Bound<K>, Bound<K>),
    hash: Fingerprint,
    size: usize,
}

pub type DiffRange<K> = (Bound<K>, Bound<K>);

/// Returns a representation of all the elements in the tree that can be sent to [`diff_round`]:
/// the root segment `{(−∞, +∞), global hash, size}` that bootstraps a reconciliation.
pub fn start_diff<K, V>(tree: &HRTree<K, V>) -> Vec<HashSegment<K>>
where
    K: std::hash::Hash + Ord,
    V: std::hash::Hash,
{
    vec![HashSegment {
        range: (Bound::Unbounded, Bound::Unbounded),
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
        let HashSegment { range, hash, size } = segment;
        let local_hash = tree.hash(&range);
        let (start_bound, end_bound) = range;
        // `HashSegment` derives `Deserialize` and is fed straight from the wire
        // (`reconcile_engine`), with no validation. A crafted or version-mismatched peer
        // can therefore send bound shapes this engine never emits. Our protocol only
        // produces `Included`/`Unbounded` start bounds and `Excluded`/`Unbounded` end
        // bounds (see `start_diff` and the segments pushed below). Rather than reaching
        // `unimplemented!()` — a remote-triggerable panic that would kill the
        // reconciliation task — we drop any other bound shape and move on.
        let start_index = match start_bound.as_ref() {
            Bound::Unbounded => 0,
            Bound::Included(key) => tree.insertion_position(key),
            Bound::Excluded(_) => {
                debug!("dropping segment with unsupported excluded start bound");
                continue;
            }
        };
        let end_index = match end_bound.as_ref() {
            Bound::Unbounded => tree.len(),
            Bound::Excluded(key) => tree.insertion_position(key),
            Bound::Included(_) => {
                debug!("dropping segment with unsupported included end bound");
                continue;
            }
        };
        // A crafted segment can also carry an inverted or out-of-order range
        // (`start_index > end_index`, e.g. `Included(100)..Excluded(5)`). That would
        // underflow `end_index - start_index` (panic in debug, wrap to a huge `usize`
        // in release) and then index out of bounds in `key_at`. Drop such segments
        // instead of trusting the arithmetic.
        let local_size = match end_index.checked_sub(start_index) {
            Some(local_size) => local_size,
            None => {
                debug!("dropping segment with inverted range");
                continue;
            }
        };
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
            differences.push((start_bound, end_bound));
            continue;
        } else if local_size == 0 {
            // present on remote; bounce back to the remote
            out_comparison.push(HashSegment {
                range: (start_bound, end_bound),
                hash: Fingerprint::ZERO,
                size: 0,
            });
            continue;
        } else if size == 1 && local_size == 1 {
            // ask the remote to send us the conflicting item
            out_comparison.push(HashSegment {
                range: (start_bound.clone(), end_bound.clone()),
                hash: Fingerprint::ZERO,
                size: 0,
            });
            // send the conflicting item to the remote
            differences.push((start_bound, end_bound));
        } else if local_size == 1 {
            // not enough information; bounce back to the remote
            out_comparison.push(HashSegment {
                range: (start_bound, end_bound),
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
                    let range = (cur_bound, end_bound);
                    out_comparison.push(HashSegment {
                        hash: tree.hash(&range),
                        range,
                        size: end_index - cur_index,
                    });
                    break;
                } else {
                    let next_key = tree.key_at(next_index);
                    let range = (cur_bound, Bound::Excluded(next_key.clone()));
                    out_comparison.push(HashSegment {
                        hash: tree.hash(&range),
                        range,
                        size: next_index - cur_index,
                    });
                    cur_bound = Bound::Included(next_key.clone());
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

    /// An `Excluded` start bound used to reach `unimplemented!()`. The segment must be
    /// dropped, not panic.
    #[test]
    fn excluded_start_bound_is_dropped_not_panicking() {
        let store = tree(&[10, 20, 30]);
        let segment = HashSegment {
            range: (Bound::Excluded(0), Bound::Unbounded),
            hash: Fingerprint([1, 0, 0, 0]),
            size: 1,
        };
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
    }

    /// An `Included` end bound used to reach `unimplemented!()`.
    #[test]
    fn included_end_bound_is_dropped_not_panicking() {
        let store = tree(&[10, 20, 30]);
        let segment = HashSegment {
            range: (Bound::Unbounded, Bound::Included(20)),
            hash: Fingerprint([1, 0, 0, 0]),
            size: 1,
        };
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
    }

    /// An inverted range (`start_index > end_index`) used to underflow `end_index -
    /// start_index` (panic in debug, huge `usize` then out-of-bounds `key_at` in release). It
    /// must be dropped instead.
    #[test]
    fn inverted_range_is_dropped_not_panicking() {
        let store = tree(&[10, 20, 30]);
        let segment = HashSegment {
            // start_index = insertion_position(100) = 3, end_index = insertion_position(5) = 0
            range: (Bound::Included(100), Bound::Excluded(5)),
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
            range: (Bound::Unbounded, Bound::Unbounded),
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
            range: (Bound::Unbounded, Bound::Unbounded),
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
                range: (Bound::Unbounded, Bound::Unbounded),
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
            range: (Bound::Unbounded, Bound::Unbounded),
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
            range: (Bound::Unbounded, Bound::Unbounded),
            hash: store.hash(&..), // hashes collide ...
            size: store.len() + 7, // ... but the advertised size is wrong
        };
        let (out_comparison, differences) = round(&store, segment);
        // Not concluded in sync: the range is subdivided and bounced back for refinement.
        assert!(!out_comparison.is_empty());
        assert!(differences.is_empty());
    }
}
