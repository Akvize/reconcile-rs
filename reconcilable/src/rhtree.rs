use core::hash::Hash;

use diff::HashRangeQueryable;
use htree::HTree;

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

impl<K: Hash + Ord, V: Hash> Default for RHTree<K, V> {
    fn default() -> Self {
        RHTree { tree: HTree::default(), conflict_handler: None }
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
        for (k, v) in updates {
            if let Some(v) = match self.tree.get(&k) {
                Some(local_v) => {
                    self.conflict_handler
                        .as_ref() // default behavior in case of conflict: no forced insertion
                        .and_then(|ch| ch(&k, local_v, v))
                }
                None => Some(v),
            } {
                self.tree.insert(k, v);
                updated = true;
            }
        }
        updated.then(|| self.tree.hash(&..))
    }

    fn send_updates(&self, diffs: diff::Diffs<Self::Key>) -> Vec<(Self::Key, Self::Value)> {
        let mut ret: Vec<(K, V)> = Vec::new();
        for diff in diffs {
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
