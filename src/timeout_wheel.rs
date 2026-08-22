// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Tracks per-value expiry instants and yields expired values.
///
/// Each instant maps to a `HashSet<T>`, so same-millisecond entries do not overwrite each other
/// and per-value `insert`/`remove` stays `O(1)`.
#[derive(Clone, Default)]
pub(crate) struct TimeoutWheel<T: Clone + Hash + std::cmp::Eq> {
    /// Primary ordering structure: `expiry_instant` → `HashSet<value>`.
    wheel: Arc<RwLock<BTreeMap<DateTime<Utc>, HashSet<T>>>>,
    /// Reverse index: value → expiry instant, used by `remove` to locate the wheel bucket.
    map: Arc<RwLock<HashMap<T, DateTime<Utc>>>>,
    /// Shared so the expiry timeout can be retuned at runtime (see [`set_timeout`](Self::set_timeout)).
    timeout: Arc<RwLock<Duration>>,
}

impl<T: Clone + Hash + std::cmp::Eq> TimeoutWheel<T> {
    /// Create an empty wheel with the default 60-second expiry timeout.
    pub fn new() -> Self {
        TimeoutWheel {
            wheel: Arc::new(RwLock::new(BTreeMap::new())),
            map: Arc::new(RwLock::new(HashMap::new())),
            timeout: Arc::new(RwLock::new(DEFAULT_TIMEOUT)),
        }
    }

    /// Builder-style variant of [`set_timeout`](Self::set_timeout).
    pub fn with_timeout(self, timeout: Duration) -> Self {
        *self.timeout.write().unwrap() = timeout;
        self
    }

    /// (runtime) Retune the expiry timeout in place, visible to all clones.
    pub fn set_timeout(&self, timeout: Duration) {
        *self.timeout.write().unwrap() = timeout;
    }

    /// Track `e` as expiring at `instant`, clearing any slot it already held.
    pub fn insert(&self, e: T, instant: DateTime<Utc>) {
        let mut wheel = self.wheel.write().unwrap();
        let mut map = self.map.write().unwrap();

        if let Some(old_instant) = map.get(&e) {
            if let Some(bucket) = wheel.get_mut(old_instant) {
                bucket.remove(&e);
                if bucket.is_empty() {
                    let old_instant = *old_instant;
                    wheel.remove(&old_instant);
                }
            }
        }

        wheel.entry(instant).or_default().insert(e.clone());
        map.insert(e, instant);
    }

    /// Entries whose timeout has elapsed as of `now`, **without removing them** —
    /// causal-stability-gated GC needs to peek candidates it may still have to retain.
    ///
    /// `now` is supplied by the caller rather than read here, so the wheel itself never touches
    /// the wall clock — the caller sources it from the `Clock` adapter (`crate::clock::wall_clock_now`).
    pub fn expired(&self, now: DateTime<Utc>) -> Vec<T> {
        let timeout = *self.timeout.read().unwrap();
        self.wheel
            .read()
            .unwrap()
            .iter()
            .take_while(|(instant, _)| **instant + timeout < now)
            .flat_map(|(_, values)| values.iter().cloned())
            .collect()
    }

    /// The instant `value` is tracked under. Test-only: a tombstone that never expires is
    /// invisible to an "it expired" assertion, so the tests assert on the instant itself.
    #[cfg(test)]
    pub(crate) fn instant_of(&self, value: &T) -> Option<DateTime<Utc>> {
        self.map.read().unwrap().get(value).copied()
    }

    /// Stop tracking `value` and return it, or `None` if it was not tracked.
    pub fn remove(&self, value: &T) -> Option<T> {
        // Acquire `wheel` before `map`, matching the order used by `insert`. A consistent
        // lock acquisition order across all methods that hold both locks is what prevents an
        // ABBA deadlock between a thread in `insert` and a thread in `remove`.
        let mut wheel = self.wheel.write().unwrap();
        let mut map = self.map.write().unwrap();
        let instant = map.remove(value)?;

        if let Some(bucket) = wheel.get_mut(&instant) {
            bucket.remove(value);
            if bucket.is_empty() {
                wheel.remove(&instant);
            }
        }

        Some(value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn instant_ms(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    /// N entries with the identical instant ⇒ `expired()` yields all N once past the instant.
    #[test]
    fn same_instant_all_expire() {
        let wheel: TimeoutWheel<i32> = TimeoutWheel::new().with_timeout(Duration::ZERO);
        let shared = instant_ms(1_000_000);

        wheel.insert(1, shared);
        wheel.insert(2, shared);
        wheel.insert(3, shared);

        let mut got = wheel.expired(Utc::now());
        got.sort();
        assert_eq!(got, vec![1, 2, 3]);
    }

    /// Interleaved insert/remove with colliding instants keeps wheel/map consistent —
    /// no orphan entry survives in either direction.
    #[test]
    fn interleaved_insert_remove_colliding_instants() {
        let wheel: TimeoutWheel<i32> = TimeoutWheel::new().with_timeout(Duration::ZERO);
        let shared = instant_ms(2_000_000);

        wheel.insert(10, shared);
        wheel.insert(20, shared);

        // Remove 10; 20 must survive unharmed.
        assert_eq!(wheel.remove(&10), Some(10));

        let mut got = wheel.expired(Utc::now());
        got.sort();
        assert_eq!(got, vec![20], "20 should still be tracked");

        // Remove 20; wheel must be empty.
        assert_eq!(wheel.remove(&20), Some(20));
        assert!(wheel.expired(Utc::now()).is_empty());

        // Removing an already-gone entry is a no-op.
        assert_eq!(wheel.remove(&10), None);
    }

    /// Re-inserting the same value with a new instant removes the old wheel slot and registers
    /// the new one — the old instant must no longer fire.
    #[test]
    fn reinsert_same_value_new_instant() {
        let wheel: TimeoutWheel<i32> = TimeoutWheel::new().with_timeout(Duration::ZERO);
        let old = instant_ms(100);
        // Far future: year ~8300, will not expire under a Duration::ZERO timeout applied today.
        let new = instant_ms(200_000_000_000_000);

        wheel.insert(42, old);
        // Re-insert with a far-future instant — 42 should no longer be expired.
        wheel.insert(42, new);

        assert!(
            wheel.expired(Utc::now()).is_empty(),
            "old slot must be gone; only new (future) slot exists"
        );

        // The new slot IS tracked: remove returns the value.
        assert_eq!(wheel.remove(&42), Some(42));
        assert_eq!(wheel.remove(&42), None);
    }

    /// Consistency invariant: after any sequence of inserts and removes, the wheel bucket
    /// total element count matches the map length (no orphans in either direction).
    #[test]
    fn wheel_map_cardinality_stays_in_sync() {
        let wheel: TimeoutWheel<i32> = TimeoutWheel::new().with_timeout(Duration::ZERO);
        let t = instant_ms(3_000_000);

        for i in 0..5 {
            wheel.insert(i, t);
        }
        {
            let w = wheel.wheel.read().unwrap();
            let m = wheel.map.read().unwrap();
            let wheel_count: usize = w.values().map(|v| v.len()).sum();
            assert_eq!(wheel_count, m.len(), "after inserts");
            assert_eq!(m.len(), 5);
        }

        wheel.remove(&2);
        wheel.remove(&4);
        {
            let w = wheel.wheel.read().unwrap();
            let m = wheel.map.read().unwrap();
            let wheel_count: usize = w.values().map(|v| v.len()).sum();
            assert_eq!(wheel_count, m.len(), "after removes");
            assert_eq!(m.len(), 3);
        }
    }

    /// Bulk-remove many entries sharing a single instant (the O(n²)-prone path): every value is
    /// removed exactly once, the shared bucket is dropped when emptied, and wheel/map stay in sync.
    #[test]
    fn bulk_remove_same_instant_clears_everything() {
        let wheel: TimeoutWheel<i32> = TimeoutWheel::new().with_timeout(Duration::ZERO);
        let shared = instant_ms(4_000_000);

        let n = 1_000;
        for i in 0..n {
            wheel.insert(i, shared);
        }
        // One bucket holds all n entries.
        assert_eq!(wheel.wheel.read().unwrap().len(), 1);

        for i in 0..n {
            assert_eq!(wheel.remove(&i), Some(i));
        }

        assert!(wheel.expired(Utc::now()).is_empty());
        assert!(
            wheel.wheel.read().unwrap().is_empty(),
            "emptied bucket dropped"
        );
        assert!(wheel.map.read().unwrap().is_empty());
    }
}
