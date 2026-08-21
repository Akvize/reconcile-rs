// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`shared_cutoffs`]: the enumeration cutoffs [`SqrtFanOut`](super::SqrtFanOut),
//! [`FixedFanOut`](super::FixedFanOut) and, under `cfg(reconcile_internal_testing)`,
//! [`FingerprintDerivedSplit`](super::FingerprintDerivedSplit) all share — private, since it is a
//! shared implementation detail of those policies' `decide`, never a seam of its own.

use super::{Comparison, Decision, SplitStride};

/// The cutoffs [`SqrtFanOut`](super::SqrtFanOut) and [`FixedFanOut`](super::FixedFanOut) share,
/// where a cut by rank cannot help: the peer holds nothing, we hold nothing, or both hold exactly
/// one element. `None` when the fan-out actually matters.
/// [`EnumerateBelowThreshold`](super::EnumerateBelowThreshold) reaches the same outcomes through
/// `t`.
pub(super) fn shared_cutoffs(comparison: Comparison) -> Option<Decision> {
    let local = comparison.span();
    let remote = comparison.remote_size();
    if comparison.agrees() {
        Some(Decision::Skip)
    } else if remote == 0 {
        Some(Decision::Enumerate)
    } else if local == 0 {
        Some(Decision::Split(SplitStride::ONE))
    } else if local == 1 && remote == 1 {
        Some(Decision::Enumerate)
    } else if local == 1 {
        // One local element cannot be cut by rank: let the peer, which holds more, cut.
        Some(Decision::Split(SplitStride::ONE))
    } else {
        None
    }
}
