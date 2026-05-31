// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReconcileStore`], a wrapper to a key-value map
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

use crate::fingerprint::Fingerprint;
use crate::reconcile_engine::{version_hash, ReconcileEngine};
use crate::timeout_wheel::TimeoutWheel;

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// Core service wrapping a key-value map to enable reconciliation between different instances over a network.
///
/// The store also keeps track of the addresses of other instances.
///
/// Provides wrappers for its underlying [`HRTree`](crate::HRTree)'s insertion and deletion methods,
/// as well as its main service method: `run()`,
/// which must be called to actually synchronize with peers.
///
/// Known peers can optionally be provided using the [`with_seed`](ReconcileStore::with_seed) method. In
/// any case, the store will periodically look for new peers by sampling a random address from
/// the given peer network.
pub struct ReconcileStore<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    /// Internal map and hooks container.
    engine: ReconcileEngine<K, (DateTime<Utc>, Option<V>)>,
    /// Tombstone timestamps for deleted entries.
    tombstones: TimeoutWheel<K>,
}

impl<K, V> Clone for ReconcileStore<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    /// Allows cloning of the `ReconcileStore` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        ReconcileStore {
            engine: self.engine.clone(),
            tombstones: self.tombstones.clone(),
        }
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + PartialEq + Send + Serialize + Sync + 'static,
    > ReconcileStore<K, V>
{
    /// Create a new `ReconcileStore`, set up network and tombstones.
    pub async fn new(config: Config) -> Self {
        let svc = ReconcileStore {
            engine: ReconcileEngine::<K, (DateTime<Utc>, Option<V>)>::new(config).await,
            tombstones: TimeoutWheel::new(),
        };
        svc.add_pre_insert(|_, _| {});
        svc
    }

    /// Provides the address of a known peer to the store
    ///
    /// This is optional, but reduces the time to connect to existing peers
    pub fn with_seed(self, peer: IpAddr) -> Self {
        let now = Instant::now();
        self.engine.peers.write().insert(peer, now);
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
    /// Calling this does **not** consume the `ReconcileStore` instance; you can call it multiple times.
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
        *self.engine.pre_insert.write() = Box::new(wrapped_pre_insert);
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.fingerprint(range)
    }

    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.engine.map.read();
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
        let ret = self.engine.just_insert(key, (Utc::now(), Some(value)));
        ret.and_then(|t| t.1)
    }

    /// Fully-qualified insert: just_insert + async broadcast.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.engine.insert(key, (Utc::now(), Some(value)));
        ret.and_then(|t| t.1)
    }

    /// Bulk-insert multiple key/value pairs with hook invocation.
    ///
    /// # Behavior
    ///
    /// 1. Runs the pre-insert hook for each entry (outside any lock).
    /// 2. Acquires the write lock once and inserts all entries.
    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        self.engine.just_insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| (k.clone(), (Utc::now(), Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-insert + async broadcast.
    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.engine.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| (k.clone(), (Utc::now(), Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn just_remove(&self, key: &K) -> Option<V> {
        let ret = self.engine.just_insert(key.clone(), (Utc::now(), None));
        ret.and_then(|t| t.1)
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let ret = self.engine.insert(key.clone(), (Utc::now(), None));
        ret.and_then(|t| t.1)
    }

    pub fn just_remove_bulk(&self, keys: &[K]) {
        self.engine.just_insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), (Utc::now(), None)))
                .collect::<Vec<_>>(),
        );
    }

    pub fn remove_bulk(&self, keys: &[(K, DateTime<Utc>)]) {
        self.engine.insert_bulk(
            &keys
                .iter()
                .map(|(k, t)| (k.clone(), (*t, None)))
                .collect::<Vec<_>>(),
        );
    }

    pub async fn start_reconciliation(&self) {
        let mut buf = Vec::new();
        self.engine.start_reconciliation(&mut buf).await;
    }

    /// Permanently forget a peer (e.g. a replica that has been decommissioned and will never
    /// return), so that tombstones are no longer held back waiting for its acknowledgment
    /// before garbage collection.
    ///
    /// This is the escape hatch for the causal-stability contract: a tombstone is normally
    /// retained until every peer this node has ever communicated with has acknowledged it.
    /// A replica that is permanently gone must be decommissioned, otherwise its tombstones are
    /// retained forever.
    pub fn forget_peer(&self, peer: IpAddr) {
        self.engine.decommission_peer(peer);
    }

    /// Garbage-collect tombstones, **gated on causal stability**.
    ///
    /// A tombstone is removed from the map only once it is both older than the configured
    /// timeout *and* acknowledged by every replica this node has ever communicated with (or
    /// those replicas have been decommissioned via [`forget_peer`](Self::forget_peer)). This
    /// prevents the classic tombstone-resurrection hazard: a replica partitioned for longer
    /// than the timeout cannot resurrect a deleted value on its return, because the tombstone
    /// is kept until that replica has seen it.
    async fn clear_expired_tombstones(&self) {
        loop {
            for key in self.tombstones.expired() {
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
                    self.engine.map.write().remove(&key);
                    self.engine.forget_tombstone(&key);
                }
                // Otherwise keep the tombstone and re-check on a later iteration.
            }
            tokio::time::sleep(TOMBSTONE_CLEARING).await;
        }
    }

    pub async fn run(self) {
        let clone = self.clone();
        tokio::join!(self.engine.run(), clone.clear_expired_tombstones());
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Send + Serialize + Sync + 'static,
    > ReconcileStore<K, V>
{
    pub fn get_mut<F: FnOnce(Option<&mut V>)>(&self, k: &K, callback: F) {
        let mut guard = self.engine.map.write();
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
pub struct Config {
    pub port: u16,
    pub listen_addr: IpAddr,
    pub peer_net: IpNet,
    /// Optional shared cluster secret enabling per-datagram MAC authentication.
    ///
    /// When `None` (the default), the protocol is **unauthenticated**: any host able to send a
    /// UDP datagram to the port can forge updates and poison the cluster. When set, every node in
    /// the cluster must use the **same** 32-byte key (and the same MAC backend feature). See
    /// [`Config::with_cluster_key`].
    pub cluster_key: Option<[u8; 32]>,
    // may include other options in the future: use_tls, tombstone_ttl, metrics, etc.
}
impl Default for Config {
    fn default() -> Self {
        Config {
            port: 0,
            listen_addr: "127.0.0.1".parse().unwrap(),
            peer_net: "127.0.0.1/8".parse().unwrap(),
            cluster_key: None,
        }
    }
}
impl Config {
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

    /// Enable per-datagram MAC authentication using a shared 32-byte cluster secret.
    ///
    /// Every outgoing datagram is then framed with a keyed MAC over its payload, and every
    /// incoming datagram is verified **before** deserialization; datagrams with a missing or
    /// invalid tag are silently dropped. This closes the unauthenticated last-write-wins
    /// poisoning vector.
    ///
    /// All nodes in the same cluster **must** share the identical key and be built with the same
    /// MAC backend feature (`mac-blake3`, the default, or `mac-hmac`). Without a key, the store
    /// logs a loud warning at startup and runs unauthenticated.
    pub fn with_cluster_key(mut self, key: [u8; 32]) -> Self {
        self.cluster_key = Some(key);
        self
    }
}

#[cfg(test)]
mod reconcile_store_tests {
    use std::time::Duration;

    use crate::{reconcile_store::Config, ReconcileStore};

    #[tokio::test]
    async fn tombstones_expiration() {
        let config = Config {
            port: 8080,
            listen_addr: "127.0.0.45".parse().unwrap(),
            peer_net: "127.0.0.1/8".parse().unwrap(),
            cluster_key: None,
        };
        let store = ReconcileStore::<i32, i32>::new(config)
            .await
            .with_tombstone_timeout(Duration::from_millis(1));

        let task = tokio::spawn(store.clone().run());

        // insert a tombstone
        store.remove(&0);
        tokio::time::sleep(Duration::from_millis(10)).await; // await its expiration
                                                             // The tombstone should be expired by now
        assert_eq!(store.tombstones.expired(), vec![0]);
        assert_eq!(store.tombstones.remove(&0), Some(0));
        assert_eq!(store.tombstones.remove(&0), None);

        task.abort();
    }
}
