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
use super::{Comparison, Decision, RefinementPolicy, SplitStride};

/// **Test-only probe (#356), `cfg(reconcile_internal_testing)`-gated.** Deliberately violates the law
/// [`Comparison`]'s docs state: it derives its split stride from the **local** aggregate's
/// fingerprint instead of from the range alone, reintroducing the oracle dependence rank-cut
/// refinement exists to avoid — the index set this produces is no longer a deterministic function
/// of the data alone.
///
/// Since #352, `Comparison`'s public API carries no accessor returning a fingerprint, so this
/// cannot be built from outside the crate; it exists only under `cfg(reconcile_internal_testing)`, so a
/// measurement harness in `rbsr/tests/` can still reach it. Never a shipped policy — see this
/// crate's `tests/oracle_dependent_split_vs_the_union_bound.rs` for what it measures.
///
/// Enumeration cutoffs match [`FixedFanOut`](super::FixedFanOut)'s (`cutoffs::shared_cutoffs`), so
/// the only variable this isolates is *how the split stride is chosen*, never *when* a range is
/// enumerated instead of split.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FingerprintDerivedSplit;

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
