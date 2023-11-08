use core::hash::Hash;

use crate::diff::DiffRange;
use crate::htree::HTree;

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
