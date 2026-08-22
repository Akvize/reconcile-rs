// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::ops::RangeBounds;
use std::time::Duration;

use tracing::warn;

use crate::bounds::{Key, Value};
use crate::clock::{
    wall_clock_now, BoundedInstant, ClockDrift, StampBound, Timestamp, MAX_CLOCK_DRIFT,
};
use crate::entry::Entry;
use crate::replica::version_hash;

use super::ReplicatedMap;

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// How far a **stored** tombstone stamp may lead this node's physical time before the instant
/// derived from it — never the stamp itself — is capped.
///
/// The same budget as the clock's far-future clamp ([`MAX_CLOCK_DRIFT`]), which is not reachable
/// through the [`Clock`](crate::clock::Clock) port; if it ever reaches `Config`, this follows it.
pub(super) const TOMBSTONE_STAMP_DRIFT_BUDGET: ClockDrift = MAX_CLOCK_DRIFT;

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Set the pre-insert hook, invoked before each key/value pair reaches the map. This is a
    /// setter: a second call replaces the first, it does not add to it.
    ///
    /// Also fires once per entry on process restart when persistence is enabled (see
    /// [`with_persistence`](Self::with_persistence)), replaying the full persisted dataset
    /// through the hook — a hook that assumes it only sees genuinely new state must account for
    /// this.
    ///
    /// Hooks run outside the map's write lock, so a hook may call back into an insert method.
    pub fn set_pre_insert<F: Send + Sync + Fn(&K, &Entry<Timestamp, V>) + 'static>(
        &self,
        pre_insert: F,
    ) {
        let tombstones = self.tombstones.clone();
        let wrapped_pre_insert = move |k: &K, v: &Entry<Timestamp, V>| {
            pre_insert(k, v);
            if v.value().is_some() {
                tombstones.remove(k);
            } else {
                // `v.stamp` is peer-controlled on a socket unauthenticated by default, so the
                // instant handed to the wheel is bounded — the stamp itself is LWW data and is
                // never rewritten. Beyond the budget the instant becomes replica-dependent, which
                // costs nothing: expiry timing is already local and GC is gated on causal
                // stability besides.
                let bounded = BoundedInstant::from_stored_stamp(
                    v.stamp.physical(),
                    TOMBSTONE_STAMP_DRIFT_BUDGET,
                );
                match bounded.bound() {
                    StampBound::Verbatim => {}
                    StampBound::Capped => {
                        warn!(
                            key = ?k,
                            stamp_physical_ms = v.stamp.physical().millis(),
                            stamp_node_id = v.stamp.node_id().get(),
                            budget_ms = TOMBSTONE_STAMP_DRIFT_BUDGET.millis(),
                            bounded_instant = %bounded.instant(),
                            "tombstone stamp leads local physical time by more than the drift \
                             budget; bounding its expiry instant to the cap (the stored stamp is \
                             unchanged). A peer is planting far-future stamps."
                        );
                        crate::observability::record_tombstone_stamp_bounded("capped");
                    }
                    StampBound::Unrepresentable => {
                        warn!(
                            key = ?k,
                            stamp_physical_ms = v.stamp.physical().millis(),
                            stamp_node_id = v.stamp.node_id().get(),
                            budget_ms = TOMBSTONE_STAMP_DRIFT_BUDGET.millis(),
                            "tombstone expiry cap is not a representable wall-clock instant; \
                             ageing the tombstone from now instead (the stored stamp is unchanged)"
                        );
                        crate::observability::record_tombstone_stamp_bounded("unrepresentable");
                    }
                }
                tombstones.insert(k.clone(), bounded.instant());
            }
        };
        *self.engine.pre_insert.write() = Box::new(wrapped_pre_insert);
    }

    /// Insert one pair — hook outside the lock, then insert under it — returning the overwritten
    /// value.
    ///
    /// Local-only and off the published API: [`load_bulk`](Self::load_bulk) for no-broadcast
    /// seeding, [`insert`](Self::insert) for a propagating write.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        let ret = self
            .engine
            .just_insert(key, Entry::present(self.engine.clock_now(), value));
        ret.and_then(|t| t.state.into())
    }

    /// Fully-qualified insert: `just_insert` plus an async broadcast.
    ///
    /// # Value-size ceiling
    ///
    /// A single encoded `(key, entry)` must fit `65507 - authentication overhead` bytes: the send
    /// path packs messages into datagrams but never fragments one. Above that the key **never
    /// converges on any peer**, visible only as a `warn!` on the send path. Stay well clear of the
    /// ceiling, and of the MTU.
    ///
    /// # Panics
    ///
    /// The broadcast is dispatched on a detached `tokio::spawn`ed task, which panics with "there
    /// is no reactor running" unless called from inside a Tokio runtime (`#[tokio::main]`,
    /// `#[tokio::test]`, or an explicit `Runtime::block_on`/`Handle::enter`). This holds for every
    /// write method on this type.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// use reconcile::{replicated_map::Config, InMemoryNetwork, ReplicatedMap};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let network = InMemoryNetwork::new();
    /// let transport = Arc::new(network.bind("127.0.0.1:8301".parse().unwrap()));
    /// let store = ReplicatedMap::<String, i32>::new_with_transport(
    ///     Config::default().with_insecure_no_key(),
    ///     transport,
    /// );
    ///
    /// assert_eq!(store.insert("a".to_string(), 1), None); // nothing there before
    /// assert_eq!(store.insert("a".to_string(), 2), Some(1)); // returns the value it replaced
    /// assert_eq!(store.get_cloned(&"a".to_string()), Some(2));
    /// # }
    /// ```
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self
            .engine
            .insert(key, Entry::present(self.engine.clock_now(), value));
        ret.and_then(|t| t.state.into())
    }

    /// Bulk-insert with hooks — every hook outside any lock, then one write lock for all entries.
    ///
    /// Local-only and off the published API; [`load_bulk`](Self::load_bulk) is the public
    /// no-broadcast seeding path.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        self.load_bulk(key_values);
    }

    /// Bulk-insert + async broadcast.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.engine.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Entry::present(self.engine.clock_now(), v.clone()),
                    )
                })
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-insert **locally, without broadcasting** — the one deliberate no-broadcast write on
    /// the public API, for seeding a large dataset without a broadcast storm.
    ///
    /// Entries are stamped and hooked as usual, and propagate on the next anti-entropy round.
    ///
    /// Deliberately **not** subject to [`insert`](Self::insert)'s Tokio-runtime panic: this is the
    /// one write path that never broadcasts.
    pub fn load_bulk(&self, key_values: &[(K, V)]) {
        self.engine.just_insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Entry::present(self.engine.clock_now(), v.clone()),
                    )
                })
                .collect::<Vec<_>>(),
        );
    }

    /// Local-only single removal; off the published API (test/`cfg(reconcile_internal_testing)` only). Use
    /// [`remove`](Self::remove) for a propagating deletion.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_remove(&self, key: &K) -> Option<V> {
        let ret = self
            .engine
            .just_insert(key.clone(), Entry::tombstone(self.engine.clock_now()));
        ret.and_then(|t| t.state.into())
    }

    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// use reconcile::{replicated_map::Config, InMemoryNetwork, ReplicatedMap};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let network = InMemoryNetwork::new();
    /// let transport = Arc::new(network.bind("127.0.0.1:8303".parse().unwrap()));
    /// let store = ReplicatedMap::<String, i32>::new_with_transport(
    ///     Config::default().with_insecure_no_key(),
    ///     transport,
    /// );
    ///
    /// store.insert("a".to_string(), 1);
    /// assert_eq!(store.remove(&"a".to_string()), Some(1)); // returns the removed value
    /// assert_eq!(store.remove(&"a".to_string()), None); // already gone: a tombstone, not live
    /// assert!(store.get(&"a".to_string()).is_none());
    /// # }
    /// ```
    pub fn remove(&self, key: &K) -> Option<V> {
        let ret = self
            .engine
            .insert(key.clone(), Entry::tombstone(self.engine.clock_now()));
        ret.and_then(|t| t.state.into())
    }

    /// Local-only bulk removal; off the published API (test/`cfg(reconcile_internal_testing)` only). Use
    /// [`remove_bulk`](Self::remove_bulk) for propagating deletions.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_remove_bulk(&self, keys: &[K]) {
        self.engine.just_insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), Entry::tombstone(self.engine.clock_now())))
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-remove: a fresh HLC stamp per key, broadcast as tombstones.
    ///
    /// Callers cannot supply the timestamp: a chosen `DateTime` can collide with another
    /// replica's and make the tie-break non-commutative.
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn remove_bulk(&self, keys: &[K]) {
        self.engine.insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), Entry::tombstone(self.engine.clock_now())))
                .collect::<Vec<_>>(),
        );
    }

    /// Collect the live keys currently satisfying `select`, holding the map read lock only for
    /// the scan (dropped before any deletion). Shared by [`clear`](Self::clear),
    /// [`retain`](Self::retain), and [`delete_range`](Self::delete_range).
    fn live_keys_where<P: FnMut(&K, &V) -> bool>(&self, mut select: P) -> Vec<K> {
        let guard = self.engine.map.read();
        guard
            .range(..)
            .filter_map(|(k, entry)| {
                entry
                    .value()
                    .and_then(|value| select(k, value).then(|| k.clone()))
            })
            .collect()
    }

    /// Delete every live entry, as broadcast tombstones (so the deletion reconciles to peers
    /// rather than mutating the map only locally). Tombstoned keys are reclaimed later by
    /// causal-stability GC. A no-op if the store holds no live entry.
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// the store is non-empty; a no-op call never spawns).
    pub fn clear(&self) {
        let keys = self.live_keys_where(|_, _| true);
        if !keys.is_empty() {
            self.remove_bulk(&keys);
        }
    }

    /// Delete every live entry for which `keep` returns `false`, as broadcast tombstones. Keys
    /// where `keep` returns `true` are retained. The predicate runs under the read lock; keep it
    /// cheap and side-effect free.
    ///
    /// # Deadlock
    ///
    /// `keep` runs while the map read lock is held. Calling a write method from `keep`
    /// self-deadlocks — see [`get`](Self::get)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// at least one entry is removed; a no-op call never spawns).
    pub fn retain<P: FnMut(&K, &V) -> bool>(&self, mut keep: P) {
        let keys = self.live_keys_where(|k, v| !keep(k, v));
        if !keys.is_empty() {
            self.remove_bulk(&keys);
        }
    }

    /// Delete every live entry whose key falls in `range`, as broadcast tombstones. Mirrors the
    /// [`fingerprint`](Self::fingerprint) range signature.
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// the range is non-empty; a no-op call never spawns).
    pub fn delete_range<R: RangeBounds<K>>(&self, range: R) {
        let keys: Vec<K> = {
            let guard = self.engine.map.read();
            guard
                .range(range)
                .filter_map(|(k, entry)| entry.value().map(|_| k.clone()))
                .collect()
        };
        if !keys.is_empty() {
            self.remove_bulk(&keys);
        }
    }

    /// Set a specific expiry timeout to handle tombstones.
    /// The default value is 60 seconds.
    pub fn with_tombstone_timeout(mut self, tombstone_timeout: Duration) -> Self {
        self.tombstones = self.tombstones.with_timeout(tombstone_timeout);
        self
    }

    /// (runtime) Retune the tombstone expiry timeout in place, visible to all clones. The runtime
    /// counterpart of the [`with_tombstone_timeout`](Self::with_tombstone_timeout) builder.
    pub fn set_tombstone_timeout(&self, timeout: Duration) {
        self.tombstones.set_timeout(timeout);
    }

    /// Garbage-collect tombstones, **gated on causal stability** (`ARCHITECTURE.md` §5
    /// invariant 6): older than the timeout *and* acknowledged by every replica this node has
    /// communicated with, or decommissioned via [`forget_peer`](Self::forget_peer).
    pub(super) async fn clear_expired_tombstones(&self) {
        loop {
            for key in self.tombstones.expired(wall_clock_now()) {
                // Version token of the tombstone actually stored, matched against peer acks.
                let version = self.engine.map.read().get(&key).map(version_hash);
                let Some(version) = version else {
                    // The key is no longer present (overwritten or already removed): stop
                    // tracking it.
                    self.tombstones.remove(&key);
                    continue;
                };
                if self.engine.is_tombstone_stable(&key, version) {
                    self.tombstones.remove(&key);
                    // Remove from the dated map *and* the value-only projection together.
                    self.engine.gc_remove(&key);
                    self.engine.forget_tombstone(&key);
                }
                // Otherwise keep the tombstone and re-check on a later iteration.
            }
            tokio::time::sleep(TOMBSTONE_CLEARING).await;
        }
    }
}
