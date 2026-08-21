// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RefinementPolicy`](super::RefinementPolicy) for [`SqrtFanOut`](super::SqrtFanOut).

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, RefinementPolicy, SplitStride, SqrtFanOut};

impl RefinementPolicy for SqrtFanOut {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        // `f32` and truncation are part of the rule: past f32's mantissa `f64` would disagree.
        let stride = (comparison.span() as f32).sqrt() as usize;
        Decision::Split(SplitStride::per_child(stride))
    }
}
