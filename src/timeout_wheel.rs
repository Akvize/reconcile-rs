use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
pub(crate) struct TimeoutWheel<T: Clone + Hash + std::cmp::Eq> {
    wheel: Arc<RwLock<BTreeMap<DateTime<Utc>, T>>>,
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

    pub fn insert(&self, e: T, instant: DateTime<Utc>) {
        self.wheel.write().unwrap().insert(instant, e.clone());
        self.map.write().unwrap().insert(e, instant);
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
            .map(|(_, value)| value.clone())
            .collect()
    }

    pub fn remove(&self, value: &T) -> Option<T> {
        self.map
            .write()
            .unwrap()
            .remove(value)
            .and_then(|instant| self.wheel.write().unwrap().remove(&instant))
    }
}
