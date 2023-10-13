use std::cmp::Ordering;
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
        let mut vec: Vec<_> = iter.into_iter().collect();
        vec.sort_by(|a, b| a.0.cmp(&b.0));
        let mut keys = Vec::with_capacity(vec.len());
        let mut values = Vec::with_capacity(vec.len());
        let mut hashes = Vec::with_capacity(vec.len());
        for (k, v) in vec {
            hashes.push(hash(&k, &v));
            keys.push(k);
            values.push(v);
        }
        HVec {
            keys,
            values,
            hashes,
        }
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
        range: &R,
    ) -> (Bound<usize>, Bound<usize>) {
        (
            self.index_bound_of_key_bound(range.start_bound()),
            self.index_bound_of_key_bound(range.end_bound()),
        )
    }
}

impl<K: Ord, V> HashRangeQueryable for HVec<K, V> {
    type Key = K;
    fn hash<R: RangeBounds<Self::Key>>(&self, range: &R) -> u64 {
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
    assert_eq!(vec.hash(&..), 0);

    // 1 value
    vec.insert(50, "Hello");
    let hash1 = vec.hash(&..);
    assert_ne!(hash1, 0);

    // 2 values
    vec.insert(25, "World!");
    let hash2 = vec.hash(&..);
    assert_ne!(hash2, 0);
    assert_ne!(hash2, hash1);

    // 3 values
    vec.insert(75, "Everyone!");
    let hash3 = vec.hash(&..);
    assert_ne!(hash3, 0);
    assert_ne!(hash3, hash1);
    assert_ne!(hash3, hash2);

    // back to 2 values
    vec.remove(&75);
    let hash4 = vec.hash(&..);
    assert_eq!(hash4, hash2);
}

#[cfg(test)]
mod tests {
    use rand::{seq::SliceRandom, Rng, SeedableRng};

    use std::ops::Bound;

    use crate::diff::{Diff, Diffable};

    use super::{HVec, HashRangeQueryable};

    #[test]
    fn test_compare() {
        let vec1 = HVec::from_iter([(25, "World!"), (50, "Hello"), (75, "Everyone!")]);
        let vec2 = HVec::from_iter([(75, "Everyone!"), (50, "Hello"), (25, "World!")]);
        let vec3 = HVec::from_iter([(75, "Everyone!"), (25, "World!"), (50, "Hello")]);
        let vec4 = HVec::from_iter([(75, "Everyone!"), (25, "World!"), (40, "Hello")]);
        let vec5 = HVec::from_iter([(25, "World!"), (50, "Hello"), (75, "Goodbye!")]);

        assert_eq!(vec1.hash(&..), vec1.hash(&..));
        assert_eq!(vec1.hash(&..), vec2.hash(&..));
        assert_eq!(vec1.hash(&..), vec3.hash(&..));
        assert_ne!(vec1.hash(&..), vec4.hash(&..));
        assert_ne!(vec1.hash(&..), vec5.hash(&..));

        assert_eq!(vec1, vec1);
        assert_eq!(vec1, vec2);
        assert_eq!(vec1, vec3);
        assert_ne!(vec1, vec4);
        assert_ne!(vec1, vec5);

        assert_eq!(vec1.diff(&vec1), (vec![], vec![]));
        assert_eq!(vec1.diff(&vec2), (vec![], vec![]));
        assert_eq!(vec1.diff(&vec3), (vec![], vec![]));
        assert_eq!(
            vec1.diff(&vec4),
            (
                vec![Diff((Bound::Included(40), Bound::Excluded(75))),],
                vec![Diff((Bound::Included(40), Bound::Excluded(75)))],
            ),
        );
        assert_eq!(
            vec1.diff(&vec5),
            (
                vec![Diff((Bound::Included(75), Bound::Unbounded)),],
                vec![Diff((Bound::Included(75), Bound::Unbounded))],
            ),
        );
    }

    #[test]
    fn big_test() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut vec = HVec::new();
        let mut key_values = Vec::new();

        let mut expected_hash = 0;

        // add some
        for _ in 0..1000 {
            let key: u64 = rng.gen();
            let value: u64 = rng.gen();
            vec.insert(key, value);
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(vec.hash(&..), expected_hash);
            key_values.push((key, value));
        }

        // check for partial ranges
        let mid = u64::MAX / 2;
        assert_ne!(vec.hash(&(mid..)), vec.hash(&..));
        assert_ne!(vec.hash(&..mid), vec.hash(&..));
        assert_eq!(vec.hash(&..mid) ^ vec.hash(&(mid..)), vec.hash(&..));

        // check key_at() with first and last indexes
        assert_eq!(
            key_values.iter().map(|(key, _)| key).min(),
            Some(vec.key_at(0))
        );
        assert_eq!(
            key_values.iter().map(|(key, _)| key).max(),
            Some(vec.key_at(vec.len() - 1))
        );

        // check for at/position consistency
        let key = key_values[0].0;
        let index = vec.position(&key).unwrap();
        assert_ne!(index, 0);
        assert_eq!(vec.key_at(index), &key);

        // test insertion_position
        assert_eq!(vec.insertion_position(&key), vec.position(&key).unwrap());
        assert_eq!(vec.insertion_position(&0), 0);
        assert_eq!(vec.insertion_position(&u64::MAX), vec.len());

        let items: Vec<(u64, u64)> = vec.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(items.len(), key_values.len());
        key_values.sort();
        assert_eq!(items, key_values);

        // remove some
        key_values.shuffle(&mut rng);
        for _ in 0..1000 {
            let (key, value) = key_values.pop().unwrap();
            let value2 = vec.remove(&key);
            assert_eq!(value2, Some(value));
            expected_hash ^= super::hash(&key, &value);
            assert_eq!(vec.hash(&..), expected_hash);
        }
    }
}

impl<K: Clone + Ord, V> HVec<K, V> {
    pub fn fast_diff(&self, other: &Self) -> Vec<K> {
        let mut ret = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < self.keys.len() && j < other.keys.len() {
            match self.keys[i].cmp(&other.keys[j]) {
                Ordering::Less => {
                    ret.push(self.keys[i].clone());
                    i += 1;
                }
                Ordering::Greater => {
                    ret.push(other.keys[j].clone());
                    j += 1;
                }
                Ordering::Equal => {
                    if self.hashes[i] != other.hashes[j] {
                        ret.push(self.keys[i].clone());
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        while i < self.keys.len() {
            ret.push(self.keys[i].clone());
            i += 1;
        }
        while j < other.keys.len() {
            ret.push(other.keys[j].clone());
            j += 1;
        }
        ret
    }
}
