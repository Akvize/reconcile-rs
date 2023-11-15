use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

use chrono::{DateTime, Utc};

#[derive(Default)]
pub(crate) struct TimeoutWheel<T: Clone + Hash + std::cmp::Eq> {
    wheel: BTreeMap<DateTime<Utc>, T>,
    map: HashMap<T, DateTime<Utc>>,
}

impl<T: Clone + Hash + std::cmp::Eq> TimeoutWheel<T> {
    pub fn new() -> Self {
        TimeoutWheel {
            wheel: BTreeMap::new(),
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, e: T, timeout: DateTime<Utc>) {
        self.wheel.insert(timeout, e.clone());
        self.map.insert(e, timeout);
    }

    pub fn pop_expired(&mut self) -> Option<T> {
        self.wheel.first_entry().and_then(|entry| {
            if entry.key() < &Utc::now() {
                let value = entry.remove();
                self.map.remove(&value);
                return Some(value);
            }
            None
        })
    }

    pub fn remove(&mut self, value: &T) -> Option<T> {
        self.map
            .get(value)
            .and_then(|instant| self.wheel.remove(instant))
    }
}
