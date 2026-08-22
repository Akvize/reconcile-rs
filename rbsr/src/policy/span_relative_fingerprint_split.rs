// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RefinementPolicy`](super::RefinementPolicy) for
//! [`SpanRelativeFingerprintSplit`](super::SpanRelativeFingerprintSplit) —
//! `cfg(reconcile_internal_testing)`-gated, see the type's own docs for what it isolates.

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, RefinementPolicy, SplitStride};

/// **Test-only probe (#356), `cfg(reconcile_internal_testing)`-gated.** Oracle-coupled *and*
/// span-relative: `1 + fingerprint.low_limb mod (span − 1)`, so every SPLIT emits at least two
/// children.
///
/// The cell [`FingerprintDerivedSplit`](super::FingerprintDerivedSplit) leaves empty. It reads the
/// same oracle — the *choice of cut point* is fingerprint-determined, so the index set an execution
/// compares is still correlated with the digest the collision probability is stated over — while
/// keeping the progress property every shipped policy has. Drives under it terminate, so the
/// soundness question can be measured on a full-size population rather than a censored one. Never
/// a shipped policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpanRelativeFingerprintSplit;

impl RefinementPolicy for SpanRelativeFingerprintSplit {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        // Reads the same oracle `FingerprintDerivedSplit` does, but reduced mod the span, so the
        // stride lands in `1..span-1` and every SPLIT emits at least two children.
        // `shared_cutoffs` has already returned for `span <= 1`, so `span - 1 >= 1` here.
        let stride = 1 + comparison.local.fingerprint().0[0] as usize % (comparison.span() - 1);
        Decision::Split(SplitStride::per_child(stride))
    }
}
