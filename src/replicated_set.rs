// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`ReplicatedSet`]: a replicated set — membership-as-keys (README "Modelling sets") wrapped in
//! a set-shaped API instead of a raw [`ReplicatedMap<K, ()>`](crate::ReplicatedMap).
//!
//! A thin newtype, not a reimplementation: wire format, reconciliation protocol and persistence
//! are exactly `ReplicatedMap<K, ()>`'s. What it changes is the surface — `insert`/`remove`
//! return `bool` instead of a `()`-shaped `Option`, and the value-shaped half of `ReplicatedMap`
//! (`get`, `update`, `upsert`, `get_or_insert_with`, `values`, ...) is not exposed at all: none of
//! it has a meaningful reading when every value is `()`. Reach for `ReplicatedMap<K, ()>` directly
//! if one of those is genuinely needed.

use std::hash::Hash;
use std::io;
use std::net::IpAddr;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;

use crate::bounds::Key;
use crate::clock::{NodeId, Timestamp};
use crate::entry::Entry;
use crate::persistence::Persistence;
use crate::replicated_map::{Config, ConfigError};
use crate::{Discovery, ReplicatedMap};
use rsos::Fingerprint;

/// A replicated set; see the [module documentation](crate::replicated_set).
pub struct ReplicatedSet<K>(ReplicatedMap<K, ()>)
where
    K: Clone + Hash + Eq + Send + Sync;

impl<K: Clone + Hash + Eq + Send + Sync> Clone for ReplicatedSet<K> {
    /// Allows cloning the `ReplicatedSet` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        ReplicatedSet(self.0.clone())
    }
}

impl<K: Key + Hash> ReplicatedSet<K> {
    /// Create a `ReplicatedSet`, binding the gossip UDP socket. See [`ReplicatedMap::new`].
    ///
    /// # Errors
    ///
    /// If the socket cannot be bound to `(config.listen_addr, config.port)`.
    pub async fn new(config: Config) -> io::Result<Self> {
        ReplicatedMap::new(config).await.map(ReplicatedSet)
    }

    /// This node's HLC identity. See [`ReplicatedMap::node_id`].
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.0.node_id()
    }

    /// Plug in a durable persistence backend. See [`ReplicatedMap::with_persistence`].
    #[must_use]
    pub fn with_persistence(self, backend: Arc<dyn Persistence<K, ()>>) -> Self {
        ReplicatedSet(self.0.with_persistence(backend))
    }

    /// Seed an initial peer. See [`ReplicatedMap::with_seed`].
    #[must_use]
    pub fn with_seed(self, peer: IpAddr) -> Self {
        ReplicatedSet(self.0.with_seed(peer))
    }

    /// Register or refresh a known peer at runtime. See [`ReplicatedMap::seed_peer`].
    pub fn seed_peer(&self, peer: IpAddr) {
        self.0.seed_peer(peer);
    }

    /// Attach an authoritative peer-discovery source. See [`ReplicatedMap::with_discovery`].
    #[must_use]
    pub fn with_discovery(self, discovery: Arc<dyn Discovery>) -> Self {
        ReplicatedSet(self.0.with_discovery(discovery))
    }

    /// Discover peers by resolving a DNS name. See [`ReplicatedMap::with_dns_discovery`].
    #[must_use]
    pub fn with_dns_discovery(self, name: impl Into<String>, port: u16) -> Self {
        ReplicatedSet(self.0.with_dns_discovery(name, port))
    }

    /// See [`ReplicatedMap::with_discovery_interval`].
    #[must_use]
    pub fn with_discovery_interval(self, interval: Duration) -> Self {
        ReplicatedSet(self.0.with_discovery_interval(interval))
    }

    /// See [`ReplicatedMap::with_discovery_miss_threshold`].
    #[must_use]
    pub fn with_discovery_miss_threshold(self, threshold: u32) -> Self {
        ReplicatedSet(self.0.with_discovery_miss_threshold(threshold))
    }

    /// See [`ReplicatedMap::with_discovery_decommission_floor`].
    #[must_use]
    pub fn with_discovery_decommission_floor(self, floor: Duration) -> Self {
        ReplicatedSet(self.0.with_discovery_decommission_floor(floor))
    }

    /// See [`ReplicatedMap::with_tombstone_timeout`].
    #[must_use]
    pub fn with_tombstone_timeout(self, tombstone_timeout: Duration) -> Self {
        ReplicatedSet(self.0.with_tombstone_timeout(tombstone_timeout))
    }

    /// Set the pre-insert hook, invoked before each key reaches the set. See
    /// [`ReplicatedMap::set_pre_insert`].
    pub fn set_pre_insert<F: Send + Sync + Fn(&K, &Entry<Timestamp, ()>) + 'static>(
        &self,
        pre_insert: F,
    ) {
        self.0.set_pre_insert(pre_insert);
    }

    /// Fingerprint of a key range. See [`ReplicatedMap::fingerprint`].
    #[must_use]
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.0.fingerprint(range)
    }

    /// Number of members. See [`ReplicatedMap::len`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty. See [`ReplicatedMap::is_empty`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Add `key` as a member. Returns whether it was already present (idempotent either way).
    ///
    /// # Panics
    ///
    /// See [`ReplicatedMap::insert`] — the broadcast requires an ambient Tokio runtime.
    #[must_use]
    pub fn insert(&self, key: K) -> bool {
        self.0.insert(key, ()).is_some()
    }

    /// Add every key in `keys` as a member, one broadcast batch. See
    /// [`ReplicatedMap::insert_bulk`].
    pub fn insert_bulk(&self, keys: &[K]) {
        let pairs: Vec<(K, ())> = keys.iter().cloned().map(|k| (k, ())).collect();
        self.0.insert_bulk(&pairs);
    }

    /// Add every key in `keys` as a member, without broadcasting. See
    /// [`ReplicatedMap::load_bulk`].
    pub fn load_bulk(&self, keys: &[K]) {
        let pairs: Vec<(K, ())> = keys.iter().cloned().map(|k| (k, ())).collect();
        self.0.load_bulk(&pairs);
    }

    /// Remove `key` from the set. Returns whether it was present.
    ///
    /// # Panics
    ///
    /// See [`ReplicatedMap::remove`] — the broadcast requires an ambient Tokio runtime.
    #[must_use]
    pub fn remove(&self, key: &K) -> bool {
        self.0.remove(key).is_some()
    }

    /// Remove every key in `keys` from the set. See [`ReplicatedMap::remove_bulk`].
    pub fn remove_bulk(&self, keys: &[K]) {
        self.0.remove_bulk(keys);
    }

    /// Whether `key` is currently a member.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    /// Delete every member. See [`ReplicatedMap::clear`].
    pub fn clear(&self) {
        self.0.clear();
    }

    /// Delete every member for which `keep` returns `false`. See [`ReplicatedMap::retain`].
    pub fn retain<P: FnMut(&K) -> bool>(&self, mut keep: P) {
        self.0.retain(|k, ()| keep(k));
    }

    /// Delete every member whose key falls in `range`. See [`ReplicatedMap::delete_range`].
    pub fn delete_range<R: RangeBounds<K>>(&self, range: R) {
        self.0.delete_range(range);
    }

    /// The current members. See [`ReplicatedMap::keys`].
    #[must_use]
    pub fn keys(&self) -> Vec<K> {
        self.0.keys()
    }

    /// Start an out-of-cadence reconciliation round. See
    /// [`ReplicatedMap::start_reconciliation`].
    pub async fn start_reconciliation(&self) {
        self.0.start_reconciliation().await;
    }

    /// Permanently forget a peer. See [`ReplicatedMap::forget_peer`].
    pub fn forget_peer(&self, peer: IpAddr) {
        self.0.forget_peer(peer);
    }

    /// (runtime) Replace the declared networks. See [`ReplicatedMap::set_nets`].
    ///
    /// # Errors
    ///
    /// If `nets` exceeds `MAX_NETS`.
    pub fn set_nets(&self, nets: &[IpNet]) -> Result<(), ConfigError> {
        self.0.set_nets(nets)
    }

    /// (runtime) Declare an additional network. See [`ReplicatedMap::add_net`].
    #[must_use]
    pub fn add_net(&self, net: IpNet) -> bool {
        self.0.add_net(net)
    }

    /// (runtime) Stop declaring a network. See [`ReplicatedMap::remove_net`].
    #[must_use]
    pub fn remove_net(&self, net: IpNet) -> bool {
        self.0.remove_net(net)
    }

    /// The currently declared networks. See [`ReplicatedMap::nets`].
    #[must_use]
    pub fn nets(&self) -> Vec<IpNet> {
        self.0.nets()
    }

    /// The current local network. See [`ReplicatedMap::local_net`].
    #[must_use]
    pub fn local_net(&self) -> IpNet {
        self.0.local_net()
    }

    /// (runtime) Retune the cross-network reconciliation cadence. See
    /// [`ReplicatedMap::set_remote_interval`].
    pub fn set_remote_interval(&self, interval: u32) {
        self.0.set_remote_interval(interval);
    }

    /// (runtime) Retune the cross-network fan-out. See [`ReplicatedMap::set_remote_fanout`].
    pub fn set_remote_fanout(&self, fanout: usize) {
        self.0.set_remote_fanout(fanout);
    }

    /// (runtime) Retune the tombstone expiry timeout. See
    /// [`ReplicatedMap::set_tombstone_timeout`].
    pub fn set_tombstone_timeout(&self, timeout: Duration) {
        self.0.set_tombstone_timeout(timeout);
    }

    /// (runtime) Retune the reconciliation cadence. See
    /// [`ReplicatedMap::set_reconcile_interval`].
    pub fn set_reconcile_interval(&self, interval: Duration) {
        self.0.set_reconcile_interval(interval);
    }

    /// Run the reconciliation loop. See [`ReplicatedMap::run`].
    pub async fn run(self) {
        self.0.run().await;
    }
}

#[cfg(test)]
mod replicated_set_tests {
    use std::time::Duration;

    use crate::replicated_map::{Config, MAX_NETS};
    use crate::ReplicatedSet;

    fn ephemeral_config() -> Config {
        Config {
            port: crate::replica::next_ephemeral_test_port(),
            listen_addr: "127.0.0.1".parse().unwrap(),
            nets: [None; MAX_NETS],
            remote_interval: 6,
            remote_fanout: 2,
            cluster_key: None,
            insecure_no_key: true,
            node_id: None,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
            bulk_send_rate: Some(32 * 1024 * 1024),
            recv_buffer_size: Some(8 * 1024 * 1024),
            send_buffer_size: Some(8 * 1024 * 1024),
            freshness_window: gossip::replay::FRESHNESS_WINDOW_DEFAULT,
            max_peers: 1024,
            max_concurrent_bulk_dumps: 4,
        }
    }

    /// #377: `insert`/`remove` report prior presence, `contains` reports current presence, and
    /// bulk/`len`/`is_empty` track the same membership.
    #[tokio::test]
    async fn insert_remove_contains_and_bulk_agree_on_membership() {
        let set = ReplicatedSet::<i32>::new(ephemeral_config()).await.unwrap();

        assert!(set.is_empty());
        assert!(!set.contains(&1));
        assert!(!set.insert(1)); // wasn't present
        assert!(set.contains(&1));
        assert!(set.insert(1)); // already present, idempotent
        assert_eq!(set.len(), 1);

        set.insert_bulk(&[2, 3]);
        assert_eq!(set.len(), 3);
        assert!(set.contains(&2) && set.contains(&3));

        assert!(set.remove(&1)); // was present
        assert!(!set.contains(&1));
        assert!(!set.remove(&1)); // already gone

        set.remove_bulk(&[2, 3]);
        assert!(set.is_empty());
    }

    /// #293: `ReplicatedSet::set_nets` is a thin delegate to `ReplicatedMap::set_nets` — assert
    /// the delegation actually happens (the `MAX_NETS` cap is enforced through it), not just that
    /// calling it doesn't panic.
    #[tokio::test]
    async fn set_nets_enforces_max_nets_at_runtime() {
        let set = ReplicatedSet::<i32>::new(ephemeral_config()).await.unwrap();

        let within_cap: Vec<_> = (0..MAX_NETS)
            .map(|i| format!("127.0.0.0/{}", 8 + (i % 24)).parse().unwrap())
            .collect();
        set.set_nets(&within_cap)
            .expect("exactly MAX_NETS networks should be accepted");

        let over_cap: Vec<_> = (0..=MAX_NETS)
            .map(|i| format!("127.0.0.0/{}", 8 + (i % 24)).parse().unwrap())
            .collect();
        assert_eq!(
            set.set_nets(&over_cap),
            Err(crate::replicated_map::ConfigError::TooManyNets),
            "MAX_NETS + 1 networks should be rejected"
        );
    }
}
