// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`PeerState`]: the per-peer accept/restart decision (module docs' "Restart beats regression"
//! and "Post-restart tail guard" rules).

use super::bitmap::SlidingBitmap;
use super::{Seq, Stamp, WINDOW_SIZE};

/// The per-peer replay state.
#[derive(Debug)]
pub(super) struct PeerState {
    /// Highest sequence number accepted from this peer.
    max_seq: Seq,
    /// Sender stamp on the datagram carrying `max_seq`; restart detection compares against it.
    stamp_at_max: Stamp,
    /// Monotone high-water mark of accepted sender stamps. Never rewound by `reset` — the
    /// post-restart tail guard.
    max_stamp_seen: Stamp,
    /// Sliding out-of-order acceptance window, relative to `max_seq`.
    bitmap: SlidingBitmap,
}

impl PeerState {
    pub(super) fn new(first_seq: Seq, first_stamp: Stamp) -> Self {
        PeerState {
            max_seq: first_seq,
            stamp_at_max: first_stamp,
            max_stamp_seen: first_stamp,
            bitmap: SlidingBitmap::new(),
        }
    }

    /// The sender stamp at the highest accepted sequence number.
    pub(super) fn stamp_at_max(&self) -> Stamp {
        self.stamp_at_max
    }

    /// Whether the datagram is fresh, updating the bitmap and high-water marks if it is.
    pub(super) fn accept(&mut self, seq: Seq, stamp: Stamp) -> bool {
        if seq > self.max_seq {
            // Post-restart tail guard; strict `<` so same-millisecond bursts still pass.
            if stamp < self.max_stamp_seen {
                return false;
            }
            let delta = seq - self.max_seq;
            self.bitmap.advance(delta);
            self.max_seq = seq;
            self.stamp_at_max = stamp;
            self.max_stamp_seen = self.max_stamp_seen.max(stamp);
            true
        } else {
            // A strictly newer stamp at or below `max_seq` is a restart, not a regression.
            if stamp > self.stamp_at_max {
                self.reset(seq, stamp);
                return true;
            }
            let behind = self.max_seq - seq;
            if behind >= WINDOW_SIZE {
                return false;
            }
            if self.bitmap.is_marked(behind) {
                return false;
            }
            self.bitmap.mark(behind);
            true
        }
    }

    /// Reset state for a restarted sender. `max_stamp_seen` is never rewound — it is the tail
    /// guard.
    fn reset(&mut self, new_seq: Seq, new_stamp: Stamp) {
        self.max_seq = new_seq;
        self.stamp_at_max = new_stamp;
        self.max_stamp_seen = self.max_stamp_seen.max(new_stamp);
        self.bitmap = SlidingBitmap::new();
    }
}
