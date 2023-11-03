use core::hash::Hash;

use chrono::{DateTime, Utc};

use diff::{DiffRanges, HashRangeQueryable};
use htree::HTree;

pub trait Reconcilable {
    type Key;
    type Value;

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> Option<u64>;
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

    fn reconcile(&mut self, updates: Vec<(Self::Key, Self::Value)>) -> Option<u64> {
        let mut updated = false;
        // here, using `Option::map` is clearer than using `if let Some(…) =` because of the
        // long match expression
        #[allow(clippy::option_map_unit_fn)]
        for (k, tv) in updates {
            match self.get(&k) {
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
                self.insert(k, v);
                updated = true;
            });
        }
        updated.then(|| self.hash(&..))
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
