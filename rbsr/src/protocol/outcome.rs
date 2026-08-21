// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RoundOutcome`]'s accessors and its [`AddAssign`] for accumulating a whole reconciliation.

use std::ops::AddAssign;

use super::RoundOutcome;

impl RoundOutcome {
    /// Ranges resolved outright (SKIP): written to neither output.
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// Ranges handed back for explicit enumeration (IDLIST).
    pub const fn enumerated(&self) -> usize {
        self.enumerated
    }

    /// Ranges refined (SPLIT). This counts *parents*, not children — see
    /// [`children`](Self::children).
    pub const fn split(&self) -> usize {
        self.split
    }

    /// Child ranges written to `child_ranges` across every outcome — what travels in the next
    /// batch, and what a policy sees through
    /// [`Comparison::children_emitted`](crate::policy::Comparison::children_emitted).
    pub const fn children(&self) -> usize {
        self.children
    }

    /// Segments dropped without being answered because their bounds inverted once resolved against
    /// the local set. Non-zero means a peer is sending malformed input.
    pub const fn dropped_malformed(&self) -> usize {
        self.dropped_malformed
    }
}

impl AddAssign for RoundOutcome {
    fn add_assign(&mut self, other: RoundOutcome) {
        self.skipped += other.skipped;
        self.enumerated += other.enumerated;
        self.split += other.split;
        self.children += other.children;
        self.dropped_malformed += other.dropped_malformed;
    }
}
