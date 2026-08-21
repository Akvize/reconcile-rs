// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Construction and [`RefinementPolicy`](super::RefinementPolicy) for
//! [`EnumerateBelowThreshold`](super::EnumerateBelowThreshold) — Algorithm 1 of
//! arXiv:2603.19820 as written, with its own enumeration cutoff (`|X ∩ [l, u)| ≤ t`) rather than
//! the `shared_cutoffs` the other two shipped policies use.

use super::{Comparison, Decision, EnumerateBelowThreshold, FanOut, RefinementPolicy, SplitStride};

impl EnumerateBelowThreshold {
    /// arXiv:2603.19820 §6's experimental parameters: `t = 32`, `b = 16`.
    pub const PAPER: EnumerateBelowThreshold = EnumerateBelowThreshold {
        threshold: 32,
        fan_out: FanOut::NEGENTROPY,
    };

    /// Enumerate ranges of at most `threshold` local elements, split the rest into at most
    /// `fan_out` children. `0` is raised to `1`.
    pub const fn new(threshold: usize, fan_out: FanOut) -> EnumerateBelowThreshold {
        EnumerateBelowThreshold {
            threshold: if threshold == 0 { 1 } else { threshold },
            fan_out,
        }
    }

    /// `t`: the largest local subset this policy enumerates rather than splits. Never zero.
    pub const fn threshold(&self) -> usize {
        self.threshold
    }

    /// `b`: the branching factor this policy splits at.
    pub const fn fan_out(&self) -> FanOut {
        self.fan_out
    }
}

impl Default for EnumerateBelowThreshold {
    fn default() -> EnumerateBelowThreshold {
        EnumerateBelowThreshold::PAPER
    }
}

impl RefinementPolicy for EnumerateBelowThreshold {
    fn decide(&self, comparison: Comparison) -> Decision {
        if comparison.agrees() {
            // SKIP: `f_X = f_Y`, on the whole aggregate rather than the fingerprint alone.
            Decision::Skip
        } else if comparison.span() <= self.threshold {
            // IDLIST: `|X ∩ [l, u)| ≤ t`, which subsumes `shared_cutoffs` since `t ≥ 1`.
            Decision::Enumerate
        } else {
            // SPLIT: `span > t ≥ 1`, so the stride is below the span and really refines.
            Decision::Split(SplitStride::for_fan_out(comparison.span(), self.fan_out))
        }
    }
}
