// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`Hlc`]'s tick/advance arithmetic: Kulkarni et al. 2014's HLC update rule, the core this
//! whole module exists for.

use super::{AdmittedTime, Hlc, LogicalCounter, PhysicalTime};

impl Hlc {
    /// The reading a cold clock starts from: the Unix epoch, counter zero.
    pub const START: Hlc = Hlc {
        physical: PhysicalTime::EPOCH,
        logical: LogicalCounter::ZERO,
    };

    /// Build a reading from its two components.
    pub const fn new(physical: PhysicalTime, logical: LogicalCounter) -> Hlc {
        Hlc { physical, logical }
    }

    /// The physical time this reading sits in.
    pub const fn physical(&self) -> PhysicalTime {
        self.physical
    }

    /// The logical counter within that millisecond.
    pub const fn logical(&self) -> LogicalCounter {
        self.logical
    }

    /// This reading advanced by one logical tick, rolling `physical` forward on counter overflow.
    ///
    /// Strictly monotonic: `(physical + 1, 0) > (physical, u32::MAX)`, and saturating, so
    /// `u64::MAX` cannot wrap. Sole owner of the roll rule.
    #[must_use]
    pub fn next_tick(self) -> Hlc {
        match self.logical.checked_next() {
            Some(logical) => Hlc {
                physical: self.physical,
                logical,
            },
            None => Hlc {
                physical: self.physical.next_ms(),
                logical: LogicalCounter::ZERO,
            },
        }
    }

    /// The HLC "observe" arithmetic: move `self` strictly past both `phys_now` and a remote
    /// reading, ending at or above `max(self, phys_now, admitted remote)`.
    ///
    /// `remote_logical` comes from the original remote stamp, never affected by a clamp.
    pub fn advance_past_remote(
        &mut self,
        phys_now: PhysicalTime,
        remote_physical: AdmittedTime,
        remote_logical: LogicalCounter,
    ) {
        let remote_physical = remote_physical.physical();
        let max_physical = phys_now.max(self.physical).max(remote_physical);

        let base_logical = if max_physical == self.physical && max_physical == remote_physical {
            self.logical.max(remote_logical)
        } else if max_physical == self.physical {
            self.logical
        } else if max_physical == remote_physical {
            remote_logical
        } else {
            *self = Hlc {
                physical: max_physical,
                logical: LogicalCounter::ZERO,
            };
            return;
        };

        *self = Hlc {
            physical: max_physical,
            logical: base_logical,
        }
        .next_tick();
    }
}
