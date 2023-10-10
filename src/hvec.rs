use std::hash::Hash;
use std::ops::{Bound, RangeBounds};

use crate::diff::HashRangeQueryable;
use crate::hash::hash;

pub struct HVec<K, V> {
    keys: Vec<K>,
    values: Vec<V>,
    hashes: Vec<u64>,
}

// see https://github.com/rust-lang/rust/issues/26925
impl<K, V> Default for HVec<K, V> {
    fn default() -> Self {
        HVec {
            keys: Vec::new(),
            values: Vec::new(),
            hashes: Vec::new(),
        }
    }
}

impl<K: Hash + Ord, V: Hash> HVec<K, V> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.keys.binary_search(&key) {
            Result::Ok(index) => {
                self.hashes[index] = hash(&key, &value);
                Some(std::mem::replace(&mut self.values[index], value))
            }
            Result::Err(index) => {
                self.hashes.insert(index, hash(&key, &value));
                self.keys.insert(index, key);
                self.values.insert(index, value);
                None
            }
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        match self.keys.binary_search(key) {
            Result::Ok(index) => {
                self.keys.remove(index);
                self.hashes.remove(index);
                Some(self.values.remove(index))
            }
            Result::Err(_) => None,
        }
    }
}

fn ok_index(result: Result<usize, usize>) -> usize {
    match result {
        Ok(index) => index,
        Err(index) => index,
    }
}

impl<K: Ord, V> HVec<K, V> {
    fn index_bound_of_key_bound(&self, key: Bound<&K>) -> Bound<usize> {
        match key {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(key) => Bound::Included(ok_index(self.keys.binary_search(key))),
            Bound::Excluded(key) => Bound::Excluded(ok_index(self.keys.binary_search(key))),
        }
    }

    fn index_range_of_key_range<R: RangeBounds<K>>(
        &self,
        range: R,
    ) -> (Bound<usize>, Bound<usize>) {
        (
            self.index_bound_of_key_bound(range.start_bound()),
            self.index_bound_of_key_bound(range.end_bound()),
        )
    }
}

impl<K: Ord, V> HashRangeQueryable for HVec<K, V> {
    type Key = K;
    fn hash<R: RangeBounds<Self::Key>>(&self, range: R) -> u64 {
        self.hashes[self.index_range_of_key_range(range)]
            .iter()
            .fold(0, |acc, val| acc ^ val)
    }

    fn insertion_position(&self, key: &Self::Key) -> usize {
        ok_index(self.keys.binary_search(key))
    }

    fn key_at(&self, index: usize) -> &Self::Key {
        &self.keys[index]
    }

    fn len(&self) -> usize {
        unimplemented!();
    }
}
