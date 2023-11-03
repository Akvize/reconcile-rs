use std::ops::{Bound, RangeBounds};

use serde::{Deserialize, Serialize};

pub trait HashRangeQueryable {
    type Key;
    fn hash<R: RangeBounds<Self::Key>>(&self, range: &R) -> u64;
    fn insertion_position(&self, key: &Self::Key) -> usize;
    fn key_at(&self, index: usize) -> &Self::Key;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HashSegment<K> {
    range: (Bound<K>, Bound<K>),
    hash: u64,
    size: usize,
}

pub type DiffRange<K> = (Bound<K>, Bound<K>);

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
