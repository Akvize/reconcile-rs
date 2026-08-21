// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`Comparison`] construction and the read-only accessors [`RefinementPolicy`] is soundly allowed
//! to see (`ARCHITECTURE.md` §5's no-fingerprint-derived-decisions law).
//!
//! [`RefinementPolicy`]: super::RefinementPolicy

use super::Comparison;
use rsos::Aggregate;

impl Comparison {
    /// Build a comparison. Public so a policy can be unit-tested without a driver.
    pub const fn new(local: Aggregate, remote: Aggregate, children_emitted: usize) -> Comparison {
        Comparison {
            local,
            remote,
            children_emitted,
        }
    }

    /// `|X ∩ [l, u)|`: **local** elements covered — what `t` is compared against, and what a
    /// [`Decision::Split`](super::Decision::Split) cuts, since a split is by local rank.
    pub const fn span(&self) -> usize {
        self.local.size()
    }

    /// `|Y ∩ [l, u)|`: **remote** elements covered, as advertised. Unauthenticated peer input —
    /// readable, never to be assumed true.
    pub const fn remote_size(&self) -> usize {
        self.remote.size()
    }

    /// Whether the range is already resolved.
    ///
    /// Compares the **whole** aggregate, never the fingerprint alone (`ARCHITECTURE.md` §5
    /// invariant 3). Owned here so no policy can re-derive it wrongly.
    pub fn agrees(&self) -> bool {
        self.local == self.remote
    }

    /// Child ranges already emitted this round: the round-budget seam
    /// (`SOTA.md` §2.4 P3-9).
    ///
    /// Counted in ranges, not bytes — this crate owns no encoding. No shipped policy reads it;
    /// [`RefinementPolicy`](super::RefinementPolicy) carries a worked capping example.
    pub const fn children_emitted(&self) -> usize {
        self.children_emitted
    }
}
