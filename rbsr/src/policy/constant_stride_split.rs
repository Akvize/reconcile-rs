// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RefinementPolicy`](super::RefinementPolicy) for
//! [`ConstantStrideSplit`](super::ConstantStrideSplit) — `cfg(reconcile_internal_testing)`-gated,
//! see the type's own docs for what it controls for.

use super::cutoffs::shared_cutoffs;
use super::{Comparison, Decision, RefinementPolicy, SplitStride};

/// **Test-only probe (#356), `cfg(reconcile_internal_testing)`-gated.** A fixed stride for every
/// range, however wide.
///
/// A constant is trivially "a function of the data alone", so this satisfies the
/// oracle-independence [`Comparison`]'s law is about, and cannot be accused of reading the digest.
/// It is nonetheless *not* progress-making once a range's span falls to `stride` or below, which is
/// the point: it is the control deciding whether
/// [`FingerprintDerivedSplit`](super::FingerprintDerivedSplit)'s failure is caused by the oracle
/// coupling or by the span-independent magnitude that came with it. Never a shipped policy — see
/// `tests/joint_progress_and_the_oracle_coupling_confound.rs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConstantStrideSplit(SplitStride);

impl ConstantStrideSplit {
    /// A policy cutting every `elements` keys regardless of the span. `0` is raised to `1`.
    pub const fn per_child(elements: usize) -> ConstantStrideSplit {
        ConstantStrideSplit(SplitStride::per_child(elements))
    }

    /// The constant stride this policy cuts at.
    pub const fn stride(&self) -> SplitStride {
        self.0
    }
}

impl RefinementPolicy for ConstantStrideSplit {
    fn decide(&self, comparison: Comparison) -> Decision {
        if let Some(decision) = shared_cutoffs(comparison) {
            return decision;
        }
        Decision::Split(self.0)
    }
}
