use core::hash::Hash;
use crate::{htree::HTree, reconcilable::Reconcilable};

pub struct ReconcilableHTree<K, V, C: Fn(&K, &V, V) -> Option<V>> {
    tree: HTree<K, V>,
    conflict_handler: Option<C>,
}

impl<K: Hash + Ord, V: Hash, C: Fn(&K, &V, V) -> Option<V>> ReconcilableHTree<K, V, C> {
    pub fn new() -> Self {
        ReconcilableHTree { tree: HTree::default(), conflict_handler: None }
    }

    pub fn with_conflict_handler(self, conflict_handler: Option<C>) -> Self {
        ReconcilableHTree { tree: self.tree, conflict_handler }
    }
}

impl<K, V, C> Reconcilable for ReconcilableHTree<K, V, C>
 where K: Clone + Hash + Ord, V: Clone + Hash, C: Fn(&K, &V, V) -> Option<V> {
    type Key = K;
    type Value = V;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) {
        for (k, v) in updates {
            match self.tree.get(&k) { 
                Some(local_v) => {
                    self.conflict_handler.as_ref() // default behavior in case of conflict: no forced insertion
                        .map(|ch| ch(&k, local_v, v))
                        .flatten()
                        .map(|v| self.tree.insert(k, v));
                },
                None => { self.tree.insert(k, v); },
            }
        }
    }

    fn send_updates(&self, diffs: crate::diff::Diffs<Self::Key>) -> Vec<(Self::Key, Self::Value)> {
        let mut ret: Vec<(K, V)> = Vec::new();
        for diff in diffs {
            for (k, v) in self.tree.get_range(&diff) {
                ret.push((k.clone(), v.clone()));
            }
        }
        ret
    }
}