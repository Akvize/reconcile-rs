// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::ops::RangeBounds;

use crate::bounds::{Key, Value};
use crate::clock::{NodeId, Timestamp};
use rsos::Fingerprint;

use super::Replica;

impl<K: Key + Hash, V: Value> Replica<K, V> {
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.map.read().aggregate(range).fingerprint()
    }

    /// Fingerprint of the value-only [`projection`](Self::projection) over a range.
    ///
    /// This is the timestamp-less counterpart of [`fingerprint`](Self::fingerprint); a dateless
    /// read replica that has converged with this store computes the same value over the same range.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.projection.read().aggregate(range).fingerprint()
    }

    /// Mint a fresh Hybrid Logical Clock timestamp for a local write.
    pub fn clock_now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Advance the clock past `stamp` using the trusted path — for stamps this node itself
    /// authored (e.g. restored from its own persisted state). Unlike the remote-peer path,
    /// this does not apply the far-future clamp, so the clock reliably chases its own output
    /// even after a backward wall-clock step (NTP correction, VM resume).
    pub(crate) fn clock_observe_trusted(&self, stamp: Timestamp) {
        self.clock.observe_trusted(stamp);
    }

    /// Returns `true` when the node id was generated at random (`Config::node_id` was `None`).
    pub(crate) fn node_id_is_random(&self) -> bool {
        self.node_id_is_random
    }

    /// This node's Hybrid-Logical-Clock identity, read back from the clock adapter so it can never
    /// disagree with the `node_id` actually stamped onto minted timestamps.
    pub(crate) fn node_id(&self) -> NodeId {
        self.clock.node_id()
    }
}
