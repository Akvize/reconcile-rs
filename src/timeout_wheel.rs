use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A wheel that tracks per-value expiry instants and yields expired values.
///
/// # Collision safety
///
/// The wheel maps each `DateTime<Utc>` expiry instant to a **`HashSet<T>`** rather than a single
/// `T`. Multiple entries sharing the same millisecond instant (e.g. two keys passed to `remove_bulk`
/// in the same wall-clock millisecond) each occupy their own slot inside the set, so neither
/// silently overwrites the other.
///
/// Using a `HashSet` (rather than a `Vec`) for the bucket keeps per-value `insert`/`remove` O(1):
/// removing one of `n` entries sharing an instant no longer scans the whole bucket, so a
/// `remove_bulk` of `n` same-instant keys is O(n) overall instead of O(n²).
#[derive(Clone, Default)]
pub(crate) struct TimeoutWheel<T: Clone + Hash + std::cmp::Eq> {
    /// Primary ordering structure: `expiry_instant` → `HashSet<value>`.
    ///
    /// A set per instant means that two entries sharing the same timestamp are stored as
    /// separate elements in the same bucket; `remove_bulk` of ≥2 keys in the same millisecond
    /// no longer silently drops one of them, and locating a single value in the bucket is O(1).
    wheel: Arc<RwLock<BTreeMap<DateTime<Utc>, HashSet<T>>>>,
    /// Reverse index: value → expiry instant, used by `remove` to locate the wheel bucket.
    map: Arc<RwLock<HashMap<T, DateTime<Utc>>>>,
    /// Shared so the expiry timeout can be retuned at runtime (see [`set_timeout`](Self::set_timeout)).
    timeout: Arc<RwLock<Duration>>,
}

impl<T: Clone + Hash + std::cmp::Eq> TimeoutWheel<T> {
    pub fn new() -> Self {
        TimeoutWheel {
            wheel: Arc::new(RwLock::new(BTreeMap::new())),
            map: Arc::new(RwLock::new(HashMap::new())),
            timeout: Arc::new(RwLock::new(DEFAULT_TIMEOUT)),
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        *self.timeout.write().unwrap() = timeout;
        self
    }

    /// (runtime) Retune the expiry timeout in place, visible to all clones.
    pub fn set_timeout(&self, timeout: Duration) {
        *self.timeout.write().unwrap() = timeout;
    }

    /// Track `e` as expiring at `instant`.
    ///
    /// If `e` was already tracked under a different instant, the old wheel slot is removed first
    /// so no orphan entry is left behind.
    pub fn insert(&self, e: T, instant: DateTime<Utc>) {
        let mut wheel = self.wheel.write().unwrap();
        let mut map = self.map.write().unwrap();

        // If the value is already tracked, remove it from its current wheel bucket before
        // inserting it at the new instant. This keeps wheel and map consistent when the same
        // value is re-inserted with a new instant (e.g. a tombstone whose HLC timestamp was
        // bumped by a re-remove).
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

    /// Return all entries whose timeout has elapsed, **without removing them**.
    ///
    /// Used by causal-stability-gated tombstone GC: a tombstone may be old enough to
    /// expire by wall-clock age but must be retained until every replica has acknowledged
    /// it, so the caller needs to peek expired candidates without dropping the tracking.
    pub fn expired(&self) -> Vec<T> {
        let now = Utc::now();
        let timeout = *self.timeout.read().unwrap();
        self.wheel
            .read()
            .unwrap()
            .iter()
            .take_while(|(instant, _)| **instant + timeout < now)
            .flat_map(|(_, values)| values.iter().cloned())
            .collect()
    }

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

        let mut got = wheel.expired();
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

        let mut got = wheel.expired();
        got.sort();
        assert_eq!(got, vec![20], "20 should still be tracked");

        // Remove 20; wheel must be empty.
        assert_eq!(wheel.remove(&20), Some(20));
        assert!(wheel.expired().is_empty());

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
            wheel.expired().is_empty(),
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

        assert!(wheel.expired().is_empty());
        assert!(
            wheel.wheel.read().unwrap().is_empty(),
            "emptied bucket dropped"
        );
        assert!(wheel.map.read().unwrap().is_empty());
    }
}
