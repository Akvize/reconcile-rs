// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides two traits:
//! [`HashRangeQueryable`] and [`Diffable`].

use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::fingerprint::Fingerprint;

/// Provides the necessary methods to be able
/// to efficiently determine and compare
/// differences between two key stores:
/// * [`hash`](HashRangeQueryable::hash),
/// * [`insertion_position`](HashRangeQueryable::insertion_position),
/// * [`key_at`](HashRangeQueryable::key_at),
/// * [`len`](HashRangeQueryable::len)
///
/// (with [`is_empty`](HashRangeQueryable::is_empty) as a default implementation).
///
/// This is a low-level trait.
pub trait HashRangeQueryable {
    type Key;
    /// Cumulated [`Fingerprint`] over a given range of keys: the combination
    /// (256-bit addition, see [`crate::fingerprint`]) of the per-element hashes
    /// of every element in the range.
    fn hash<R: RangeBounds<Self::Key>>(&self, range: &R) -> Fingerprint;
    /// Position of the given key in the collection, if it exists, or position where it would be after insertion otherwise
    fn insertion_position(&self, key: &Self::Key) -> usize;
    /// Reference to the [`Key`](HashRangeQueryable::Key) at a given position. Panics if the key is not in the collection.
    fn key_at(&self, index: usize) -> &Self::Key;
    /// Number of elements in the collection.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Represents the elements of the collections in the given key range. The `hash` and `size` fields allow testing whether the two segments represent the same elements.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HashSegment<K> {
    range: (Bound<K>, Bound<K>),
    hash: Fingerprint,
    size: usize,
}

pub type DiffRange<K> = (Bound<K>, Bound<K>);

/// Exposes two methods that can be used to implement a reconciliation protocol over a network.
pub trait Diffable {
    type ComparisonItem;
    type DifferenceItem;
    /// Returns a representation of all the elements in the collection
    /// that can be sent to `diff_round`; for instance, an accumulated hash of the elements
    fn start_diff(&self) -> Vec<Self::ComparisonItem>;
    /// Refines set differences (typically, a range of keys along with the accumulated hash) into smaller sets.
    ///
    /// When sets are determined to contains the same elements, they can be remove from the output.
    /// When sets are determinied to only contains differing elements,
    /// the corresponding elements are listed as `differences`.
    /// In other cases, the set must be refined and sent back to the peer for further analysis.
    fn diff_round(
        &self,
        in_comparison: Vec<Self::ComparisonItem>,
        out_comparison: &mut Vec<Self::ComparisonItem>,
        differences: &mut Vec<Self::DifferenceItem>,
    );
}

impl<K: Clone, T: HashRangeQueryable<Key = K>> Diffable for T {
    type ComparisonItem = HashSegment<K>;
    type DifferenceItem = DiffRange<K>;

    fn start_diff(&self) -> Vec<Self::ComparisonItem> {
        vec![HashSegment {
            range: (Bound::Unbounded, Bound::Unbounded),
            hash: self.hash(&..),
            size: self.len(),
        }]
    }

    fn diff_round(
        &self,
        in_comparison: Vec<Self::ComparisonItem>,
        out_comparison: &mut Vec<Self::ComparisonItem>,
        differences: &mut Vec<Self::DifferenceItem>,
    ) {
        for segment in in_comparison {
            let HashSegment { range, hash, size } = segment;
            let local_hash = self.hash(&range);
            let (start_bound, end_bound) = range;
            // `HashSegment` derives `Deserialize` and is fed straight from the wire
            // (`reconcile_engine`), with no validation. A crafted or version-mismatched peer
            // can therefore send bound shapes this engine never emits. Our protocol only
            // produces `Included`/`Unbounded` start bounds and `Excluded`/`Unbounded` end
            // bounds (see `start_diff` and the segments pushed below). Rather than reaching
            // `unimplemented!()` — a remote-triggerable panic that would kill the
            // reconciliation task (issue #112) — we drop any other bound shape and move on.
            let start_index = match start_bound.as_ref() {
                Bound::Unbounded => 0,
                Bound::Included(key) => self.insertion_position(key),
                Bound::Excluded(_) => {
                    debug!("dropping segment with unsupported excluded start bound");
                    continue;
                }
            };
            let end_index = match end_bound.as_ref() {
                Bound::Unbounded => self.len(),
                Bound::Excluded(key) => self.insertion_position(key),
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
            // permanent divergence. See issues #106 and #111.
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
                            hash: self.hash(&range),
                            range,
                            size: end_index - cur_index,
                        });
                        break;
                    } else {
                        let next_key = self.key_at(next_index);
                        let range = (cur_bound, Bound::Excluded(next_key.clone()));
                        out_comparison.push(HashSegment {
                            hash: self.hash(&range),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-memory store over sorted, distinct `i32` keys implementing
    /// [`HashRangeQueryable`]. The blanket impl above gives us [`Diffable`] for free, so we
    /// can drive crafted [`HashSegment`]s straight through [`diff_round`] — the code path a
    /// hostile peer reaches over the network. Living in the same module, the test can build
    /// the otherwise-private `HashSegment` directly.
    struct MockStore {
        keys: Vec<i32>,
    }

    impl MockStore {
        fn new(mut keys: Vec<i32>) -> Self {
            keys.sort_unstable();
            keys.dedup();
            MockStore { keys }
        }
    }

    impl HashRangeQueryable for MockStore {
        type Key = i32;

        fn hash<R: RangeBounds<i32>>(&self, range: &R) -> Fingerprint {
            self.keys
                .iter()
                .filter(|k| range.contains(k))
                .fold(Fingerprint::ZERO, |acc, &k| {
                    let m = (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                    acc + Fingerprint([m, 0, 0, 0])
                })
        }

        fn insertion_position(&self, key: &i32) -> usize {
            self.keys.partition_point(|k| k < key)
        }

        fn key_at(&self, index: usize) -> &i32 {
            &self.keys[index]
        }

        fn len(&self) -> usize {
            self.keys.len()
        }
    }

    /// Run a single crafted segment through `diff_round` and return whatever it produced.
    /// The point of these tests is that the call *returns* at all (no panic).
    fn round(
        store: &MockStore,
        segment: HashSegment<i32>,
    ) -> (Vec<HashSegment<i32>>, Vec<DiffRange<i32>>) {
        let mut out_comparison = Vec::new();
        let mut differences = Vec::new();
        store.diff_round(vec![segment], &mut out_comparison, &mut differences);
        (out_comparison, differences)
    }

    /// Issue #112: an `Excluded` start bound used to reach `unimplemented!()`. Both early
    /// `hash`/`size` checks are bypassed (the range is non-empty so `local_hash != 0`, and
    /// `size`/`hash` are chosen to differ), so control reaches the bound match. The segment
    /// must be dropped, not panic.
    #[test]
    fn excluded_start_bound_is_dropped_not_panicking() {
        let store = MockStore::new(vec![10, 20, 30]);
        let segment = HashSegment {
            range: (Bound::Excluded(0), Bound::Unbounded),
            hash: Fingerprint([1, 0, 0, 0]),
            size: 1,
        };
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
    }

    /// Issue #112: an `Included` end bound used to reach `unimplemented!()`.
    #[test]
    fn included_end_bound_is_dropped_not_panicking() {
        let store = MockStore::new(vec![10, 20, 30]);
        let segment = HashSegment {
            range: (Bound::Unbounded, Bound::Included(20)),
            hash: Fingerprint([1, 0, 0, 0]),
            size: 1,
        };
        let (out_comparison, differences) = round(&store, segment);
        assert!(out_comparison.is_empty());
        assert!(differences.is_empty());
    }

    /// Issue #112: an inverted range (`start_index > end_index`) used to underflow
    /// `end_index - start_index` (panic in debug, huge `usize` then out-of-bounds `key_at`
    /// in release). It must be dropped instead.
    #[test]
    fn inverted_range_is_dropped_not_panicking() {
        let store = MockStore::new(vec![10, 20, 30]);
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

    /// A well-formed segment from an empty peer over our whole, non-empty range must still be
    /// recognised as a difference we owe — i.e. the validation guards do not swallow the
    /// legitimate `(Unbounded, Unbounded)` shape the protocol actually uses.
    #[test]
    fn wellformed_segment_still_processed() {
        let store = MockStore::new(vec![10, 20, 30]);
        let segment = HashSegment {
            range: (Bound::Unbounded, Bound::Unbounded),
            hash: Fingerprint::ZERO,
            size: 0,
        };
        let (_out_comparison, differences) = round(&store, segment);
        assert_eq!(differences, vec![(Bound::Unbounded, Bound::Unbounded)]);
    }
}
