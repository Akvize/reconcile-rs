// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `gossip`: the network adapter layer of `reconcile-rs`.
//!
//! # ⚠ Implementation detail — no stability guarantee
//!
//! **Published as `reconcile-gossip`. Do not depend on this crate directly — depend on
//! [`reconcile`](https://crates.io/crates/reconcile).** It is on crates.io for one reason: cargo
//! has no vendoring, so `reconcile` cannot be published unless every crate it depends on is
//! published too. That is the same reason `serde_derive`, `pin-project-internal` and
//! `tracing-attributes` are on the registry, and it carries the same warning: **anything here may
//! change or disappear in any release**, including in a patch release, without a deprecation
//! period and without appearing in `reconcile`'s changelog. The API this crate is versioned
//! against is `reconcile`'s, not its own.
//!
//! (The published name is a registry detail only: `gossip` was taken, so the manifest renames the
//! package and every dependent renames it straight back with
//! `gossip = { package = "reconcile-gossip", … }`. All Rust source, here and in `reconcile`, says
//! `gossip`.)
//!
//! # What it holds
//!
//! Everything a replica needs to *talk* to its peers, and nothing about what it says:
//!
//! - [`transport`] — the [`transport::Transport`] port (`send_to`/`recv_from` over datagrams) plus
//!   its two implementations: [`transport::UdpTransport`] for real sockets and
//!   [`transport::InMemoryTransport`]/[`transport::InMemoryNetwork`] for deterministic,
//!   socket-free tests.
//! - [`bincode`] — the crate's wire encoding functions. Named after the codec they wrap, since
//!   there is no abstraction left to name (see the module docs for why this is not a port).
//! - [`auth`] — per-datagram MAC authentication and optional AEAD encryption over a shared cluster
//!   key, and the `Payload` type that can only be obtained by verifying one.
//! - [`replay`] — per-sender sequence/freshness state, the anti-replay half of the same envelope.
//! - [`discovery`] — the [`discovery::Discovery`] port and its
//!   [`discovery::RandomProbe`]/[`discovery::DnsDiscovery`] adapters.
//! - [`gen_ip`] — random address generation within a set of networks, which `RandomProbe` draws
//!   its probe targets from.
//!
//! # No dependency on the domain
//!
//! This crate does **not** depend on `lww-register`: nothing here knows what an `Entry`, a
//! `Timestamp` or a `Key` is. A datagram is a byte slice; a peer is an address. That is the same
//! rationale `ARCHITECTURE.md` gives for defining `Transport` outside the core — it is a
//! core-independent sibling, not a layer above it, and the `reconcile` facade is the one place the
//! two meet.
//!
//! # Visibility
//!
//! Several items here (`auth::Authenticator`, `replay::Seq`/`Stamp`, the encoding functions) are
//! `pub` only so the `reconcile` facade can reach them across the crate boundary — they were
//! `pub(crate)` while everything lived in one crate. They are implementation detail, not supported
//! API, and `reconcile` deliberately does not re-export them.

// The entire crate is implemented in safe Rust; this turns any `unsafe` block into a hard
// compile error.
#![forbid(unsafe_code)]

pub mod auth;
// `bincode.rs` holds the wire-encoding functions, not a port: unlike `Transport`
// (`ARCHITECTURE.md` §3.4/§3.5) there is a single implementation and no plausible swap (compression
// interacts with authenticate-before-decode; cross-language interop needs a published wire spec,
// not a Rust trait) — see the module doc comment for the full reasoning. Named after the external
// `bincode` crate it wraps, since there is no abstraction left to name; references to the crate
// itself from inside (or near) this module use `::bincode::…` to disambiguate.
pub mod bincode;
pub mod discovery;
pub mod gen_ip;
pub mod replay;
pub mod transport;

pub use discovery::{DiscoverFuture, Discovery, DiscoveryKind, DnsDiscovery, RandomProbe};
pub use transport::{InMemoryNetwork, InMemoryTransport, Transport, UdpTransport};
