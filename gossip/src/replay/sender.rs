// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`SenderCounter`]'s monotonic sequence/stamp issuance.

use std::sync::atomic::{AtomicU64, Ordering};

use super::{phys_now_ms, SenderCounter, Seq, Stamp};

impl Default for SenderCounter {
    fn default() -> Self {
        SenderCounter::new()
    }
}

impl SenderCounter {
    /// A fresh counter: sequence numbers start at [`Seq::FIRST`], the stamp floor at 0.
    pub fn new() -> Self {
        SenderCounter {
            seq: AtomicU64::new(Seq::FIRST.0),
            stamp_floor: AtomicU64::new(0),
        }
    }

    /// Allocate the next sequence number (strictly increasing).
    pub fn next_seq(&self) -> Seq {
        Seq(self.seq.fetch_add(1, Ordering::Relaxed))
    }

    /// Mint a monotonically non-decreasing sender stamp, advancing the internal floor.
    pub fn next_stamp(&self) -> Stamp {
        self.next_stamp_at(phys_now_ms())
    }

    /// [`next_stamp`](Self::next_stamp) with an injectable `now_ms`.
    pub fn next_stamp_at(&self, now_ms: u64) -> Stamp {
        let prev = self.stamp_floor.fetch_max(now_ms, Ordering::Relaxed);
        Stamp(prev.max(now_ms))
    }
}
