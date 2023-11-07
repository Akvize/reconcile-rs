use core::hash::Hash;

use chrono::{DateTime, Utc};

use diff::DiffRange;
use htree::HTree;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReconciliationResult {
    KeepSelf,
    KeepOther,
}

pub trait Reconcilable {
    fn reconcile(&self, other: &Self) -> ReconciliationResult;
}

impl<V> Reconcilable for (DateTime<Utc>, V) {
    fn reconcile(&self, other: &Self) -> ReconciliationResult {
        if other.0 > self.0 {
            ReconciliationResult::KeepOther
        } else {
            ReconciliationResult::KeepSelf
        }
    }
}

pub trait Map {
    type Key;
    type Value;
    type DifferenceItem;

    fn enumerate_diff_ranges(
        &self,
        diff_ranges: Vec<Self::DifferenceItem>,
    ) -> Vec<(Self::Key, Self::Value)>;
    fn get<'a>(&'a self, key: &Self::Key) -> Option<&'a Self::Value>;
    fn insert(&mut self, key: Self::Key, value: Self::Value) -> Option<Self::Value>;
}

impl<K, V> Map for HTree<K, V>
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    type Key = K;
    type Value = V;
    type DifferenceItem = DiffRange<K>;

    fn enumerate_diff_ranges(
        &self,
        diff_ranges: Vec<Self::DifferenceItem>,
    ) -> Vec<(Self::Key, Self::Value)> {
        let mut ret = Vec::new();
        for diff in diff_ranges {
            for (k, v) in self.get_range(&diff) {
                ret.push((k.clone(), v.clone()));
            }
        }
        ret
    }

    fn get<'a>(&'a self, key: &Self::Key) -> Option<&'a Self::Value> {
        self.get(key)
    }

    fn insert(&mut self, key: Self::Key, value: Self::Value) -> Option<Self::Value> {
        self.insert(key, value)
    }
}
