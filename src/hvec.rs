use std::hash::Hash;
use std::iter::{zip, Zip};
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

    pub fn position(&self, key: &K) -> Option<usize> {
        self.keys.binary_search(key).ok()
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

impl<K, V> PartialEq for HVec<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.hashes == other.hashes
    }
}

impl<K, V> Eq for HVec<K, V> {}

impl<K: Hash + Ord, V: Hash> FromIterator<(K, V)> for HVec<K, V> {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
    {
        let mut vec = HVec::new();
        for (k, v) in iter {
            vec.insert(k, v);
        }
        vec
    }
}

impl<K, V> IntoIterator for HVec<K, V> {
    type Item = (K, V);
    type IntoIter = Zip<std::vec::IntoIter<K>, std::vec::IntoIter<V>>;
    fn into_iter(self) -> Self::IntoIter {
        zip(self.keys, self.values)
    }
}

impl<'a, K, V> IntoIterator for &'a HVec<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Zip<std::slice::Iter<'a, K>, std::slice::Iter<'a, V>>;
    fn into_iter(self) -> Self::IntoIter {
        zip(self.keys.iter(), self.values.iter())
    }
}

impl<K, V> HVec<K, V> {
    fn iter(&self) -> <&Self as IntoIterator>::IntoIter {
        self.into_iter()
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for HVec<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
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
        self.keys.len()
    }
}

#[test]
fn test_simple() {
    // empty
    let mut vec = HVec::new();
    assert_eq!(vec.hash(..), 0);

    // 1 value
    vec.insert(50, "Hello");
    let hash1 = vec.hash(..);
    assert_ne!(hash1, 0);

    // 2 values
    vec.insert(25, "World!");
    let hash2 = vec.hash(..);
    assert_ne!(hash2, 0);
    assert_ne!(hash2, hash1);

    // 3 values
    vec.insert(75, "Everyone!");
    let hash3 = vec.hash(..);
    assert_ne!(hash3, 0);
    assert_ne!(hash3, hash1);
    assert_ne!(hash3, hash2);

    // back to 2 values
    vec.remove(&75);
    let hash4 = vec.hash(..);
    assert_eq!(hash4, hash2);
}
