// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Durability for a [`ReplicatedMap`](crate::ReplicatedMap): the re-export surface.
//!
//! Persistence was split across two crates by the workspace split (ARCHITECTURE.md §3.9), along the
//! line between contract and adapter:
//!
//! - [`lww_register::persistence`] holds the [`Persistence`] port, the [`PersistedState`] snapshot
//!   type, and [`InMemoryPersistence`] — the non-durable default, which sits next to the trait it
//!   trivially implements so that the default backend costs no extra crate. Nothing there touches a
//!   filesystem or a wire codec.
//! - The [`snapshot`] crate holds [`FileSnapshot`], the durable adapter, together with the
//!   versioned on-disk header that makes a stale-format snapshot a clean load error rather than a
//!   silent misread.
//!
//! Everything is re-exported here so `reconcile::persistence::*` keeps resolving exactly as before.
//! See [`lww_register::persistence`] for what is persisted and why it matters for tombstone
//! resurrection.

pub use lww_register::persistence::{
    DatedEntries, InMemoryPersistence, PersistedState, Persistence,
};
pub use snapshot::FileSnapshot;
