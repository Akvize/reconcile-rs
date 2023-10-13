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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diff<K>(pub (Bound<K>, Bound<K>));

type Diffs<K> = Vec<Diff<K>>;

pub trait Diffable {
    type Key;
    fn start_diff(&self) -> Vec<HashSegment<Self::Key>>;
    fn diff_round(
        &self,
        diffs: &mut Vec<Diff<Self::Key>>,
        segments: Vec<HashSegment<Self::Key>>,
    ) -> Vec<HashSegment<Self::Key>>;

    fn diff(&self, remote: &Self) -> (Diffs<Self::Key>, Diffs<Self::Key>) {
        let mut diffs1 = Vec::new();
        let mut diffs2 = Vec::new();
        let mut segments = self.start_diff();
        while !segments.is_empty() {
            segments = remote.diff_round(&mut diffs2, segments);
            segments = self.diff_round(&mut diffs1, segments);
        }
        (diffs1, diffs2)
    }
}

impl<K: Clone, T: HashRangeQueryable<Key = K>> Diffable for T {
    type Key = K;

    fn start_diff(&self) -> Vec<HashSegment<Self::Key>> {
        vec![HashSegment {
            range: (Bound::Unbounded, Bound::Unbounded),
            hash: self.hash(&..),
            size: self.len(),
        }]
    }

    fn diff_round(
        &self,
        diffs: &mut Vec<Diff<Self::Key>>,
        segments: Vec<HashSegment<Self::Key>>,
    ) -> Vec<HashSegment<Self::Key>> {
        let mut ret = Vec::new();
        for segment in segments {
            let HashSegment { range, hash, size } = segment;
            let local_hash = self.hash(&range);
            if hash == local_hash {
                continue;
            } else if hash == 0 {
                diffs.push(Diff(range));
                continue;
            } else if local_hash == 0 {
                // present on remote; bounce back to the remote
                ret.push(HashSegment {
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
                ret.push(HashSegment {
                    range: (start_bound.clone(), end_bound.clone()),
                    hash: 0,
                    size: 0,
                });
                // send the conflicting item to the remote
                diffs.push(Diff((start_bound, end_bound)));
            } else if local_size == 1 {
                // not enough information; bounce back to the remote
                ret.push(HashSegment {
                    range: (start_bound, end_bound),
                    hash: local_hash,
                    size: local_size,
                });
            } else {
                // NOTE: end_index - start_index ≥ 2
                let mid_index = start_index + (end_index - start_index) / 2;
                assert_ne!(mid_index, start_index);
                assert_ne!(mid_index, end_index);
                let mid_key = self.key_at(mid_index);
                let left_range = (start_bound, Bound::Excluded(mid_key.clone()));
                ret.push(HashSegment {
                    hash: self.hash(&left_range),
                    range: left_range,
                    size: mid_index - start_index,
                });
                let right_range = (Bound::Included(mid_key.clone()), end_bound);
                ret.push(HashSegment {
                    hash: self.hash(&right_range),
                    range: right_range,
                    size: end_index - mid_index,
                });
            }
        }
        ret
    }
}
