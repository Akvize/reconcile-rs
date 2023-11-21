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
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ipnet::IpNet;
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLockReadGuard, RwLockWriteGuard,
};
use serde::{de::DeserializeOwned, Serialize};

use crate::diff::Diffable;
use crate::internal_service::InternalService;
use crate::map::{Map, MutMap};
use crate::timeout_wheel::TimeoutWheel;

pub type MaybeTombstone<V> = Option<V>;
pub type DatedMaybeTombstone<V> = (DateTime<Utc>, MaybeTombstone<V>);

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// Wraps a key-value map to enable reconciliation between different instances over a network.
///
/// The service also keeps track of the addresses of other instances.
///
/// Provides wrappers for its underlying [`Map`]s insertion and deletion methods,
/// as well as its main service method: `run()`,
/// which must be called to actually synchronize with peers.
///
/// Known peers can optionally be provided using the [`with_seed`](Service::with_seed) method. In
/// any case, the service will periodically look for new peers by sampling a random address from
/// the given peer network.
pub struct Service<M: Map>
where
    M::Key: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    service: InternalService<M>,
    tombstones: TimeoutWheel<M::Key>,
}

impl<M: Map> Clone for Service<M>
where
    M::Key: Clone + Hash + std::cmp::Eq + Send + Sync,
{
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
        C: Debug + DeserializeOwned + Send + Serialize + Sync + 'static,
        D: Debug + 'static,
        M: Map<Key = K, Value = DatedMaybeTombstone<V>, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>
            + Send
            + Sync
            + 'static,
    > Service<M>
{
    pub async fn new(map: M, port: u16, listen_addr: IpAddr, peer_net: IpNet) -> Self {
        Service {
            service: InternalService::new(map, port, listen_addr, peer_net).await,
            tombstones: TimeoutWheel::new(),
        }
        .with_pre_insert(|_, _| {})
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

    pub fn with_pre_insert<F: Send + Sync + Fn(&M::Key, &M::Value) + 'static>(
        self,
        pre_insert: F,
    ) -> Self {
        let tombstones = self.tombstones.clone();
        let wrapped_pre_insert = move |k: &K, v: &(DateTime<Utc>, Option<V>)| {
            pre_insert(k, v);
            if v.1.is_some() {
                tombstones.remove(k);
            } else {
                tombstones.insert(k.clone(), v.0);
            }
        };
        *self.service.pre_insert.write() = Box::new(wrapped_pre_insert);
        self
    }

    /// Direct read access to the underlying map.
    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.service.map.read()
    }

    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.service.map.read();
        RwLockReadGuard::try_map(guard, |map: &M| map.get(k).and_then(|(_, v)| v.as_ref())).ok()
    }

    pub fn just_insert(&self, key: K, value: V, timestamp: DateTime<Utc>) -> Option<V> {
        let ret = self.service.just_insert(key, (timestamp, Some(value)));
        ret.and_then(|t| t.1)
    }

    pub fn insert(&self, key: K, value: V, timestamp: DateTime<Utc>) -> Option<V> {
        let ret = self.service.insert(key, (timestamp, Some(value)));
        ret.and_then(|t| t.1)
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, V, DateTime<Utc>)]) {
        self.service.just_insert_bulk(
            &key_values
                .iter()
                .map(|(k, v, t)| (k.clone(), (*t, Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn insert_bulk(&self, key_values: &[(K, V, DateTime<Utc>)]) {
        self.service.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v, t)| (k.clone(), (*t, Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn just_remove(&self, key: &K, timestamp: DateTime<Utc>) -> Option<V> {
        let ret = self.service.just_insert(key.clone(), (timestamp, None));
        ret.and_then(|t| t.1)
    }

    pub fn remove(&self, key: &K, timestamp: DateTime<Utc>) -> Option<V> {
        let ret = self.service.insert(key.clone(), (timestamp, None));
        ret.and_then(|t| t.1)
    }

    pub fn just_remove_bulk(&self, keys: &[(K, DateTime<Utc>)]) {
        self.service.just_insert_bulk(
            &keys
                .iter()
                .map(|(k, t)| (k.clone(), (*t, None)))
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
        C: Debug + DeserializeOwned + Send + Serialize + Sync + 'static,
        D: Debug + 'static,
        M: MutMap<Key = K, Value = DatedMaybeTombstone<V>, DifferenceItem = D>
            + Diffable<ComparisonItem = C, DifferenceItem = D>
            + Send
            + Sync
            + 'static,
    > Service<M>
{
    pub fn get_mut(&self, k: &K) -> Option<MappedRwLockWriteGuard<'_, V>> {
        let guard = self.service.map.write();
        RwLockWriteGuard::try_map(guard, |map: &mut M| {
            map.get_mut(k).and_then(|(_, v)| v.as_mut())
        })
        .ok()
    }
}

#[cfg(test)]
mod service_tests {
    use chrono::Utc;
    use std::time::Duration;

    use crate::{DatedMaybeTombstone, HRTree, Service};

    #[tokio::test]
    async fn tombstones_expiration() {
        let service = Service::new(
            HRTree::<u8, DatedMaybeTombstone<String>>::new(),
            8080,
            "127.0.0.44".parse().unwrap(),
            "127.0.0.1/8".parse().unwrap(),
        )
        .await
        .with_tombstone_timeout(Duration::from_millis(1));

        let task = tokio::spawn(service.clone().run());

        // insert an already-expired tombstone
        service.remove(&0, Utc::now() - Duration::from_millis(2));
        // check that pop_expired() does yield the tombstone
        assert_eq!(service.tombstones.pop_expired(), Some(0));
        // check that it was indeed removed
        assert_eq!(service.tombstones.remove(&0), None);

        task.abort();
    }
}
