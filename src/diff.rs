use std::ops::{Bound, RangeBounds};

pub trait HashRangeQueryable {
    type Key;
    fn hash<R: RangeBounds<Self::Key>>(&self, range: R) -> u64;
    fn insertion_position(&self, key: &Self::Key) -> usize;
    fn key_at(&self, index: usize) -> &Self::Key;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashSegment<'a, K> {
    range: (Bound<&'a K>, Bound<&'a K>),
    hash: u64,
    size: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Diff<K> {
    InSelf((Bound<K>, Bound<K>)),
    InOther((Bound<K>, Bound<K>)),
    InBoth((Bound<K>, Bound<K>)),
}

pub trait Diffable {
    type Key;
    fn start_diff(&self) -> Vec<HashSegment<Self::Key>>;
    fn diff_round<'a>(
        &'a self,
        diffs: &mut Vec<Diff<&'a Self::Key>>,
        segments: Vec<HashSegment<'a, Self::Key>>,
    ) -> Vec<HashSegment<'a, Self::Key>>;
    fn diff(&self, other: &Self) -> Vec<Diff<Self::Key>>;
}

impl<K: Clone, T: HashRangeQueryable<Key = K>> Diffable for T {
    type Key = K;
    fn diff(&self, other: &T) -> Vec<Diff<K>> {
        fn aux<'a, K: Clone, T: HashRangeQueryable<Key = K>>(
            self_: &'a T,
            other: &'a T,
            range: (Bound<&'a K>, Bound<&'a K>),
            output: &mut Vec<Diff<K>>,
        ) {
            match (self_.hash(range), other.hash(range)) {
                (a, b) if a == b => return,
                (_, 0) => {
                    output.push(Diff::InSelf((range.0.cloned(), range.1.cloned())));
                    return;
                }
                (0, _) => {
                    output.push(Diff::InOther((range.0.cloned(), range.1.cloned())));
                    return;
                }
                (_, _) => (),
            }
            let (start_bound, end_bound) = range;
            let self_start_index = match start_bound {
                Bound::Unbounded => 0,
                Bound::Included(key) => self_.insertion_position(key),
                Bound::Excluded(_) => unreachable!(),
            };
            let self_end_index = match end_bound {
                Bound::Unbounded => self_.len(),
                Bound::Included(_) => unreachable!(),
                Bound::Excluded(key) => self_.insertion_position(key),
            };
            let other_start_index = match start_bound {
                Bound::Unbounded => 0,
                Bound::Included(key) => other.insertion_position(key),
                Bound::Excluded(_) => unreachable!(),
            };
            let other_end_index = match end_bound {
                Bound::Unbounded => other.len(),
                Bound::Included(_) => unreachable!(),
                Bound::Excluded(key) => other.insertion_position(key),
            };
            let self_count = self_end_index - self_start_index;
            let other_count = other_end_index - other_start_index;
            match (self_count, other_count) {
                (0, 0) => unreachable!(),          // both hashes would be 0, and thus equal
                (0, _) | (_, 0) => unreachable!(), // detected above since hashes equal to 0
                (1, 1) => {
                    // either the same key with different
                    // values, or two different keys; in
                    // any cases, the range should be
                    // exchanged
                    output.push(Diff::InBoth((range.0.cloned(), range.1.cloned())));
                }
                (a, _) => {
                    let mid_key = if a == 1 {
                        // recurse w.r.t. other
                        other.key_at(other_start_index + (other_end_index - other_start_index) / 2)
                    } else {
                        // recurse w.r.t. self
                        self_.key_at(self_start_index + (self_end_index - self_start_index) / 2)
                    };
                    // recurse left
                    let left_range = (start_bound, Bound::Excluded(mid_key));
                    aux(self_, other, left_range, output);
                    // recurse right
                    let right_range = (Bound::Included(mid_key), end_bound);
                    aux(self_, other, right_range, output);
                }
            }
        }
        let mut ret = Vec::new();
        aux(self, other, (Bound::Unbounded, Bound::Unbounded), &mut ret);
        ret
    }

    fn start_diff(&self) -> Vec<HashSegment<Self::Key>> {
        vec![HashSegment {
            range: (Bound::Unbounded, Bound::Unbounded),
            hash: self.hash(..),
            size: self.len(),
        }]
    }

    fn diff_round<'a>(
        &'a self,
        diffs: &mut Vec<Diff<&'a Self::Key>>,
        segments: Vec<HashSegment<'a, Self::Key>>,
    ) -> Vec<HashSegment<'a, Self::Key>> {
        let mut ret = Vec::new();
        for segment in segments {
            let HashSegment { range, hash, size } = segment;
            let local_hash = self.hash(range);
            if hash == local_hash {
                continue;
            } else if hash == 0 {
                diffs.push(Diff::InSelf(range));
                continue;
            } else if local_hash == 0 {
                // present on other side; bounce back to the remote
                ret.push(HashSegment {
                    range,
                    hash: 0,
                    size: 0,
                });
                continue;
            }
            let (start_bound, end_bound) = range;
            let start_index = match start_bound {
                Bound::Unbounded => 0,
                Bound::Included(key) => self.insertion_position(key),
                Bound::Excluded(_) => unimplemented!(),
            };
            let end_index = match end_bound {
                Bound::Unbounded => self.len(),
                Bound::Included(_) => unimplemented!(),
                Bound::Excluded(key) => self.insertion_position(key),
            };
            let local_size = end_index - start_index;
            if size == 0 || local_size == 0 {
                // handled by the hash checks above
                unreachable!();
            } else if size == 1 && local_size == 1 {
                diffs.push(Diff::InBoth(range));
            } else if local_size == 1 {
                // not enough information; bounce back to the remote
                ret.push(HashSegment {
                    range,
                    hash: local_hash,
                    size: local_size,
                });
            } else {
                // NOTE: end_index - start_index ≥ 2
                let mid_index = start_index + (end_index - start_index) / 2;
                assert_ne!(mid_index, start_index);
                assert_ne!(mid_index, end_index);
                let mid_key = self.key_at(mid_index);
                let left_range = (start_bound, Bound::Excluded(mid_key));
                ret.push(HashSegment {
                    range: left_range,
                    hash: self.hash(left_range),
                    size: mid_index - start_index,
                });
                let right_range = (Bound::Included(mid_key), end_bound);
                ret.push(HashSegment {
                    range: right_range,
                    hash: self.hash(right_range),
                    size: end_index - mid_index,
                });
            }
        }
        ret
    }
}
