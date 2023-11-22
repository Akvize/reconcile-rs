// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Map`] trait and the related implementation for [`HRTree`].

use core::hash::Hash;

use crate::diff::DiffRange;
use crate::hrtree::HRTree;

/// Provides the basic methods of a key-value map.
/// In addition to [`get`](Map::get), [`insert`](Map::insert) and [`remove`](Map::remove),
/// the method [`enumerate_diff_ranges`](Map::enumerate_diff_ranges) allows listing key-value pairs
/// within the given [`DifferenceItem`](Map::DifferenceItem)s (typically, ranges).
pub trait Map {
    type Key;
    type Value;
    type DifferenceItem;

    fn enumerate_diff_ranges(
        &self,
        diff_ranges: Vec<Self::DifferenceItem>,
    ) -> Vec<(Self::Key, Self::Value)>;
    /// Get the value associated with the given key, if it exists.
    fn get<'a>(&'a self, key: &Self::Key) -> Option<&'a Self::Value>;
    /// Insert a value at the given key, return the current value if it exists.
    fn insert(&mut self, key: Self::Key, value: Self::Value) -> Option<Self::Value>;
    /// Remove and return the value at the given key if it exists.
    fn remove(&mut self, key: &Self::Key) -> Option<Self::Value>;
}

pub trait MutMap: Map {
    /// Get a mutable reference to the value associated with the given key, if it exists.
    fn get_mut<F: FnOnce(Option<&mut Self::Value>)>(&mut self, key: &Self::Key, callback: F);
}

impl<K, V> Map for HRTree<K, V>
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

    fn remove(&mut self, key: &Self::Key) -> Option<Self::Value> {
        self.remove(key)
    }
}

impl<K, V> MutMap for HRTree<K, V>
where
    K: Clone + Hash + Ord,
    V: Clone + Hash,
{
    fn get_mut<F: FnOnce(Option<&mut Self::Value>)>(&mut self, key: &Self::Key, callback: F) {
        self.get_mut(key, callback);
    }
}
