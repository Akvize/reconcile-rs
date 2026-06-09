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

//! # Security model
//!
//! By default the UDP reconciliation protocol is **unauthenticated**: any host able to send a
//! datagram to the port can forge an update and poison the whole cluster through last-write-wins.
//! To prevent this, configure a shared cluster secret with
//! [`Config::with_cluster_key`](reconcile_store::Config::with_cluster_key) on **every** node: this
//! enables a per-datagram keyed MAC that is verified before deserialization, silently dropping
//! unauthenticated or forged datagrams. See the README "Security model" section for the full
//! threat model and scope.

pub mod bounds;
pub mod fingerprint;
pub mod hlc;
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
pub(crate) mod diff;
pub(crate) mod gen_ip;
pub(crate) mod hrtree_iter;
pub(crate) mod observability;
pub(crate) mod reconcile_engine;
pub(crate) mod timeout_wheel;

pub use bounds::{Key, Value};
pub use fingerprint::Fingerprint;
pub use hlc::Timestamp;
pub use hrtree::HRTree;
// The `hrtree_iter` module is `pub(crate)`, but these iterator types appear in public `HRTree`
// method return types, so they must stay publicly reachable. A `pub` type re-exported from a
// `pub(crate)` module is publicly reachable, which avoids private-in-public errors (E0446).
pub use hrtree_iter::{IntoIter, IntoKeys, IntoValues, Iter, IterMut, Keys, Values, ValuesMut};
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
    pub use crate::diff::{DiffRange, Diffable, HashRangeQueryable, HashSegment};
    pub use crate::fingerprint::hash;
}
