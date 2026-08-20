// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Construction and accessors for the plain newtypes: [`ClockDrift`], [`PhysicalTime`],
//! [`LogicalCounter`], [`NodeId`]. No cross-type arithmetic lives here -- that is
//! [`super::hlc`]/[`super::admitted`]'s job.

use super::{ClockDrift, LogicalCounter, NodeId, PhysicalTime};

impl ClockDrift {
    /// A drift budget of `ms` milliseconds.
    pub const fn from_millis(ms: u64) -> ClockDrift {
        ClockDrift(ms)
    }

    /// The budget, in milliseconds.
    pub const fn millis(self) -> u64 {
        self.0
    }
}

impl PhysicalTime {
    /// The Unix epoch itself — the instant a cold clock starts from.
    pub const EPOCH: PhysicalTime = PhysicalTime(0);

    /// An instant `ms` milliseconds after the Unix epoch.
    pub const fn from_millis(ms: u64) -> PhysicalTime {
        PhysicalTime(ms)
    }

    /// This instant, in milliseconds since the Unix epoch.
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// This instant moved forward by `drift`, saturating: the budget may be arbitrarily large.
    #[must_use]
    pub const fn saturating_add(self, drift: ClockDrift) -> PhysicalTime {
        PhysicalTime(self.0.saturating_add(drift.0))
    }

    /// The next millisecond, saturating at `u64::MAX`.
    #[must_use]
    pub const fn next_ms(self) -> PhysicalTime {
        PhysicalTime(self.0.saturating_add(1))
    }
}

impl LogicalCounter {
    /// The counter a fresh physical-time bucket starts at.
    pub const ZERO: LogicalCounter = LogicalCounter(0);

    /// A counter holding `value`.
    pub const fn new(value: u32) -> LogicalCounter {
        LogicalCounter(value)
    }

    /// The raw counter value.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The next counter, or `None` on `u32::MAX` — the signal for [`super::Hlc::next_tick`]'s
    /// physical-time roll. Crate-private so no caller can rebuild that rule.
    #[must_use]
    pub(crate) const fn checked_next(self) -> Option<LogicalCounter> {
        match self.0.checked_add(1) {
            Some(c) => Some(LogicalCounter(c)),
            None => None,
        }
    }
}

impl NodeId {
    /// The replica identified by `id`.
    pub const fn new(id: u64) -> NodeId {
        NodeId(id)
    }

    /// The raw identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}
