// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`Seq`]/[`Stamp`] wire encoding, ordering and freshness — the two replay-header fields'
//! type-owned validation (AGENTS.md §4).

use std::fmt;
use std::ops::Sub;
use std::time::Duration;

use super::{phys_now_ms, Seq, Stamp};

impl Seq {
    /// The first sequence number a [`super::SenderCounter`] issues in authenticated mode.
    pub const FIRST: Seq = Seq(1);
    /// Sentinel carried on a [`crate::auth::Payload`] produced in unauthenticated mode, where no
    /// replay header exists.
    pub const NONE: Seq = Seq(0);

    /// Wrap a raw sequence number.
    #[allow(dead_code)] // used by cfg(test) unit tests and the `reconcile_internal_testing` cfg seam
    pub const fn new(value: u64) -> Seq {
        Seq(value)
    }

    /// Encode as the 8-byte little-endian replay-header representation.
    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Decode from the 8-byte little-endian replay-header representation.
    pub fn from_le_bytes(bytes: [u8; 8]) -> Seq {
        Seq(u64::from_le_bytes(bytes))
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Number of sequence numbers between two `Seq`s. Precondition: `self >= rhs`.
impl Sub for Seq {
    type Output = u64;

    fn sub(self, rhs: Seq) -> u64 {
        self.0 - rhs.0
    }
}

impl Stamp {
    /// Sentinel carried on a [`crate::auth::Payload`] produced in unauthenticated mode, where no
    /// replay header exists.
    pub const NONE: Stamp = Stamp(0);

    /// Wrap a raw millisecond-since-epoch value.
    #[allow(dead_code)] // used by cfg(test) unit tests and the `reconcile_internal_testing` cfg seam
    pub const fn new(value: u64) -> Stamp {
        Stamp(value)
    }

    /// Encode as the 8-byte little-endian replay-header representation.
    pub fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Decode from the 8-byte little-endian replay-header representation.
    pub fn from_le_bytes(bytes: [u8; 8]) -> Stamp {
        Stamp(u64::from_le_bytes(bytes))
    }

    /// Read local physical time as a `Stamp` (ms since the Unix epoch, clamped to non-negative).
    pub(super) fn now() -> Stamp {
        Stamp(phys_now_ms())
    }

    /// Whether `self` deviates from `now` by no more than `window` in either direction.
    pub(super) fn is_fresh(self, now: Stamp, window: Duration) -> bool {
        let window_ms = window.as_millis() as u64;
        self.age_relative_to(now) <= window_ms && now.age_relative_to(self) <= window_ms
    }

    /// How far behind `now` this stamp is, saturating at 0 if `self` is not older than `now`.
    pub(super) fn age_relative_to(self, now: Stamp) -> u64 {
        now.0.saturating_sub(self.0)
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
