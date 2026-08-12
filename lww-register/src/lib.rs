// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `lww-register`: the state-based LWW-Register CRDT domain of `reconcile-rs`.
//!
//! **⚠ Implementation detail. Depend on [`reconcile`](https://crates.io/crates/reconcile)
//! instead** — published only because cargo has no vendoring, and anything here may change or
//! disappear in any release, patch included.
//!
//! - [`entry`] — the register cell, its tombstone state, and the LWW merge.
//! - [`clock`] — [`clock::Timestamp`], the [`clock::Clock`] port, the HLC arithmetic.
//! - [`persistence`] — the [`persistence::Persistence`] port and its in-memory backend.
//! - [`bounds`] — the [`bounds::Key`]/[`bounds::Value`] data-bound bundles.
//!
//! Infrastructure-free by construction: `ARCHITECTURE.md` §2.1, gated by
//! `scripts/check-domain-purity.sh`.

#![forbid(unsafe_code)]

pub mod bounds;
pub mod clock;
pub mod entry;
pub mod persistence;

pub use bounds::{Key, Value};
pub use clock::{Clock, Timestamp};
pub use entry::{Entry, State};
pub use persistence::{DatedEntries, InMemoryPersistence, PersistedState, Persistence};
