// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This crate provides a key-data map structure [`FingerprintTreeMap`] (from the [`rsos`] crate)
//! that can be used together with the reconciliation [`ReplicatedMap`]. Different instances can talk
//! together over UDP to efficiently reconcile their differences.
//!
//! All the data is available locally in all instances, and the user can be
//! notified of changes to the collection with an insertion hook.
//!
//! The protocol allows finding a difference over millions of elements with a limited
//! number of round-trips. It should also work well to populate an instance from
//! scratch from other instances.
//!
//! # When to use this
//!
//! An **embedded, in-memory, eventually-consistent replicated map**: every instance holds the
//! whole dataset and serves reads locally, writes propagate asynchronously and merge
//! last-write-wins. Right when reads dominate, the working set fits in RAM on every node, and
//! same-key conflicts are rare. Wrong for counters (LWW overwrites, it does not sum), strong
//! consistency, datasets past one node's RAM, and collaborative text.
//!
//! # Security model
//!
//! **Unauthenticated by default**: any host that can reach the port can forge an update and
//! poison the cluster through last-write-wins, and a captured datagram can be replayed. Suitable
//! only for a trusted underlay.
//!
//! [`Config::with_cluster_key`](replicated_map::Config::with_cluster_key), set on **every** node,
//! enables per-datagram MAC authentication and per-sender replay protection. Full threat model:
//! README "Security model".

#![forbid(unsafe_code)]

pub mod clock;
pub mod persistence;
pub mod read_replica_map;
pub mod replicated_map;
pub(crate) mod snapshot;

// Sibling crates re-exported under their historical paths (`ARCHITECTURE.md` §2).
pub use gossip::{discovery, transport};
pub use lww_register::{bounds, entry};

/// Optional Prometheus integration (enabled by the `metrics-prometheus` feature).
#[cfg(feature = "metrics-prometheus")]
pub mod prometheus;

// Internal mechanism, `pub(crate)` (`ARCHITECTURE.md` §3.2); test seams go through [`testing`].
pub(crate) mod observability;
pub(crate) mod replica;
pub(crate) mod timeout_wheel;

pub use bounds::{Key, Value};
pub use clock::{Clock, Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
pub use discovery::{DiscoverFuture, Discovery, DiscoveryKind, DnsDiscovery, RandomProbe};
pub use entry::{Entry, State};
pub use transport::{InMemoryNetwork, InMemoryTransport, Transport, UdpTransport};
// `IterMut`/`ValuesMut` are deliberately not re-exported: they leave fingerprints stale.
// `FingerprintTreeMap::with_mut` is the supported mutation path.
pub use rsos::{
    Aggregate, Fingerprint, FingerprintTreeMap, IntoIter, IntoKeys, IntoValues, Iter, Keys, Rsos,
    Values,
};

pub use persistence::{FileSnapshot, InMemoryPersistence, PersistedState, Persistence};
pub use read_replica_map::ReadReplicaMap;
pub use replicated_map::ReplicatedMap;

/// Internal seam for the integration tests, behind `cfg(test)` or `internal-testing`.
///
/// Carries only what stays crate-internal: `rbsr`/`rsos` primitives are `pub` on their own crates
/// and are imported directly.
#[doc(hidden)]
#[cfg(any(test, feature = "internal-testing"))]
pub mod testing {
    /// Seal `payload` with MAC authentication: `tag(32) || seq(8 LE) || stamp(8 LE) ||
    /// version(1) || payload` (the wire-version byte).
    pub fn seal_datagram(key: [u8; 32], seq: u64, stamp: u64, payload: &[u8]) -> Vec<u8> {
        gossip::auth::Authenticator::new(Some(key), false).seal(
            gossip::replay::Seq::new(seq),
            gossip::replay::Stamp::new(stamp),
            payload,
        )
    }

    /// The current causal-stability membership set.
    pub fn members_snapshot<K, V>(
        store: &crate::ReplicatedMap<K, V>,
    ) -> std::collections::HashSet<std::net::IpAddr>
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.members_snapshot()
    }

    /// Number of entries in the peers gossip-routing map.
    pub fn peers_map_len<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.peers_map_len()
    }

    /// Number of entries in the per-peer replay filter.
    pub fn replay_filter_len<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.replay_filter_len()
    }

    /// Number of keys tracked in the tombstone-acknowledgment map.
    pub fn tombstone_acks_len<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.tombstone_acks_len()
    }

    /// Number of bulk dump tasks in flight across all peers.
    pub fn bulk_dumps_in_flight_count<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.bulk_dumps_in_flight_count()
    }
}
