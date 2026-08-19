// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `gossip`: the network adapter layer of `reconcile-rs`.
//!
//! **⚠ Implementation detail. Published as `reconcile-gossip`; depend on
//! [`reconcile`](https://crates.io/crates/reconcile)** — this crate is on the registry only
//! because cargo has no vendoring, and anything here may change or disappear in any release. Its
//! `pub` items (`auth::Authenticator`, `replay::Seq`/`Stamp`, the encoding functions) are `pub`
//! only to cross the crate boundary.
//!
//! - [`transport`] — the [`transport::Transport`] port plus its UDP and in-memory adapters.
//! - [`bincode`] — the wire encoding functions (not a port, `ARCHITECTURE.md` §3.2).
//! - [`auth`] — per-datagram MAC and optional AEAD over a shared cluster key, and `Payload`.
//! - [`replay`] — per-sender sequence and freshness state.
//! - [`discovery`] — the [`discovery::Discovery`] port and its adapters.
//! - [`gen_ip`] — random address generation within a set of networks.
//!
//! No dependency on `lww-register`: a datagram is a byte slice, a peer is an address
//! (`ARCHITECTURE.md` §2).

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod auth;
// Named after the external crate it wraps; `::bincode::…` disambiguates from this module.
pub mod bincode;
pub mod discovery;
pub mod gen_ip;
pub mod replay;
pub mod transport;

pub use discovery::{
    DiscoverFuture, Discovery, DiscoveryError, DiscoveryKind, DnsDiscovery, DnsDiscoveryError,
    RandomProbe,
};
pub use transport::{InMemoryNetwork, InMemoryTransport, Transport, UdpTransport};

// #297: re-exported so a public signature naming one of these types (`RandomProbe::new`'s
// `parking_lot`/`rand` parameters, `Config::nets`' `ipnet::IpNet`, `UdpTransport::new`/`socket`'s
// `tokio::net::UdpSocket`, `Transport`'s `#[async_trait]`) never forces a dependent onto an
// independently-versioned copy of the crate that type comes from — the version dependents see is
// the exact one this crate was built against. Not every dependency is re-exported this way: only
// ones that are genuinely part of a public contract, as opposed to `bincode`/
// `metrics-exporter-prometheus`, which are wrapped instead (`bincode.rs`, `reconcile`'s
// `prometheus.rs`) because their errors are an implementation choice, not something a caller of
// this crate should have to name.
pub use async_trait::async_trait;
pub use ipnet;
pub use parking_lot;
pub use rand;
pub use tokio;
