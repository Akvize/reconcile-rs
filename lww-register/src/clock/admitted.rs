// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`AdmittedTime`]'s drift-clamped admission: the only way to obtain one, guarding
//! [`super::Hlc::advance_past_remote`] against a far-future remote reading.

use super::{AdmittedTime, ClockDrift, PhysicalTime};

impl AdmittedTime {
    /// Admit an **untrusted** reading, clamping it to `local_now + max_drift`: the
    /// [`super::Clock::observe`] path. The remote's own [`super::Timestamp`] is untouched.
    ///
    /// [`was_clamped`](AdmittedTime::was_clamped) reports whether the cap fired.
    pub fn clamped_to_drift(
        remote: PhysicalTime,
        local_now: PhysicalTime,
        max_drift: ClockDrift,
    ) -> AdmittedTime {
        let cap = local_now.saturating_add(max_drift);
        if remote > cap {
            AdmittedTime {
                physical: cap,
                clamped: true,
            }
        } else {
            AdmittedTime {
                physical: remote,
                clamped: false,
            }
        }
    }

    /// Admit a reading **without** the clamp, on the caller's word.
    ///
    /// Correct for exactly one case: a stamp this node itself authored
    /// ([`super::Clock::observe_trusted`]). Anything off the network reopens the clock-poisoning
    /// hole.
    pub fn trusted(physical: PhysicalTime) -> AdmittedTime {
        AdmittedTime {
            physical,
            clamped: false,
        }
    }

    /// The admitted instant: the cap if the clamp fired, the original reading otherwise.
    pub fn physical(self) -> PhysicalTime {
        self.physical
    }

    /// Whether [`clamped_to_drift`](AdmittedTime::clamped_to_drift) actually replaced the
    /// reading with the cap. Always `false` for [`trusted`](AdmittedTime::trusted).
    pub fn was_clamped(self) -> bool {
        self.clamped
    }
}
