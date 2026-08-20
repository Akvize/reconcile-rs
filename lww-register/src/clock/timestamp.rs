// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`Timestamp`]: pairing an [`Hlc`] reading with the [`NodeId`] tie-break that mints it. The
//! ordering itself is the struct's derived `Ord` (field order, in `super::clock`) -- this file
//! only assembles and delegates.

use super::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};

impl Timestamp {
    /// Pair a clock reading with the identity of the node minting it.
    ///
    /// For tests and for reconstruction from external storage; normal code goes through the
    /// store's clock.
    pub const fn new(hlc: Hlc, node_id: NodeId) -> Timestamp {
        Timestamp { hlc, node_id }
    }

    /// The clock reading this stamp carries.
    pub const fn hlc(&self) -> Hlc {
        self.hlc
    }

    /// The physical time this stamp sits in — [`hlc`](Timestamp::hlc)'s, delegated.
    pub const fn physical(&self) -> PhysicalTime {
        self.hlc.physical()
    }

    /// The logical counter within that millisecond — [`hlc`](Timestamp::hlc)'s, delegated.
    pub const fn logical(&self) -> LogicalCounter {
        self.hlc.logical()
    }

    /// Identity of the node that minted this timestamp.
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }
}
