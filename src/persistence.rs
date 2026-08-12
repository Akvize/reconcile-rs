// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Durability for a [`ReplicatedMap`](crate::ReplicatedMap), re-exported so
//! `reconcile::persistence::*` resolves unchanged.
//!
//! The [`Persistence`] port, [`PersistedState`] and [`InMemoryPersistence`] live in
//! [`lww_register::persistence`]; [`FileSnapshot`], the durable adapter with its versioned on-disk
//! header, lives in `crate::snapshot` (`ARCHITECTURE.md` §2).

pub use crate::snapshot::FileSnapshot;
pub use lww_register::persistence::{
    DatedEntries, InMemoryPersistence, PersistedState, Persistence,
};
