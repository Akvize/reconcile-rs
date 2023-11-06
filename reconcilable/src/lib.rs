use core::hash::Hash;

use chrono::{DateTime, Utc};

use diff::DiffRanges;
use htree::HTree;

pub trait Reconcilable {
    type Key;
    type Value;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>);
    fn enumerate_diff_ranges(
        &self,
        diff_ranges: DiffRanges<Self::Key>,
    ) -> Vec<(Self::Key, Self::Value)>;
}

type TV<V> = (DateTime<Utc>, V);

impl<K, V> Reconcilable for HTree<K, TV<V>>
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    type Key = K;
    type Value = TV<V>;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) {
        for (k, tv) in updates {
            if let Some(local_tv) = self.get(&k) {
                if tv.0 > local_tv.0 {
                    self.insert(k, tv);
                }
            } else {
                self.insert(k, tv);
            }
        }
    }

    fn enumerate_diff_ranges(
        &self,
        diff_ranges: diff::DiffRanges<Self::Key>,
    ) -> Vec<(Self::Key, Self::Value)> {
        let mut ret = Vec::new();
        for diff in diff_ranges {
            for (k, v) in self.get_range(&diff) {
                ret.push((k.clone(), v.clone()));
            }
        }
        ret
    }
}
