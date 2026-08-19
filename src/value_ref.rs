// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`ValueRef`]: the read-locked handle [`ReplicatedMap::get`](crate::ReplicatedMap::get) and
//! [`ReadReplicaMap::get`](crate::ReadReplicaMap::get) return.

use std::ops::Deref;

use parking_lot::MappedRwLockReadGuard;

/// A read-locked reference to a live value.
///
/// Opaque wrapper over this crate's internal lock guard (#297): naming the concrete
/// `parking_lot` guard type in a public signature would force every dependent onto this crate's
/// exact `parking_lot` version. Derefs to `&V`; holding one across a write on the same handle
/// deadlocks — see [`ReplicatedMap::get`](crate::ReplicatedMap::get)'s docs for the pattern to
/// avoid and [`ReplicatedMap::get_cloned`](crate::ReplicatedMap::get_cloned) for the safe default.
pub struct ValueRef<'a, V>(pub(crate) MappedRwLockReadGuard<'a, V>);

impl<V> Deref for ValueRef<'_, V> {
    type Target = V;

    fn deref(&self) -> &V {
        &self.0
    }
}
