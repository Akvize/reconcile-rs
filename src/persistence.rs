// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Durability for a [`ReplicatedMap`](crate::ReplicatedMap): the port, and the file adapter.
//!
//! Persistence is split along the line between contract and adapter (ARCHITECTURE.md §3.9), but
//! only one of the two halves left this package:
//!
//! - [`lww_register::persistence`] holds the [`Persistence`] port, the [`PersistedState`] snapshot
//!   type, and [`InMemoryPersistence`] — the non-durable default, which sits next to the trait it
//!   trivially implements so that the default backend costs no extra crate. Nothing there touches a
//!   filesystem or a wire codec, which is exactly why [`FileSnapshot`] could not stay with it.
//! - `crate::snapshot` holds [`FileSnapshot`], the durable adapter, together with the versioned
//!   on-disk header that makes a stale-format snapshot a clean load error rather than a silent
//!   misread. It is a module of this package, not a crate of its own.
//!
//! Everything is re-exported here so `reconcile::persistence::*` keeps resolving exactly as before.
//! See [`lww_register::persistence`] for what is persisted and why it matters for tombstone
//! resurrection.

pub use crate::snapshot::FileSnapshot;
pub use lww_register::persistence::{
    DatedEntries, InMemoryPersistence, PersistedState, Persistence,
};
