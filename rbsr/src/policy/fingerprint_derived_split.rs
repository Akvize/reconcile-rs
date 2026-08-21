// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RefinementPolicy`](super::RefinementPolicy) for
//! [`FingerprintDerivedSplit`](super::FingerprintDerivedSplit) — `cfg(reconcile_internal_testing)`-gated, see the
//! type's own docs for why it exists and why it must never ship.

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, FingerprintDerivedSplit, RefinementPolicy, SplitStride};

impl RefinementPolicy for FingerprintDerivedSplit {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        // (A5)-violating on purpose (#356): the stride comes from `local`'s fingerprint, the same
        // oracle the skip rule's per-comparison collision probability is stated over.
        let stride = 1 + comparison.local.fingerprint().0[0] as usize % 32;
        Decision::Split(SplitStride::per_child(stride))
    }
}
