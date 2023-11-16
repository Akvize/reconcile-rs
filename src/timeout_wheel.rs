use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct TimeoutWheel<T: Clone + Hash + std::cmp::Eq> {
    wheel: Arc<RwLock<BTreeMap<DateTime<Utc>, T>>>,
    map: Arc<RwLock<HashMap<T, DateTime<Utc>>>>,
    timeout: Duration,
}

impl<T: Clone + Hash + std::cmp::Eq> Clone for TimeoutWheel<T> {
    fn clone(&self) -> Self {
        TimeoutWheel {
            wheel: self.wheel.clone(),
            map: self.map.clone(),
            timeout: self.timeout,
        }
    }
}

impl<T: Clone + Hash + std::cmp::Eq> TimeoutWheel<T> {
    pub fn new() -> Self {
        TimeoutWheel {
            wheel: Arc::new(RwLock::new(BTreeMap::new())),
            map: Arc::new(RwLock::new(HashMap::new())),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn insert(&self, e: T, instant: DateTime<Utc>) {
        self.wheel.write().unwrap().insert(instant, e.clone());
        self.map.write().unwrap().insert(e, instant);
    }

    pub fn pop_expired(&self) -> Option<T> {
        self.wheel
            .write()
            .unwrap()
            .first_entry()
            .filter(|entry| *entry.key() + self.timeout < Utc::now())
            .map(|entry| {
                let value = entry.remove();
                self.map.write().unwrap().remove(&value);
                value
            })
    }

    pub fn remove(&self, value: &T) -> Option<T> {
        self.map
            .write()
            .unwrap()
            .remove(value)
            .and_then(|instant| self.wheel.write().unwrap().remove(&instant))
    }
}
