// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Property 4: adversarial RSOS backends.
//!
//! `RsosView` is public, so a backend's answers are untrusted input (ARCHITECTURE.md §5 inv. 9).
//! `rbsr/src/protocol.rs` carries the worked example; this is the property behind it.
//!
//! The whole module is gated on `reconcile_internal_testing` (only `RsosView` needs the seam, and
//! that cfg is off by default), so the gate sits at the file level rather than per-item — an
//! `#[cfg]` on each item individually would leave the imports below unused, and thus warn, whenever
//! the cfg is off.
#![cfg(reconcile_internal_testing)]

use proptest::prelude::*;
use rbsr::{protocol_round, RangeAggregate};
use rsos::Fingerprint;

/// `rank` supplied by the test rather than derived from contents, so it can point anywhere —
/// including past `size()`, which `RsosView`'s rank-within-store law forbids. `select` indexes a
/// `Vec`, so it panics
/// out of bounds: the trap the driver must not spring.
struct HostileRanks {
    keys: Vec<u64>,
    rank_answers: Vec<usize>,
}

impl rbsr::RsosView<u64> for HostileRanks {
    fn size(&self) -> usize {
        self.keys.len()
    }

    fn aggregate<R: std::ops::RangeBounds<u64>>(&self, _range: R) -> rsos::Aggregate {
        // Non-empty and constant, so it never matches what the segments below advertise: the
        // driver always reaches SPLIT, the branch that indexes.
        rsos::Aggregate::new(self.keys.len(), Fingerprint([7, 0, 0, 0]))
    }

    fn rank(&self, z: &u64) -> usize {
        // Deterministic per key — the driver ranks both bounds and compares the two answers — but
        // otherwise unconstrained, and free to exceed `size()`.
        self.rank_answers[(*z as usize) % self.rank_answers.len()]
    }

    fn select(&self, r: usize) -> &u64 {
        &self.keys[r]
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// No backend answer, and no peer segment, may panic the driver. Rank answers range well past
    /// any plausible `size()`, so the bound is exercised rather than merely present; the tally
    /// check catches a "fix" that drops such segments instead.
    #[test]
    fn no_backend_answer_can_drive_the_protocol_out_of_bounds(
        keys in prop::collection::vec(0u64..64, 1..24),
        rank_answers in prop::collection::vec(0usize..4096, 1..16),
        start in prop::option::of(0u64..64),
        end in prop::option::of(0u64..64),
    ) {
        let mut keys = keys;
        keys.sort_unstable();
        keys.dedup();
        let store = HostileRanks { keys, rank_answers };

        let segment = RangeAggregate::new(
            start,
            end,
            rsos::Aggregate::new(1, Fingerprint([1, 0, 0, 0])),
        );

        let mut child_ranges = Vec::new();
        let mut enumeration_ranges = Vec::new();
        let outcome = protocol_round(
            &store,
            vec![segment],
            &mut child_ranges,
            &mut enumeration_ranges,
        );

        prop_assert_eq!(child_ranges.len(), outcome.children());
        prop_assert_eq!(enumeration_ranges.len(), outcome.enumerated());
    }
}
