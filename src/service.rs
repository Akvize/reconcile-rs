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
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use ipnet::IpNet;
use serde::{de::DeserializeOwned, Serialize};

use crate::diff::Diffable;
use crate::internal_service::InternalService;
use crate::map::Map;
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
    tombstones: Arc<RwLock<TimeoutWheel<M::Key>>>,
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
            tombstones: Arc::new(RwLock::new(TimeoutWheel::new())),
        }
    }

    /// Provides the address of a known peer to the service
    ///
    /// This is optional, but reduces the time to connect to existing peers
    pub fn with_seed(mut self, peer: IpAddr) -> Self {
        self.service = self.service.with_seed(peer);
        self
    }

    pub fn with_before_insert<
        F: Send + Sync + Fn(&M::Key, &M::Value, Option<&M::Value>) + 'static,
    >(
        mut self,
        before_insert: F,
    ) -> Self {
        self.service = self.service.with_before_insert(before_insert);
        self
    }

    /// Direct read access to the underlying map.
    pub fn read(&self) -> RwLockReadGuard<'_, M> {
        self.service.read()
    }

    pub fn insert(&self, key: K, value: V, timestamp: DateTime<Utc>) -> Option<V> {
        let mut guard = self.tombstones.write().unwrap();
        guard.remove(&key);

        let ret = self.service.insert(key, (timestamp, Some(value)));
        ret.and_then(|t| t.1)
    }

    pub fn insert_bulk(&self, key_values: &[(K, V, DateTime<Utc>)]) {
        let mut guard = self.tombstones.write().unwrap();
        for (k, _, _) in key_values {
            guard.remove(k);
        }

        self.service.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v, t)| (k.clone(), (*t, Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn remove(&self, key: &K, timestamp: DateTime<Utc>) -> Option<V> {
        let mut guard = self.tombstones.write().unwrap();
        guard.insert(key.clone(), timestamp);

        let ret = self.service.insert(key.clone(), (timestamp, None));
        ret.and_then(|t| t.1)
    }

    pub fn remove_bulk(&self, keys: &[(K, DateTime<Utc>)]) {
        let mut guard = self.tombstones.write().unwrap();
        for (k, t) in keys {
            guard.insert(k.clone(), *t);
        }

        self.service.insert_bulk(
            &keys
                .iter()
                .map(|(k, t)| (k.clone(), (*t, None)))
                .collect::<Vec<_>>(),
        );
    }

    async fn clear_expired_tombstones(&self) {
        loop {
            while let Some(value) = self.tombstones.write().unwrap().pop_expired() {
                self.service.map.write().unwrap().remove(&value);
            }
            tokio::time::sleep(TOMBSTONE_CLEARING).await;
        }
    }

    pub async fn run(self) {
        let clone = self.clone();
        tokio::spawn(async move { clone.clear_expired_tombstones().await });
        self.service.run().await
    }
}
