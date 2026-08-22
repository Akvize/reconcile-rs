// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::HashSet;
use std::hash::Hash;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use tracing::warn;

use gossip::gen_ip::{host_net, net_of};

use crate::bounds::{Key, Value};
use crate::replicated_map::{ConfigError, MAX_NETS};

use super::Replica;

/// Derive a node's **local network** — whichever declared net contains `listen_addr`, and the one
/// reconciled with every round.
///
/// With no match, falls back to the node's own host route, so every peer is treated as remote
/// rather than mis-qualified. Called at construction and on each `nets` mutation, so the warning
/// fires only there.
pub(crate) fn derive_local_net(nets: &[IpNet], listen_addr: IpAddr) -> IpNet {
    net_of(nets, listen_addr).unwrap_or_else(|| {
        warn!(
            "listen address {listen_addr} is contained in none of the configured networks \
             {nets:?}; cannot identify the local network — treating only this node as local, so \
             every peer is remote and reconciled on the throttled cross-network cadence. Declare \
             the network containing {listen_addr} via Config::with_net or ReplicatedMap::add_net.",
        );
        host_net(listen_addr)
    })
}

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// (runtime) Replace the declared networks wholesale and re-derive the local network.
    ///
    /// # Errors
    ///
    /// If `nets` exceeds [`MAX_NETS`] — the same cap `Config::with_net`/`try_with_net` enforce at
    /// construction time and [`add_net`](Self::add_net) enforces at runtime.
    pub(crate) fn set_nets(&self, nets: &[IpNet]) -> Result<(), ConfigError> {
        if nets.len() > MAX_NETS {
            return Err(ConfigError::TooManyNets);
        }
        let nets = nets.to_vec();
        let local = derive_local_net(&nets, self.listen_addr);
        // local_net is always derived from nets; update it together so a reader never observes a
        // local net that is inconsistent with the freshly-installed nets for long.
        *self.local_net.write() = local;
        *self.nets.write() = nets;
        Ok(())
    }

    /// (runtime) Declare an additional network. Idempotent; returns `false` (and logs) if the
    /// [`MAX_NETS`](crate::replicated_map::MAX_NETS) cap is reached.
    pub(crate) fn add_net(&self, net: IpNet) -> bool {
        let mut guard = self.nets.write();
        if guard.contains(&net) {
            return true;
        }
        if guard.len() >= MAX_NETS {
            warn!("cannot add network {net}: already at the maximum of {MAX_NETS} networks");
            return false;
        }
        guard.push(net);
        *self.local_net.write() = derive_local_net(&guard, self.listen_addr);
        true
    }

    /// (runtime) Stop declaring a network. Returns whether it was present. Anti-entropy repair of
    /// already-known peers is **not** gated on net membership, so removing a net never orphans a
    /// real peer — it only stops discovery probes into that range and may reclassify peers from
    /// local to remote (throttled) cadence.
    pub(crate) fn remove_net(&self, net: IpNet) -> bool {
        let mut guard = self.nets.write();
        let before = guard.len();
        guard.retain(|n| *n != net);
        let removed = guard.len() != before;
        if removed {
            *self.local_net.write() = derive_local_net(&guard, self.listen_addr);
        }
        removed
    }

    pub(crate) fn nets(&self) -> Vec<IpNet> {
        self.nets.read().clone()
    }

    pub(crate) fn local_net(&self) -> IpNet {
        *self.local_net.read()
    }

    pub(crate) fn set_remote_interval(&self, interval: u32) {
        self.remote_interval.store(interval, Ordering::Relaxed);
    }

    pub(crate) fn set_remote_fanout(&self, fanout: usize) {
        self.remote_fanout.store(fanout, Ordering::Relaxed);
    }

    pub(crate) fn set_reconcile_interval(&self, interval: Duration) {
        *self.reconcile_interval.write() = interval;
    }

    /// See [`Config::coalesce_window`](crate::replicated_map::Config::coalesce_window).
    pub(crate) fn set_coalesce_window(&self, window: Duration) {
        *self.coalesce_window.write() = window;
    }

    /// The currently configured coalescing window. Exposed for integration-test assertions
    /// under `cfg(reconcile_internal_testing)`, matching e.g. `peers_map_len`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn coalesce_window(&self) -> Duration {
        *self.coalesce_window.read()
    }

    /// Permanently remove a peer from membership, clearing its recorded acks, so tombstones stop
    /// waiting for it.
    ///
    /// Per-peer replay state is deliberately **kept** (AGENTS.md §8): dropping it would let a
    /// captured datagram replayed inside the freshness window re-add the peer.
    pub(crate) fn decommission_peer(&self, peer: IpAddr) {
        self.members.write().remove(&peer);
        self.peers.write().remove(&peer);
        for key_acks in self.tombstone_acks.write().values_mut() {
            key_acks.remove(&peer);
        }
    }

    /// Register or refresh a known peer at runtime — what a discovery source calls per resolved
    /// address.
    ///
    /// Re-arms `PEER_EXPIRATION` and makes the address a gossip target. Never touches
    /// [`members`](Self::members) (`ARCHITECTURE.md` §5 invariant 6).
    pub(crate) fn seed_peer(&self, peer: IpAddr) {
        self.peers.write().insert(peer, Instant::now());
    }

    /// A snapshot of the monotonic membership set (peers that gate tombstone GC).
    pub(crate) fn members_snapshot(&self) -> HashSet<IpAddr> {
        self.members.read().clone()
    }

    /// Number of entries currently in the peers gossip-routing map.
    ///
    /// Exposed for test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn peers_map_len(&self) -> usize {
        self.peers.read().len()
    }

    /// Number of entries currently in the per-peer replay filter.
    ///
    /// Exposed for test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn replay_filter_len(&self) -> usize {
        self.replay_filter.len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn tombstone_acks_len(&self) -> usize {
        self.tombstone_acks.read().len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for test assertions under `cfg(reconcile_internal_testing)`.
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn bulk_dumps_in_flight_count(&self) -> usize {
        self.bulk_dumps_in_flight.load(Ordering::Acquire)
    }

    /// This node's configured listen address, used by discovery to never decommission itself.
    pub(crate) fn listen_addr(&self) -> IpAddr {
        self.listen_addr
    }

    /// The transport's actual bound local address — what port was assigned, not just what was
    /// requested (see [`Config::port`](crate::replicated_map::Config::port)'s docs on why the two
    /// can differ).
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.transport.local_addr()
    }

    /// Reconciliation rounds completed so far. Monotonic, shared across clones — wraps at `u32`
    /// like the counter backing it.
    pub(crate) fn round(&self) -> u32 {
        self.round.load(Ordering::Relaxed)
    }

    /// When the most recently completed reconciliation round started, or `None` before the first
    /// one. Updated at [`start_reconciliation`](super::Replica::start_reconciliation).
    pub(crate) fn last_round_at(&self) -> Option<Instant> {
        *self.last_round_at.read()
    }

    /// The gossip-routing peer set: addresses seen recently enough to still be gossip targets
    /// (bounded by [`Config::freshness_window`](crate::replicated_map::Config::freshness_window)-
    /// adjacent expiry, unlike [`members`](Self::members_snapshot), which never expires).
    pub(crate) fn peers_vec(&self) -> Vec<IpAddr> {
        self.peers.read().keys().copied().collect()
    }

    /// The causal-stability membership set, as a `Vec` — see
    /// [`members_snapshot`](Self::members_snapshot) for the `HashSet` form
    /// `cfg(reconcile_internal_testing)` assertions use.
    pub(crate) fn members_vec(&self) -> Vec<IpAddr> {
        self.members.read().iter().copied().collect()
    }
}
