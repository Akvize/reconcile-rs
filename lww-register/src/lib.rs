// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `lww-register`: the state-based LWW-Register CRDT domain of `reconcile-rs`.
//!
//! # ⚠ Implementation detail — no stability guarantee
//!
//! **Do not depend on this crate directly — depend on
//! [`reconcile`](https://crates.io/crates/reconcile),** which re-exports everything here that is
//! meant for consumers (`reconcile::Entry`, `reconcile::Timestamp`, `reconcile::Persistence`, …).
//! It is on crates.io for one reason: cargo has no vendoring, so `reconcile` cannot be published
//! unless every crate it depends on is published too. That is the same reason `serde_derive`,
//! `pin-project-internal` and `tracing-attributes` are on the registry, and it carries the same
//! warning: **anything here may change or disappear in any release**, including in a patch
//! release, without a deprecation period and without appearing in `reconcile`'s changelog. It is
//! not offered as a general-purpose LWW-Register; it is shaped entirely by what `reconcile` needs.
//!
//! # What it holds
//!
//! A *last-write-wins register* is the simplest useful state-based CRDT: a cell holding one value
//! plus the timestamp it was written at, merged by keeping the entry with the greater timestamp.
//! Because the merge is a join on a total order, it is commutative, associative and idempotent —
//! so replicas that have seen the same set of writes, in any order, hold the same value. A
//! replicated map is then just a keyed collection of such registers.
//!
//! This crate holds that domain and nothing else:
//!
//! - [`entry`] — [`entry::Entry`]/[`entry::State`]: the register cell itself, its tombstone-aware
//!   state, and the LWW merge rule.
//! - [`clock`] — [`clock::Timestamp`] (the hybrid logical stamp the merge orders on), the
//!   [`clock::Clock`] port, and the HLC ordering arithmetic. The one physical-time read lives in an
//!   adapter behind that port, outside this crate.
//! - [`persistence`] — the [`persistence::Persistence`] port, the
//!   [`persistence::PersistedState`] snapshot type, and the in-memory default backend. The
//!   file-backed adapter (`FileSnapshot`) lives outside this crate, in `reconcile`.
//! - [`bounds`] — the [`bounds::Key`]/[`bounds::Value`] data-bound bundles every generic signature
//!   in the workspace is written against.
//!
//! # Infrastructure-free, structurally
//!
//! Nothing here reads a clock, opens a socket, touches a filesystem, or picks a wire format: the
//! only dependency is `serde`'s derive, for data shapes that *other* crates encode. That is the
//! hexagonal boundary of `ARCHITECTURE.md` §2.2/§3.3, and it is enforced twice over — by this
//! crate's `Cargo.toml` (which cannot name `tokio`/`bincode`/`chrono`/`ipnet`) and by
//! `scripts/check-domain-purity.sh`, which greps these sources for such imports in case one is
//! reached through a re-export.

// The entire crate is implemented in safe Rust; this turns any `unsafe` block into a hard
// compile error.
#![forbid(unsafe_code)]

pub mod bounds;
pub mod clock;
pub mod entry;
pub mod persistence;

pub use bounds::{Key, Value};
pub use clock::{Clock, Timestamp};
pub use entry::{Entry, State};
pub use persistence::{DatedEntries, InMemoryPersistence, PersistedState, Persistence};
