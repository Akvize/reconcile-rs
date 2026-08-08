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

//! All the data is available locally in all instances, and the user can be
//! notified of changes to the collection with an insertion hook.

//! The protocol allows finding a difference over millions of elements with a limited
//! number of round-trips. It should also work well to populate an instance from
//! scratch from other instances.

//! # When to use this
//!
//! `reconcile-rs` is an **embedded, in-memory, eventually-consistent replicated map** — in
//! data-grid terms, the masterless / AP / gossip corner of an in-memory data grid (the niche of
//! Hazelcast's *Replicated Map* or Pekko *Distributed Data*, with no mature Rust equivalent). Every
//! instance keeps the **whole dataset in memory** and serves reads locally with no network hop;
//! writes propagate asynchronously and merge last-write-wins.
//!
//! Good fit:
//! - reads dominate and must be fast and local (no per-read round-trip to Redis/etcd);
//! - the working set fits in RAM on every node (full replication gives redundancy, not sharding);
//! - eventual consistency and last-write-wins are acceptable, and same-key conflicts are rare;
//! - you want no separate datastore to operate, and want to keep serving across partitions.
//!
//! Wrong tool for: counters/quotas (LWW overwrites, it does not sum), ledgers or anything needing
//! strong consistency or transactions, datasets larger than one node's RAM (it is fully replicated,
//! not partitioned), and collaborative text (use a sequence CRDT).
//!
//! Because every replica holds everything, memory use and write fan-out grow with the dataset and
//! the node count; see the open performance issues (cold-sync throughput, per-entry memory
//! overhead, point-read latency) for current limitations and their status.

//! # Security model
//!
//! By default the UDP reconciliation protocol is **unauthenticated**: any host able to send a
//! datagram to the port can forge an update and poison the whole cluster through last-write-wins,
//! and there is **no replay protection** — a captured datagram can be re-injected later to
//! re-poison membership or re-deliver stale data. Unauthenticated mode is intentionally unprotected
//! against both attacks; it is suitable only for fully trusted network underlays.
//!
//! To close the forgery and replay vectors, configure a shared cluster secret with
//! [`Config::with_cluster_key`](replicated_map::Config::with_cluster_key) on **every** node: this
//! enables per-datagram MAC authentication and per-sender replay protection. Every datagram carries
//! a monotonically increasing sequence number and a sender wall-clock stamp (both inside the
//! authenticated region); the receiver maintains per-peer state and rejects duplicates, stale
//! out-of-window sequences, and datagrams whose freshness stamp deviates from local physical time
//! by more than the configured freshness window (default 5 minutes). See the README "Security
//! model" section for the full threat model and scope.

// The entire crate is implemented in safe Rust; this turns any `unsafe` block into a hard
// compile error.
#![forbid(unsafe_code)]

// Modules that still live here: the replicated-map facade and the per-node driver behind it, the
// wall-clock adapter behind the `Clock` port, tombstone expiry, and metrics.
pub mod clock;
pub mod persistence;
pub mod read_replica_map;
pub mod replicated_map;
// The file-backed `Persistence` adapter (`FileSnapshot` + the versioned on-disk header). Private:
// its one public type is re-exported through [`persistence`] and the crate root, the paths it has
// always been reachable by.
pub(crate) mod snapshot;

// Modules that moved out to sibling crates in the workspace split (ARCHITECTURE.md §3.9), re-exported
// under their historical paths so `reconcile::entry::Entry`, `reconcile::transport::UdpTransport`
// and friends keep resolving unchanged for existing consumers.
pub use gossip::{discovery, transport};
pub use lww_register::{bounds, entry};

/// Optional Prometheus integration (enabled by the `metrics-prometheus` feature).
#[cfg(feature = "metrics-prometheus")]
pub mod prometheus;

// Internal reconciliation mechanism. `pub(crate)` (ARCHITECTURE.md §3.7): these are implementation
// details, not part of the supported public surface. The few internals the integration-test oracles
// need are re-exported through the gated [`testing`] module below.
pub(crate) mod observability;
pub(crate) mod replica;
pub(crate) mod timeout_wheel;

pub use bounds::{Key, Value};
pub use clock::{Clock, Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
pub use discovery::{DiscoverFuture, Discovery, DiscoveryKind, DnsDiscovery, RandomProbe};
pub use entry::{Entry, State};
pub use transport::{InMemoryNetwork, InMemoryTransport, Transport, UdpTransport};
// `FingerprintTreeMap`, `Fingerprint`, `Aggregate`, the `Rsos<K>` trait, and its
// iterator types now live in the standalone `rsos` crate (see `rsos/src/lib.rs`); re-exported here
// so existing consumers of `reconcile::*` see no change in what resolves at the crate root.
// `IterMut`/`ValuesMut` are intentionally not re-exported (and are themselves `#[cfg(test)]`-only
// in `rsos`): they hand out `&mut V` without updating per-element fingerprints or the
// node's cached subtree aggregate, so exposing them publicly would silently corrupt fingerprints. The supported
// mutation path is `FingerprintTreeMap::with_mut`. A correct iterator-based design is future work.
pub use rsos::{
    Aggregate, Fingerprint, FingerprintTreeMap, IntoIter, IntoKeys, IntoValues, Iter, Keys, Rsos,
    Values,
};

pub use persistence::{FileSnapshot, InMemoryPersistence, PersistedState, Persistence};
pub use read_replica_map::ReadReplicaMap;
pub use replicated_map::ReplicatedMap;

/// Internal seam for the external integration tests (today: `tests/service.rs`).
///
/// The reconciliation mechanism modules are `pub(crate)` (ARCHITECTURE.md §3.7), but the
/// integration tests need to reach a handful of their internals. This module exposes exactly those
/// symbols so the default public surface stays clean while the tests can still reach them. It is
/// hidden from docs and only compiled under `cfg(test)` or the `internal-testing` feature
/// (integration tests are separate crates, so `cfg(test)` does not apply to them — they enable the
/// feature instead).
///
/// It deliberately carries only what stays `reconcile`-crate-internal. The diff mechanism itself
/// (`initial_ranges`/`protocol_round`/`RangeAggregate`/`EnumerationRange`) and the tree/fingerprint primitives are
/// genuinely `pub` on the standalone `rbsr` and `rsos` crates now, so the oracles import those
/// directly instead of routing through here.
#[doc(hidden)]
#[cfg(any(test, feature = "internal-testing"))]
pub mod testing {
    /// Seal `payload` with MAC authentication (not encryption): `tag(32) || seq(8 LE) || stamp(8
    /// LE) || payload`. Lets integration tests craft legitimate datagrams to exercise the
    /// anti-replay pipeline over a raw UDP socket.
    pub fn seal_datagram(key: [u8; 32], seq: u64, stamp: u64, payload: &[u8]) -> Vec<u8> {
        gossip::auth::Authenticator::new(Some(key), false)
            .seal(
                gossip::replay::Seq::new(seq),
                gossip::replay::Stamp::new(stamp),
                payload,
            )
            .expect("Enabled authenticator always seals")
    }

    /// Return the current membership set for integration-test assertions.
    ///
    /// Members are peers that have sent at least one dated, authenticated datagram and gate
    /// tombstone garbage collection via causal stability. Exposed so integration tests can
    /// assert that a decommissioned peer was not re-added to membership by a replayed datagram.
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
    ///
    /// Exposed for integration-test assertions so the peer-cap tests can verify that no new
    /// gossip-peer record is created for a capped-out sender.
    pub fn peers_map_len<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.peers_map_len()
    }

    /// Number of entries in the per-peer replay filter.
    ///
    /// Exposed for integration-test assertions so the peer-cap tests can verify that no new
    /// replay-filter entry is created for a capped-out sender.
    pub fn replay_filter_len<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.replay_filter_len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for integration-test assertions so tests can verify that acks for
    /// non-tombstone keys are dropped without growing bookkeeping.
    pub fn tombstone_acks_len<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.tombstone_acks_len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for integration-test assertions so tests can verify that the global dump
    /// budget is respected and slots are released after completion.
    pub fn bulk_dumps_in_flight_count<K, V>(store: &crate::ReplicatedMap<K, V>) -> usize
    where
        K: crate::bounds::Key + std::hash::Hash,
        V: crate::bounds::Value,
    {
        store.bulk_dumps_in_flight_count()
    }
}
