// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use ipnet::IpNet;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use crate::bounds::{Key, Value};

use super::{ConfigError, ReplicatedMap};

/// A snapshot of [`ReplicatedMap`]'s liveness, for a caller building its own readiness signal
/// (e.g. a Kubernetes probe) instead of conflating "the process is up" with "this node is
/// actually synchronizing".
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SyncState {
    /// Reconciliation rounds completed since construction.
    pub rounds: u64,
    /// When the most recently completed round started, or `None` before the first one.
    pub last_round_at: Option<Instant>,
    /// Current size of the gossip-routing peer set — see [`ReplicatedMap::peers`].
    pub peers: usize,
    /// When the most recent snapshot (periodic, or a caller-triggered
    /// [`snapshot_now`](ReplicatedMap::snapshot_now)) completed successfully, or `None` if none
    /// has happened yet.
    pub last_snapshot_at: Option<Instant>,
}

/// How [`ReplicatedMap::run`] ended.
///
/// Always cancellation today — the four background loops [`run`](ReplicatedMap::run) joins never
/// return on their own — but `#[non_exhaustive]` so a future variant (e.g. a fatal I/O error)
/// does not break a caller matching on this today.
#[derive(Debug)]
#[non_exhaustive]
pub struct RunOutcome {
    /// The result of the snapshot flush taken immediately after the shutdown signal fired.
    pub final_snapshot: io::Result<()>,
}

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
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn members_snapshot(&self) -> std::collections::HashSet<std::net::IpAddr> {
        self.engine.members_snapshot()
    }

    /// Number of entries in the peers gossip-routing map.
    ///
    /// Exposed for integration-test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn peers_map_len(&self) -> usize {
        self.engine.peers_map_len()
    }

    /// Number of entries in the per-peer replay filter.
    ///
    /// Exposed for integration-test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn replay_filter_len(&self) -> usize {
        self.engine.replay_filter_len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for integration-test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn tombstone_acks_len(&self) -> usize {
        self.engine.tombstone_acks_len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for integration-test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
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

    /// A snapshot of liveness for a caller building its own readiness signal — see [`SyncState`].
    pub fn sync_state(&self) -> SyncState {
        SyncState {
            rounds: u64::from(self.engine.round()),
            last_round_at: self.engine.last_round_at(),
            peers: self.engine.peers_vec().len(),
            last_snapshot_at: *self.last_snapshot_at.read(),
        }
    }

    /// The current gossip-routing peer set — see [`SyncState::peers`] for just its size.
    pub fn peers(&self) -> Vec<IpAddr> {
        self.engine.peers_vec()
    }

    /// The current causal-stability membership set — see `members_snapshot` (gated behind
    /// `cfg(reconcile_internal_testing)`) for the `HashSet` form test assertions use.
    pub fn members(&self) -> Vec<IpAddr> {
        self.engine.members_vec()
    }

    /// The transport's actual bound local address — what port was assigned, not just what was
    /// requested (see [`Config::port`](super::Config::port)'s docs on why the two can differ).
    ///
    /// # Errors
    ///
    /// If the underlying transport fails to report its local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.engine.local_addr()
    }

    /// Drive the gossip and reconciliation loops until `shutdown` fires, then flush a final
    /// snapshot before returning.
    ///
    /// Cannot fail outright: send errors during the run are logged and counted, and a failed final
    /// snapshot is reported via [`RunOutcome::final_snapshot`] rather than as an `Err` here, so a
    /// caller cannot mistake "the run loop panicked" for "the last flush failed".
    #[instrument(name = "reconcile.store", skip_all)]
    pub async fn run(self, shutdown: CancellationToken) -> RunOutcome {
        info!("reconcile store starting");
        let engine = self.engine.clone();
        let tombstones = self.clone();
        let snapshots = self.clone();
        let discovery = self.clone();
        tokio::select! {
            () = async {
                tokio::join!(
                    engine.run(),
                    tombstones.clear_expired_tombstones(),
                    snapshots.snapshot_periodically(),
                    discovery.discover_periodically(),
                );
            } => {}
            () = shutdown.cancelled() => {
                info!("reconcile store received shutdown signal");
            }
        }
        info!("flushing final snapshot before returning");
        let final_snapshot = self.snapshot_now();
        if let Err(ref err) = final_snapshot {
            warn!("final snapshot on shutdown failed: {err}");
        }
        RunOutcome { final_snapshot }
    }
}
