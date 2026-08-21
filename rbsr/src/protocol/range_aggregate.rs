// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`RangeAggregate`]'s own construction (including the subspace-reconciliation entry point,
//! [`RangeAggregate::new`]) and its three field readers.

use std::ops::{Bound, RangeBounds};

use rsos::Aggregate;

use super::{EndBound, KeyRange, RangeAggregate, StartBound};

impl<K> RangeAggregate<K> {
    /// Build a `RangeAggregate` over a bounded starting family — the entry point for **subspace**
    /// (prefix/partial) reconciliation: seed [`protocol_round`](super::protocol_round) with one or
    /// more of these instead of [`initial_ranges`](super::initial_ranges)' single `(−∞, +∞)`
    /// range, and only the given key interval is compared.
    ///
    /// `None` is unbounded, `Some(k)` is `Included(k)` on the start and `Excluded(k)` on the end —
    /// an excluded start or included end stays unspellable, matching what
    /// [`initial_ranges`](super::initial_ranges) and [`protocol_round`](super::protocol_round)
    /// themselves can ever produce.
    pub fn new(start: Option<K>, end: Option<K>, aggregate: Aggregate) -> Self {
        RangeAggregate {
            range: KeyRange::new(
                start.map_or(StartBound::Unbounded, StartBound::Included),
                end.map_or(EndBound::Unbounded, EndBound::Excluded),
            ),
            aggregate,
        }
    }

    /// The lower bound of this range, `Unbounded` or `Included`.
    pub fn start_bound(&self) -> Bound<&K> {
        RangeBounds::start_bound(&self.range)
    }

    /// The upper bound of this range, `Unbounded` or `Excluded`.
    pub fn end_bound(&self) -> Bound<&K> {
        RangeBounds::end_bound(&self.range)
    }

    /// The [`Aggregate`] (fingerprint + size) this peer computed over the range.
    pub fn aggregate(&self) -> &Aggregate {
        &self.aggregate
    }
}
