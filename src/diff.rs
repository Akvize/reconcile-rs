// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This [`module`](crate::diff) provides two traits:
//! [`HashRangeQueryable`] and [`Diffable`].

use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};

/// Provides the necessary methods to be able
/// to efficiently determine and compare
/// differences between two key stores:
/// * [`hash`](HashRangeQueryable::hash),
/// * [`insertion_position`](HashRangeQueryable::insertion_position),
/// * [`key_at`](HashRangeQueryable::key_at),
/// * [`len`](HashRangeQueryable::len)
/// (with [`is_empty`](HashRangeQueryable::is_empty) as a default implementation).
/// This is a low-level trait.
pub trait HashRangeQueryable {
    type Key;
    /// Cumulated hash over a given range of keys. For instance, it could be the XOR of all the hashes of the elements in the range.
    fn hash<R: RangeBounds<Self::Key>>(&self, range: &R) -> u64;
    /// Position of the given key in the collection, if it exists, or position where it would be after insertion otherwise
    fn insertion_position(&self, key: &Self::Key) -> usize;
    /// Reference to the [`Key`](HashRangeQueryable::Key) at a given position.
    fn key_at(&self, index: usize) -> &Self::Key;
    /// Number of [`Keys`](HashRangeQueryable::Key) in the collection.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A key [`range`](RangeBounds),
/// along with its cumulated hash and size,
/// representing the number of keys it contains.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HashSegment<K> {
    range: (Bound<K>, Bound<K>),
    hash: u64,
    size: usize,
}

pub type DiffRange<K> = (Bound<K>, Bound<K>);

/// Provides a middle-level API to enable two stores
/// to communicate their differences,
/// through rounds of communication.
///
/// Exposes two methods:
///
/// * [`start_diff`](Diffable::start_diff) to start a comparison
/// by sending a collection of [`ComparisonItems`](Diffable::ComparisonItem),
/// * [`diff_round`](Diffable::diff_round)
/// in response to a received collection
/// of [`ComparisonItems`](Diffable::ComparisonItem), to send
///    * a collection of [`ComparisonItems`](Diffable::ComparisonItem)
///    * a collection of [`DifferenceItems`](Diffable::DifferenceItem)
///
/// Provides a default implementation
/// for any [`HashRangeQueryable`] type
/// with [`cloneable`](Clone) keys.
pub trait Diffable {
    type ComparisonItem;
    type DifferenceItem;
    fn start_diff(&self) -> Vec<Self::ComparisonItem>;
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
            let HashSegment { range, hash, size } = segment.clone();
            let local_hash = self.hash(&range);
            if hash == local_hash {
                continue;
            } else if hash == 0 {
                differences.push(range);
                continue;
            } else if local_hash == 0 {
                // present on remote; bounce back to the remote
                out_comparison.push(HashSegment {
                    range,
                    hash: 0,
                    size: 0,
                });
                continue;
            }
            let (start_bound, end_bound) = range;
            let start_index = match start_bound.as_ref() {
                Bound::Unbounded => 0,
                Bound::Included(key) => self.insertion_position(key),
                Bound::Excluded(_) => unimplemented!(),
            };
            let end_index = match end_bound.as_ref() {
                Bound::Unbounded => self.len(),
                Bound::Included(_) => unimplemented!(),
                Bound::Excluded(key) => self.insertion_position(key),
            };
            let local_size = end_index - start_index;
            if size == 0 || local_size == 0 {
                // handled by the hash checks above
                unreachable!();
            } else if size == 1 && local_size == 1 {
                // ask the remote to send us the conflicting item
                out_comparison.push(HashSegment {
                    range: (start_bound.clone(), end_bound.clone()),
                    hash: 0,
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
