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

pub mod diff;
pub mod fingerprint;
pub mod gen_ip;
pub mod hlc;
pub mod hrtree;
pub mod hrtree_iter;
pub mod reconcile_store;

pub(crate) mod auth;
pub(crate) mod reconcilable;
pub(crate) mod reconcile_engine;
pub(crate) mod timeout_wheel;

pub use fingerprint::Fingerprint;
pub use hlc::Hlc;
pub use hrtree::HRTree;
pub use reconcile_store::ReconcileStore;
