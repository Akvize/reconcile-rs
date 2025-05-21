// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Service`], a wrapper to a key-value map
//! to enable reconciliation between different instances over a network.

use std::fmt::Debug;
use std::hash::Hash;
use std::net::IpAddr;
use std::ops::RangeBounds;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ipnet::IpNet;
use parking_lot::{MappedRwLockReadGuard, RwLockReadGuard};
use serde::{de::DeserializeOwned, Serialize};

use crate::internal_service::InternalService;
use crate::timeout_wheel::TimeoutWheel;

pub type MaybeTombstone<V> = Option<V>;
pub type DatedMaybeTombstone<V> = (DateTime<Utc>, MaybeTombstone<V>);

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// Core service wrapping a key-value map to enable reconciliation between different instances over a network.
///
/// The service also keeps track of the addresses of other instances.
///
/// Provides wrappers for its underlying [`HRTree`](crate::HRTree)'s insertion and deletion methods,
/// as well as its main service method: `run()`,
/// which must be called to actually synchronize with peers.
///
/// Known peers can optionally be provided using the [`with_seed`](Service::with_seed) method. In
/// any case, the service will periodically look for new peers by sampling a random address from
/// the given peer network.
pub struct Service<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    /// Internal map and hooks container.
    service: InternalService<K, (DateTime<Utc>, Option<V>)>,
    /// Tombstone timestamps for deleted entries.
    tombstones: TimeoutWheel<K>,
}

impl<K, V> Clone for Service<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    /// Allows cloning of the `Service` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        Service {
            service: self.service.clone(),
            tombstones: self.tombstones.clone(),
        }
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Send + Serialize + Sync + 'static,
    > Service<K, V>
{
    /// Create a new `Service`, set up network and tombstones.
    pub async fn new(config: ServiceConfig) -> Self {
        let svc = Service {
            service: InternalService::<K, (DateTime<Utc>, Option<V>)>::new(config).await,
            tombstones: TimeoutWheel::new(),
        };
        svc.add_pre_insert(|_, _| {});
        svc
    }

    /// Provides the address of a known peer to the service
    ///
    /// This is optional, but reduces the time to connect to existing peers
    pub fn with_seed(self, peer: IpAddr) -> Self {
        let now = Instant::now();
        self.service.peers.write().insert(peer, now);
        self
    }

    /// Set a specific expiry timeout to handle tombstones.
    /// The default value is 60 seconds.
    pub fn with_tombstone_timeout(mut self, tombstone_timeout: Duration) -> Self {
        self.tombstones = self.tombstones.with_timeout(tombstone_timeout);
        self
    }

    /// Register a pre-insert hook.
    ///
    /// The hook is invoked **before** inserting each key/value pair into the internal map.
    /// Calling this does **not** consume the `Service` instance; you can call it multiple times.
    ///
    /// # Deadlock Safety
    ///
    /// Hooks are executed outside of the map’s write lock, so calling back into any insert
    /// method from within a hook will not block or deadlock.
    pub fn add_pre_insert<F: Send + Sync + Fn(&K, &(DateTime<Utc>, Option<V>)) + 'static>(
        &self,
        pre_insert: F,
    ) {
        let tombstones = self.tombstones.clone();
        let wrapped_pre_insert = move |k: &K, v: &(DateTime<Utc>, Option<V>)| {
            pre_insert(k, v);
            if v.1.is_some() {
                tombstones.remove(k);
            } else {
                tombstones.insert(k.clone(), v.0);
            }
        };
        // Swap in the new hook
        *self.service.pre_insert.write() = Box::new(wrapped_pre_insert);
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> u64 {
        self.service.fingerprint(range)
    }

    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.service.map.read();
        RwLockReadGuard::try_map(guard, |map| {
            map.get(k).and_then(|(_, ref opt)| opt.as_ref())
        })
        .ok()
    }

    /// Insert a single key/value pair, running the pre-insert hook first.
    ///
    /// # Behavior
    ///
    /// 1. Calls the registered `pre_insert` hook outside of any locks.
    /// 2. Acquires the write lock on the map, performs the insertion, then drops the lock.
    ///
    /// Returns the overwritten value if the key already existed.
    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.service.just_insert(key, (Utc::now(), Some(value)));
        ret.and_then(|t| t.1)
    }

    /// Fully-qualified insert: just_insert + async broadcast.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.service.insert(key, (Utc::now(), Some(value)));
        ret.and_then(|t| t.1)
    }

    /// Bulk-insert multiple key/value pairs with hook invocation.
    ///
    /// # Behavior
    ///
    /// 1. Runs the pre-insert hook for each entry (outside any lock).
    /// 2. Acquires the write lock once and inserts all entries.
    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        self.service.just_insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| (k.clone(), (Utc::now(), Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-insert + async broadcast.
    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.service.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| (k.clone(), (Utc::now(), Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn just_remove(&self, key: &K) -> Option<V> {
        let ret = self.service.just_insert(key.clone(), (Utc::now(), None));
        ret.and_then(|t| t.1)
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let ret = self.service.insert(key.clone(), (Utc::now(), None));
        ret.and_then(|t| t.1)
    }

    pub fn just_remove_bulk(&self, keys: &[K]) {
        self.service.just_insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), (Utc::now(), None)))
                .collect::<Vec<_>>(),
        );
    }

    pub fn remove_bulk(&self, keys: &[(K, DateTime<Utc>)]) {
        self.service.insert_bulk(
            &keys
                .iter()
                .map(|(k, t)| (k.clone(), (*t, None)))
                .collect::<Vec<_>>(),
        );
    }

    pub async fn start_reconciliation(&self) {
        let mut buf = Vec::new();
        self.service.start_reconciliation(&mut buf).await;
    }

    async fn clear_expired_tombstones(&self) {
        loop {
            while let Some(value) = self.tombstones.pop_expired() {
                self.service.map.write().remove(&value);
            }
            tokio::time::sleep(TOMBSTONE_CLEARING).await;
        }
    }

    pub async fn run(self) {
        let clone = self.clone();
        tokio::join!(self.service.run(), clone.clear_expired_tombstones());
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Send + Serialize + Sync + 'static,
    > Service<K, V>
{
    pub fn get_mut<F: FnOnce(Option<&mut V>)>(&self, k: &K, callback: F) {
        let mut guard = self.service.map.write();
        guard.with_mut(k, |maybe_tv| {
            if let Some((_, v)) = maybe_tv {
                callback(v.as_mut());
            } else {
                callback(None);
            }
        });
    }
}

#[derive(Clone, Copy)]
pub struct ServiceConfig {
    pub port: u16,
    pub listen_addr: IpAddr,
    pub peer_net: IpNet,
    // may include other options in the future: use_tls, tombstone_ttl, metrics, etc.
}
impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            port: 0,
            listen_addr: "127.0.0.1".parse().unwrap(),
            peer_net: "127.0.0.1/8".parse().unwrap(),
        }
    }
}
impl ServiceConfig {
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    pub fn with_listen_addr(mut self, listen_addr: IpAddr) -> Self {
        self.listen_addr = listen_addr;
        self
    }
    pub fn with_peer_net(mut self, peer_net: IpNet) -> Self {
        self.peer_net = peer_net;
        self
    }
}

#[cfg(test)]
mod service_tests {
    use std::time::Duration;

    use crate::{service::ServiceConfig, Service};

    #[tokio::test]
    async fn tombstones_expiration() {
        let config = ServiceConfig {
            port: 8080,
            listen_addr: "127.0.0.45".parse().unwrap(),
            peer_net: "127.0.0.1/8".parse().unwrap(),
        };
        let service = Service::<i32, i32>::new(config)
            .await
            .with_tombstone_timeout(Duration::from_millis(1));

        let task = tokio::spawn(service.clone().run());

        // insert a tombstone
        service.remove(&0);
        tokio::time::sleep(Duration::from_millis(10)).await; // await its expiration
                                                             // The tombstone should be expired by now
        assert_eq!(service.tombstones.pop_expired(), Some(0));
        assert_eq!(service.tombstones.remove(&0), None);

        task.abort();
    }
}
