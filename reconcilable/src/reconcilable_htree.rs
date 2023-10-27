use core::hash::Hash;

use diff::HashRangeQueryable;
use htree::HTree;

use crate::Reconcilable;

pub struct ReconcilableHTree<K, V> {
    tree: HTree<K, V>,
    conflict_handler: Option<fn(&K, &V, V) -> Option<V>>,
}

impl<K: Hash + Ord, V: Hash> ReconcilableHTree<K, V> {
    pub fn new(tree: HTree<K, V>) -> Self {
        ReconcilableHTree {
            tree: tree,
            conflict_handler: None,
        }
    }

    pub fn with_conflict_handler(self, conflict_handler: fn(&K, &V, V) -> Option<V>) -> Self {
        ReconcilableHTree {
            tree: self.tree,
            conflict_handler: Some(conflict_handler),
        }
    }
}

impl<K, V> Reconcilable for ReconcilableHTree<K, V>
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    type Value = V;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> u64 {
        for (k, v) in updates {
            match self.tree.get(&k) {
                Some(local_v) => {
                    self.conflict_handler
                        .as_ref() // default behavior in case of conflict: no forced insertion
                        .map(|ch| ch(&k, local_v, v))
                        .flatten()
                        .map(|v| self.tree.insert(k, v));
                }
                None => {
                    self.tree.insert(k, v);
                }
            }
        }
        self.tree.hash(&..)
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

impl<K: Hash + Ord, V: Hash> HashRangeQueryable
    for ReconcilableHTree<K, V>
{
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
