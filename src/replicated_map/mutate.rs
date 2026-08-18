// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::Entry;

use super::ReplicatedMap;

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Mutate the value for `k` in place, then propagate like [`insert`](ReplicatedMap::insert).
    ///
    /// The callback sees `Some(&mut V)` for a live key, `None` for an absent or tombstoned one; a
    /// mutated entry is re-stamped and broadcast. Holds the write lock for the whole
    /// read-modify-write, so it is atomic against the reconciliation loop.
    ///
    /// # Deadlock
    ///
    /// `callback` runs while the map **write** lock is held. Calling any read or write method
    /// (`get`, `insert`, `for_each`, another `get_mut`, …) from `callback` self-deadlocks — see
    /// [`get`](Self::get)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// the callback mutates a live entry).
    pub fn get_mut<F: FnOnce(Option<&mut V>)>(&self, k: &K, callback: F) {
        // Mint the timestamp before taking the map lock, matching the lock order of `insert`
        // (clock, then map → projection).
        let now = self.engine.clock_now();
        let mut updated: Option<Entry<Timestamp, V>> = None;
        let mut guard = self.engine.map.write();
        guard.with_mut(k, |maybe_entry| {
            if let Some(entry) = maybe_entry {
                callback(entry.value_mut());
                entry.stamp = now;
                updated = Some(entry.clone());
            } else {
                callback(None);
            }
        });
        // The mutation bypassed `insert`: refresh the projection (lock order map → projection).
        if let Some(entry) = guard.get(k) {
            let projected = entry.project();
            self.engine.projection.write().insert(k.clone(), projected);
        }
        drop(guard);
        if let Some(value) = updated {
            self.engine.broadcast_update(k.clone(), value);
        }
    }

    /// Mutate `k` in place **only when live**, re-stamping and broadcasting; returns whether it
    /// was. Atomic against the reconciliation loop.
    ///
    /// The shared core of [`update`](Self::update) and [`upsert`](Self::upsert).
    fn mutate_live<F: FnOnce(&mut V)>(&self, k: &K, callback: F) -> bool {
        // Mint the timestamp before taking the map lock, matching the lock order of `insert`.
        let now = self.engine.clock_now();
        let mut updated: Option<Entry<Timestamp, V>> = None;
        let mut guard = self.engine.map.write();
        guard.with_mut(k, |maybe_entry| {
            if let Some(entry) = maybe_entry {
                if let Some(value) = entry.value_mut() {
                    callback(value);
                    entry.stamp = now;
                    updated = Some(entry.clone());
                }
            }
        });
        if updated.is_some() {
            if let Some(entry) = guard.get(k) {
                let projected = entry.project();
                self.engine.projection.write().insert(k.clone(), projected);
            }
        }
        drop(guard);
        if let Some(value) = updated {
            self.engine.broadcast_update(k.clone(), value);
            true
        } else {
            false
        }
    }

    /// Atomically mutate the live value for `k`, then re-stamp and broadcast; returns whether the
    /// key was live. The race-free replacement for a `get`-then-`insert`.
    ///
    /// # Deadlock
    ///
    /// `f` runs while the map write lock is held — same hazard as
    /// [`get_mut`](Self::get_mut)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// `k` is live).
    #[must_use]
    pub fn update<F: FnOnce(&mut V)>(&self, k: &K, f: F) -> bool {
        self.mutate_live(k, f)
    }

    /// Update the live value for `k` with `f`, or insert `default` if it is absent or tombstoned.
    ///
    /// The update branch is atomic; the insert branch behaves like [`insert`](Self::insert).
    ///
    /// # Deadlock
    ///
    /// `f` runs while the map write lock is held on the update branch — same hazard as
    /// [`get_mut`](Self::get_mut)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn upsert<F: FnOnce(&mut V)>(&self, k: K, default: V, f: F) {
        if !self.mutate_live(&k, f) {
            self.insert(k, default);
        }
    }

    /// Return the live value for `k`, inserting (and broadcasting) `f()` first if it is
    /// absent/tombstoned. Under last-write-wins, two nodes racing to insert converge by timestamp
    /// order; this node returns the value it observed/created.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// `k` is absent/tombstoned).
    pub fn get_or_insert_with<F: FnOnce() -> V>(&self, k: &K, f: F) -> V {
        if let Some(value) = self.get(k) {
            return value.clone();
        }
        let value = f();
        self.insert(k.clone(), value.clone());
        value
    }
}
