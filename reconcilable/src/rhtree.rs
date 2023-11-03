use core::hash::Hash;

use diff::HashRangeQueryable;
use htree::{HTree, Iter};

use crate::Reconcilable;

type ConflictHandler<K, V> = fn(&K, &V, V) -> Option<V>;

pub struct RHTree<K, V> {
    tree: HTree<K, V>,
    conflict_handler: Option<ConflictHandler<K, V>>,
}

impl<K: Hash + Ord, V: Hash> RHTree<K, V> {
    pub fn from(tree: HTree<K, V>) -> Self {
        RHTree {
            tree,
            ..Default::default()
        }
    }

    pub fn with_conflict_handler(self, conflict_handler: ConflictHandler<K, V>) -> Self {
        RHTree {
            tree: self.tree,
            conflict_handler: Some(conflict_handler),
        }
    }

    pub fn get<'a>(&'a self, key: &'a K) -> Option<&'a V> {
        self.tree.get(key)
    }

    pub fn position(&self, key: &K) -> Option<usize> {
        self.tree.position(key)
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.tree.insert(key, value)
    }

    pub fn remove(&mut self, _key: &K) -> Option<V> {
        self.tree.remove(_key)
    }
}

impl<'a, K, V> IntoIterator for &'a RHTree<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.tree.into_iter()
    }
}

impl<K, V> RHTree<K, V> {
    pub fn iter(&self) -> Iter<K, V> {
        self.into_iter()
    }
}

impl<K: Hash + Ord, V: Hash> Default for RHTree<K, V> {
    fn default() -> Self {
        RHTree {
            tree: HTree::default(),
            conflict_handler: None,
        }
    }
}

impl<K, V> Reconcilable for RHTree<K, V>
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    type Value = V;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> Option<u64> {
        let mut updated = false;
        // here, using `Option::map` is clearer than using `if let Some(…) =` because of the
        // long match expression
        #[allow(clippy::option_map_unit_fn)]
        for (k, v) in updates {
            match self.tree.get(&k) {
                Some(local_v) => {
                    self.conflict_handler
                        .as_ref() // default behavior in case of conflict: no forced insertion
                        .and_then(|ch| ch(&k, local_v, v))
                }
                None => Some(v),
            }
            .map(|v| {
                self.tree.insert(k, v);
                updated = true;
            });
        }
        updated.then(|| self.tree.hash(&..))
    }

    fn send_updates(
        &self,
        diff_ranges: diff::DiffRanges<Self::Key>,
    ) -> Vec<(Self::Key, Self::Value)> {
        let mut ret: Vec<(K, V)> = Vec::new();
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
