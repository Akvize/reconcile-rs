// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ops::RangeBounds;

use parking_lot::RwLockReadGuard;

use crate::bounds::{Key, Value};
use crate::value_ref::ValueRef;
use rsos::Fingerprint;

use super::ReadReplicaMap;

impl<K: Key, V: Value> ReadReplicaMap<K, V> {
    /// Get the live value for a key, or `None` if the key is absent or holds a replicated tombstone.
    pub fn get(&self, k: &K) -> Option<ValueRef<'_, V>> {
        let guard = self.tree.read();
        RwLockReadGuard::try_map(guard, |tree| tree.get(k).and_then(|state| state.as_value()))
            .ok()
            .map(ValueRef)
    }

    /// Clone of the live value for `k`, or `None`. Unlike [`get`](Self::get), the read lock is
    /// released before this returns — the default read when the value will be compared against or
    /// fed into a subsequent write, mirroring [`ReplicatedMap::get_cloned`](crate::ReplicatedMap::get_cloned).
    pub fn get_cloned(&self, k: &K) -> Option<V> {
        self.get(k).map(|v| v.clone())
    }

    /// Whether the read replica currently holds a live value for the key (a tombstone counts as
    /// absent).
    pub fn contains_key(&self, k: &K) -> bool {
        self.tree
            .read()
            .get(k)
            .is_some_and(|state| !state.is_tombstone())
    }

    /// The number of **live** entries currently held (a replicated tombstone counts as absent, not
    /// present). Mirrors [`ReplicatedMap::len`](crate::ReplicatedMap::len): `O(n)`, it scans the
    /// tree filtering out tombstones.
    pub fn len(&self) -> usize {
        self.tree
            .read()
            .iter()
            .filter(|(_, state)| !state.is_tombstone())
            .count()
    }

    /// Whether the read replica holds no live entry (a tree holding only tombstones is empty).
    /// `O(n)` worst case, but returns as soon as it finds a live value.
    pub fn is_empty(&self) -> bool {
        !self
            .tree
            .read()
            .iter()
            .any(|(_, state)| !state.is_tombstone())
    }

    /// Value-only fingerprint over a range. After convergence this equals the dated peer's
    /// [`value_fingerprint`](crate::ReplicatedMap::value_fingerprint) over the same range.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.tree.read().aggregate(range).fingerprint()
    }

    /// Deprecated alias for [`value_fingerprint`](Self::value_fingerprint) — the name collided
    /// with [`ReplicatedMap::fingerprint`](crate::ReplicatedMap::fingerprint), which includes the
    /// timestamp and so never equals this one between converged peers (#294).
    #[deprecated(since = "1.0.0", note = "renamed to `value_fingerprint`")]
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.value_fingerprint(range)
    }

    /// The smallest live key and its value, or `None` if the read replica holds no live entry.
    /// Same complexity as [`ReplicatedMap::first_key_value`](crate::ReplicatedMap::first_key_value).
    pub fn first_key_value(&self) -> Option<(K, V)> {
        let guard = self.tree.read();
        guard
            .iter()
            .find(|(_, state)| !state.is_tombstone())
            .map(|(k, state)| (k.clone(), state.as_value().expect("checked above").clone()))
    }

    /// The largest live key and its value, or `None` if the read replica holds no live entry. Same
    /// complexity as [`ReplicatedMap::last_key_value`](crate::ReplicatedMap::last_key_value).
    pub fn last_key_value(&self) -> Option<(K, V)> {
        let guard = self.tree.read();
        let mut index = guard.len();
        while index > 0 {
            index -= 1;
            let key = guard.select(index).clone();
            if let Some(value) = guard.get(&key).and_then(|state| state.as_value()) {
                return Some((key, value.clone()));
            }
        }
        None
    }

    /// Call `f` for every live entry, in key order, under the tree read lock. Do not block or call
    /// back into the replica from `f`.
    pub fn for_each<F: FnMut(&K, &V)>(&self, mut f: F) {
        let guard = self.tree.read();
        for (k, state) in guard.iter() {
            if let Some(value) = state.as_value() {
                f(k, value);
            }
        }
    }

    /// Call `f` for every live entry whose key falls in `range`, in key order. Mirrors the
    /// [`value_fingerprint`](Self::value_fingerprint) range signature; same locking discipline as
    /// [`for_each`](Self::for_each).
    pub fn for_each_in_range<R: RangeBounds<K>, F: FnMut(&K, &V)>(&self, range: R, mut f: F) {
        let guard = self.tree.read();
        for (k, state) in guard.range(range) {
            if let Some(value) = state.as_value() {
                f(k, value);
            }
        }
    }

    /// Snapshot all live entries into an owned `Vec`, in key order. Clones under the read lock;
    /// prefer [`for_each`](Self::for_each) to avoid the copy for large scans.
    pub fn to_vec(&self) -> Vec<(K, V)> {
        let guard = self.tree.read();
        guard
            .iter()
            .filter_map(|(k, state)| state.as_value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// Snapshot the live entries whose keys fall in `range` into an owned `Vec`, in key order.
    pub fn range_to_vec<R: RangeBounds<K>>(&self, range: R) -> Vec<(K, V)> {
        let guard = self.tree.read();
        guard
            .range(range)
            .filter_map(|(k, state)| state.as_value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// The keys of all live entries, in key order. Thin owned convenience over [`to_vec`](Self::to_vec).
    pub fn keys(&self) -> Vec<K> {
        let guard = self.tree.read();
        guard
            .iter()
            .filter_map(|(k, state)| state.as_value().map(|_| k.clone()))
            .collect()
    }

    /// The values of all live entries, in key order.
    pub fn values(&self) -> Vec<V> {
        let guard = self.tree.read();
        guard
            .iter()
            .filter_map(|(_, state)| state.as_value().cloned())
            .collect()
    }
}
