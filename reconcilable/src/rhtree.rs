use core::hash::Hash;

use chrono::{DateTime, Utc};

use diff::HashRangeQueryable;
use htree::{HTree, Iter};

use crate::Reconcilable;

type TV<V> = (DateTime<Utc>, V);

pub struct RHTree<K, V> {
    tree: HTree<K, TV<V>>,
}

impl<K: Hash + Ord, V: Hash> RHTree<K, V> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn get<'a>(&'a self, key: &'a K) -> Option<&'a TV<V>> {
        self.tree.get(key)
    }

    pub fn position(&self, key: &K) -> Option<usize> {
        self.tree.position(key)
    }

    pub fn insert(&mut self, key: K, time: DateTime<Utc>, value: V) -> Option<TV<V>> {
        self.tree.insert(key, (time, value))
    }

    pub fn remove(&mut self, key: &K) -> Option<TV<V>> {
        self.tree.remove(key)
    }
}

impl<'a, K, V> IntoIterator for &'a RHTree<K, V> {
    type Item = (&'a K, &'a TV<V>);
    type IntoIter = Iter<'a, K, TV<V>>;
    fn into_iter(self) -> Self::IntoIter {
        self.tree.into_iter()
    }
}

impl<K, V> RHTree<K, V> {
    pub fn iter(&self) -> Iter<K, TV<V>> {
        self.into_iter()
    }
}

impl<K: Hash + Ord, V: Hash> Default for RHTree<K, V> {
    fn default() -> Self {
        RHTree {
            tree: HTree::default(),
        }
    }
}

impl<K, V> Reconcilable for RHTree<K, V>
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    type Value = TV<V>;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> Option<u64> {
        let mut updated = false;
        // here, using `Option::map` is clearer than using `if let Some(…) =` because of the
        // long match expression
        #[allow(clippy::option_map_unit_fn)]
        for (k, tv) in updates {
            match self.tree.get(&k) {
                Some(local_tv) => {
                    if tv.0 > local_tv.0 {
                        Some(tv)
                    } else {
                        None
                    }
                }
                None => Some(tv),
            }
            .map(|v| {
                self.tree.insert(k, v);
                updated = true;
            });
        }
        updated.then(|| self.tree.hash(&..))
    }

    fn enumerate_diff_ranges(
        &self,
        diff_ranges: diff::DiffRanges<Self::Key>,
    ) -> Vec<(Self::Key, Self::Value)> {
        let mut ret = Vec::new();
        for diff in diff_ranges {
            for (k, v) in self.tree.get_range(&diff) {
                ret.push((k.clone(), v.clone()));
            }
        }
        ret
    }
}

impl<K: Hash + Ord, V: Hash> HashRangeQueryable for RHTree<K, V> {
    type Key = K;

    fn hash<R: std::ops::RangeBounds<Self::Key>>(&self, range: &R) -> u64 {
        self.tree.hash(range)
    }
    fn insertion_position(&self, key: &Self::Key) -> usize {
        self.tree.insertion_position(key)
    }
    fn key_at(&self, index: usize) -> &Self::Key {
        self.tree.key_at(index)
    }
    fn len(&self) -> usize {
        self.tree.len()
    }
}
