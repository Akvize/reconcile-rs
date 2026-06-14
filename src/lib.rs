// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This crate provides a key-data map structure [`HRTree`] that can be used together with the
//! reconciliation [`ReconcileStore`]. Different instances can talk together over UDP to efficiently
//! reconcile their differences.

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
//! [`Config::with_cluster_key`](reconcile_store::Config::with_cluster_key) on **every** node: this
//! enables per-datagram MAC authentication and per-sender replay protection. Every datagram carries
//! a monotonically increasing sequence number and a sender wall-clock stamp (both inside the
//! authenticated region); the receiver maintains per-peer state and rejects duplicates, stale
//! out-of-window sequences, and datagrams whose freshness stamp deviates from local physical time
//! by more than the configured freshness window (default 5 minutes). See the README "Security
//! model" section for the full threat model and scope.

// The entire crate is implemented in safe Rust; this turns any `unsafe` block into a hard
// compile error.
#![forbid(unsafe_code)]

pub mod bounds;
pub mod clock;
pub mod discovery;
pub mod fingerprint;
pub mod hrtree;
pub mod mirror;
pub mod persistence;
pub mod reconcilable;
pub mod reconcile_store;

/// Optional Prometheus integration (enabled by the `metrics-prometheus` feature).
#[cfg(feature = "metrics-prometheus")]
pub mod prometheus;

pub(crate) mod auth;
// Internal reconciliation mechanism. Demoted to `pub(crate)` (ARCHITECTURE.md §3.7): these are
// implementation details, not part of the supported public surface. The few internals the
// integration-test oracles need are re-exported through the gated [`testing`] module below.
pub(crate) mod gen_ip;
pub(crate) mod hrtree_iter;
pub(crate) mod observability;
pub(crate) mod proto;
pub(crate) mod reconcile_engine;
pub(crate) mod replay;
pub(crate) mod timeout_wheel;

pub use bounds::{Key, Value};
pub use clock::{Clock, Timestamp};
pub use discovery::{DiscoverFuture, Discovery, DnsDiscovery, RandomProbe};
pub use fingerprint::Fingerprint;
pub use hrtree::HRTree;
// The `hrtree_iter` module is `pub(crate)`, but the iterator types below appear in public `HRTree`
// method return types, so they must stay publicly reachable. A `pub` type re-exported from a
// `pub(crate)` module is publicly reachable, which avoids private-in-public errors (E0446).
// `IterMut` and `ValuesMut` are intentionally omitted (and are themselves `#[cfg(test)]`-only in
// `hrtree_iter`): they hand out `&mut V` without updating per-element hashes or the cumulative
// `tree_hash`, so exposing them publicly would silently corrupt fingerprints. The supported
// mutation path is `HRTree::with_mut`. A correct iterator-based design is future work.
pub use hrtree_iter::{IntoIter, IntoKeys, IntoValues, Iter, Keys, Values};
pub use mirror::ReconcileMirror;
pub use persistence::{FileSnapshot, InMemoryPersistence, PersistedState, Persistence};
pub use reconcilable::{Projectable, ValueOnly};
pub use reconcile_store::ReconcileStore;

/// Internal seam for the external integration-test oracles (`tests/diff.rs`,
/// `tests/proptest_hrtree.rs`).
///
/// The reconciliation mechanism modules are `pub(crate)` (ARCHITECTURE.md §3.7), but the
/// integration tests need to reach a handful of their internals to drive the diff protocol. This
/// module re-exports exactly those symbols so the default public surface stays clean while the
/// tests can still reach them. It is hidden from docs and only compiled under `cfg(test)` or the
/// `internal-testing` feature (integration tests are separate crates, so `cfg(test)` does not
/// apply to them — they enable the feature instead).
#[doc(hidden)]
#[cfg(any(test, feature = "internal-testing"))]
pub mod testing {
    pub use crate::fingerprint::hash;
    pub use crate::proto::{diff_round, start_diff, DiffRange, HashSegment};

    /// Range fingerprint of an [`HRTree`](crate::HRTree), exposed for the integration-test
    /// oracles. The inherent `HRTree::hash` is `pub(crate)` (reconciliation mechanism), so the
    /// external test crates reach it through this gated seam rather than the public surface.
    pub fn range_hash<K, V, R>(tree: &crate::HRTree<K, V>, range: &R) -> crate::Fingerprint
    where
        K: std::hash::Hash + Ord,
        V: std::hash::Hash,
        R: std::ops::RangeBounds<K>,
    {
        tree.hash(range)
    }

    /// Seal a raw payload with the given 32-byte cluster key, sequence number, and wall-clock stamp
    /// using MAC authentication (not encryption), producing the on-wire datagram bytes.
    ///
    /// Exposed so integration tests can craft legitimately-sealed datagrams and inject them via a
    /// raw UDP socket to exercise the anti-replay pipeline end-to-end.  The output format matches
    /// what the engine sends in authenticated (non-encrypted) mode:
    /// `tag(32) || seq(8 LE) || stamp(8 LE) || payload`.
    pub fn seal_datagram(key: [u8; 32], seq: u64, stamp: u64, payload: &[u8]) -> Vec<u8> {
        crate::auth::Authenticator::new(Some(key), false)
            .seal(seq, stamp, payload)
            .expect("Enabled authenticator always seals")
    }

    /// Return the current membership set for integration-test assertions.
    ///
    /// Members are peers that have sent at least one dated, authenticated datagram and gate
    /// tombstone garbage collection via causal stability. Exposed so integration tests can
    /// assert that a decommissioned peer was not re-added to membership by a replayed datagram.
    pub fn members_snapshot<K, V>(
        store: &crate::ReconcileStore<K, V>,
    ) -> std::collections::HashSet<std::net::IpAddr>
    where
        K: crate::bounds::Key,
        V: crate::bounds::Value,
    {
        store.members_snapshot()
    }
}
