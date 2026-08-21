// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Construction and [`RefinementPolicy`](super::RefinementPolicy) for
//! [`FixedFanOut`](super::FixedFanOut), **the default policy**.

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, FanOut, FixedFanOut, RefinementPolicy, SplitStride};

impl FixedFanOut {
    /// A policy splitting into at most `fan_out` children per range.
    pub const fn new(fan_out: FanOut) -> FixedFanOut {
        FixedFanOut { fan_out }
    }

    /// The branching factor this policy splits at.
    pub const fn fan_out(&self) -> FanOut {
        self.fan_out
    }
}

impl Default for FixedFanOut {
    fn default() -> FixedFanOut {
        FixedFanOut::new(FanOut::NEGENTROPY)
    }
}

impl RefinementPolicy for FixedFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        Decision::Split(SplitStride::for_fan_out(comparison.span(), self.fan_out))
    }
}
