// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::net::IpAddr;
use std::time::Duration;

use ipnet::IpNet;
use tracing::{info, instrument};

use crate::bounds::{Key, Value};

use super::{ConfigError, ReplicatedMap};

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Run one round of anti-entropy against the configured peers: compare fingerprints, exchange
    /// any differing ranges, and merge what comes back. Normally driven on
    /// [`reconcile_interval`](super::Config::reconcile_interval) by the background task spawned at
    /// construction; exposed for callers that want to force an out-of-band round (e.g. in tests).
    pub async fn start_reconciliation(&self) {
        let mut buf = Vec::new();
        self.engine.start_reconciliation(&mut buf).await;
    }

    /// Permanently forget a peer, so tombstones stop waiting for its acknowledgment.
    ///
    /// The escape hatch from the causal-stability gate: a replica that is never coming back must
    /// be decommissioned, or its tombstones are retained forever.
    pub fn forget_peer(&self, peer: IpAddr) {
        self.engine.decommission_peer(peer);
    }

    /// The current membership set: peers that have sent a dated, authenticated datagram, and that
    /// gate tombstone GC.
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn members_snapshot(&self) -> std::collections::HashSet<std::net::IpAddr> {
        self.engine.members_snapshot()
    }

    /// Number of entries in the peers gossip-routing map.
    ///
    /// Exposed for integration-test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn peers_map_len(&self) -> usize {
        self.engine.peers_map_len()
    }

    /// Number of entries in the per-peer replay filter.
    ///
    /// Exposed for integration-test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn replay_filter_len(&self) -> usize {
        self.engine.replay_filter_len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for integration-test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn tombstone_acks_len(&self) -> usize {
        self.engine.tombstone_acks_len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for integration-test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn bulk_dumps_in_flight_count(&self) -> usize {
        self.engine.bulk_dumps_in_flight_count()
    }

    /// (runtime) Replace the declared geographical networks, re-deriving the local one. See
    /// [`Config::nets`](super::Config::nets).
    ///
    /// # Errors
    ///
    /// If `nets` exceeds [`MAX_NETS`](super::MAX_NETS) — the same cap
    /// `Config::with_net`/`try_with_net` enforce at construction time.
    ///
    /// Safe live otherwise: topology is per-node and carries no wire tag, and repair of known
    /// peers is not gated on net membership, so the worst case is suboptimal WAN traffic, never
    /// divergence.
    pub fn set_nets(&self, nets: &[IpNet]) -> Result<(), ConfigError> {
        self.engine.set_nets(nets)
    }

    /// (runtime) Declare an additional network (e.g. opening a new region). Idempotent; returns
    /// `false` (and logs) if the [`MAX_NETS`](super::MAX_NETS) cap is already reached. The local network is re-derived.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// use reconcile::{replicated_map::Config, InMemoryNetwork, ReplicatedMap};
    ///
    /// let network = InMemoryNetwork::new();
    /// let transport = Arc::new(network.bind("127.0.0.1:8308".parse().unwrap()));
    /// let store = ReplicatedMap::<String, i32>::new_with_transport(
    ///     Config::default().with_insecure_no_key(),
    ///     transport,
    /// );
    ///
    /// let region_a: ipnet::IpNet = "10.1.0.0/16".parse().unwrap();
    /// assert!(store.add_net(region_a)); // true: declared
    /// assert!(store.add_net(region_a)); // still true: adding it again is a no-op, not an error
    /// assert_eq!(store.nets().iter().filter(|n| **n == region_a).count(), 1); // no duplicate entry
    ///
    /// assert!(store.remove_net(region_a)); // true: was declared
    /// assert!(!store.nets().contains(&region_a));
    /// ```
    #[must_use]
    pub fn add_net(&self, net: IpNet) -> bool {
        self.engine.add_net(net)
    }

    /// (runtime) Stop declaring a network, returning whether it was present. Known peers keep
    /// being repaired; add a replacement net before removing the old one to keep discovery
    /// connected through a migration.
    #[must_use]
    pub fn remove_net(&self, net: IpNet) -> bool {
        self.engine.remove_net(net)
    }

    /// The currently declared networks.
    pub fn nets(&self) -> Vec<IpNet> {
        self.engine.nets()
    }

    /// The current local network (the declared net containing the listen address, else the host
    /// route — see [`Config::nets`](super::Config::nets)).
    pub fn local_net(&self) -> IpNet {
        self.engine.local_net()
    }

    /// (runtime) Retune how often (in rounds) remote-network peers are reconciled. See
    /// [`Config::remote_interval`](super::Config::remote_interval).
    pub fn set_remote_interval(&self, interval: u32) {
        self.engine.set_remote_interval(interval);
    }

    /// (runtime) Retune the bounded number of peers contacted per remote network each cross-network
    /// round. See [`Config::remote_fanout`](super::Config::remote_fanout).
    pub fn set_remote_fanout(&self, fanout: usize) {
        self.engine.set_remote_fanout(fanout);
    }

    /// (runtime) Retune the reconciliation cadence in place. See [`Config::reconcile_interval`](super::Config::reconcile_interval).
    pub fn set_reconcile_interval(&self, interval: Duration) {
        self.engine.set_reconcile_interval(interval);
    }

    /// Drive the gossip and reconciliation loops forever. Never returns and cannot fail: send
    /// errors are logged and counted.
    #[instrument(name = "reconcile.store", skip_all)]
    pub async fn run(self) {
        info!("reconcile store starting");
        let tombstones = self.clone();
        let snapshots = self.clone();
        let discovery = self.clone();
        tokio::join!(
            self.engine.run(),
            tombstones.clear_expired_tombstones(),
            snapshots.snapshot_periodically(),
            discovery.discover_periodically(),
        );
    }
}
