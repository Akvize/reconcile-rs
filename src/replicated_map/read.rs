// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::ops::RangeBounds;

use parking_lot::{MappedRwLockReadGuard, RwLockReadGuard};

use crate::bounds::{Key, Value};
use rsos::Fingerprint;

use super::ReplicatedMap;

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Fingerprint of the live entries (value **and** timestamp) over `range`: `O(range size)`,
    /// used as the anti-entropy comparison value — equal fingerprints on both peers mean equal
    /// content over the range. See [`value_fingerprint`](Self::value_fingerprint) for the
    /// timestamp-less counterpart.
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.fingerprint(range)
    }

    /// Fingerprint of the **value-only projection** over a range: the timestamp-less counterpart
    /// of [`fingerprint`](Self::fingerprint), which a converged
    /// [`ReadReplicaMap`](crate::read_replica_map::ReadReplicaMap) reproduces.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.value_fingerprint(range)
    }

    /// # Deadlock
    ///
    /// The returned guard holds the map **read** lock for as long as it is alive. Calling any
    /// write method (`insert`, `remove`, `get_mut`, `update`, …) — which takes the **write** lock
    /// — while the guard from an earlier `get` on the same thread is still in scope self-deadlocks
    /// (`parking_lot`'s `RwLock` is not reentrant, and blocks with no timeout rather than
    /// panicking):
    ///
    /// ```ignore
    /// if let Some(v) = map.get(&k) {
    ///     map.insert(k, new_value); // deadlocks: `v` is still borrowing the read lock
    /// }
    /// ```
    ///
    /// Prefer [`get_cloned`](Self::get_cloned), which drops the lock before returning, as the
    /// default read when the value will be compared against or fed into a subsequent write.
    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.engine.map.read();
        RwLockReadGuard::try_map(guard, |map| map.get(k).and_then(|entry| entry.value())).ok()
    }

    /// Clone of the live value for `k`, or `None`. Unlike [`get`](Self::get), the read lock is
    /// released before this returns, so the result can be safely followed by a write on the same
    /// thread — this is the documented default read for that pattern. Still racy against a
    /// concurrent write between the read and the write; use [`update`](Self::update) instead when
    /// the write must be atomic with the read.
    pub fn get_cloned(&self, k: &K) -> Option<V> {
        self.get(k).map(|v| v.clone())
    }

    /// The number of **live** entries. `O(n)`, and smaller than the raw map size: tombstones
    /// linger until causal-stability-gated GC reclaims them.
    pub fn len(&self) -> usize {
        self.engine
            .map
            .read()
            .iter()
            .filter(|(_, entry)| !entry.is_tombstone())
            .count()
    }

    /// Whether the store holds no live entry. `O(n)` worst case, but returns as soon as it finds a
    /// live value. A store that holds only tombstones is empty.
    pub fn is_empty(&self) -> bool {
        !self
            .engine
            .map
            .read()
            .iter()
            .any(|(_, entry)| !entry.is_tombstone())
    }

    /// Whether `k` maps to a live value (a tombstoned key reads as absent).
    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    /// The smallest live key and its value, or `None` if the store holds no live entry. `O(log n)`,
    /// worse if the smallest raw key is tombstoned (`O(n)` if every entry is).
    pub fn first_key_value(&self) -> Option<(K, V)> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .find(|(_, entry)| !entry.is_tombstone())
            .map(|(k, entry)| (k.clone(), entry.value().expect("checked above").clone()))
    }

    /// The largest live key and its value, or `None` if the store holds no live entry. Same
    /// complexity as [`first_key_value`](Self::first_key_value).
    pub fn last_key_value(&self) -> Option<(K, V)> {
        let guard = self.engine.map.read();
        let mut index = guard.len();
        while index > 0 {
            index -= 1;
            let key = guard.select(index).clone();
            if let Some(value) = guard.get(&key).and_then(|entry| entry.value()) {
                return Some((key, value.clone()));
            }
        }
        None
    }

    /// Call `f` for every live entry, in key order, under the map read lock. Do not block or call
    /// back into the store from `f`.
    ///
    /// # Deadlock
    ///
    /// `f` runs while the map read lock is held. Calling a write method (`insert`, `get_mut`,
    /// …) from `f` self-deadlocks — see [`get`](Self::get)'s `# Deadlock` section.
    pub fn for_each<F: FnMut(&K, &V)>(&self, mut f: F) {
        let guard = self.engine.map.read();
        for (k, entry) in guard.iter() {
            if let Some(value) = entry.value() {
                f(k, value);
            }
        }
    }

    /// Call `f` for every live entry whose key falls in `range`, in key order. Mirrors the
    /// [`fingerprint`](Self::fingerprint) range signature; same locking discipline as
    /// [`for_each`](Self::for_each).
    ///
    /// # Deadlock
    ///
    /// Same hazard as [`for_each`](Self::for_each) — see [`get`](Self::get)'s `# Deadlock`
    /// section.
    pub fn for_each_in_range<R: RangeBounds<K>, F: FnMut(&K, &V)>(&self, range: R, mut f: F) {
        let guard = self.engine.map.read();
        for (k, entry) in guard.range(range) {
            if let Some(value) = entry.value() {
                f(k, value);
            }
        }
    }

    /// Snapshot all live entries into an owned `Vec`, in key order. Clones under the read lock;
    /// prefer [`for_each`](Self::for_each) to avoid the copy for large scans.
    pub fn to_vec(&self) -> Vec<(K, V)> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .filter_map(|(k, entry)| entry.value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// Snapshot the live entries whose keys fall in `range` into an owned `Vec`, in key order.
    pub fn range_to_vec<R: RangeBounds<K>>(&self, range: R) -> Vec<(K, V)> {
        let guard = self.engine.map.read();
        guard
            .range(range)
            .filter_map(|(k, entry)| entry.value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// The keys of all live entries, in key order. Thin owned convenience over [`to_vec`](Self::to_vec).
    pub fn keys(&self) -> Vec<K> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .filter_map(|(k, entry)| entry.value().map(|_| k.clone()))
            .collect()
    }

    /// The values of all live entries, in key order.
    pub fn values(&self) -> Vec<V> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .filter_map(|(_, entry)| entry.value().cloned())
            .collect()
    }
}
