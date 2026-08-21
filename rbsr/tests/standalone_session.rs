// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! End-to-end sessions built entirely from `rbsr`'s public API: no `reconcile_internal_testing`, no
//! `reconcile`. #289's acceptance criteria — a stable public constructor plus accessors on
//! [`RangeAggregate`], and a subspace session that could not have been written before them.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use rbsr::{initial_ranges, protocol_round, EnumerationRange, RangeAggregate, RsosView};
use rsos::{FingerprintTreeMap, Rsos};

/// Alternate rounds between `responder`/`advertiser` until nothing is left, starting from
/// `active`, collecting every [`EnumerationRange`] either side reported (a caller would enumerate
/// each and exchange the contents; this single-process test diffs `a`/`b` directly instead).
fn converge<'a>(
    mut active: Vec<RangeAggregate<u32>>,
    mut responder: &'a FingerprintTreeMap<u32, u32>,
    mut advertiser: &'a FingerprintTreeMap<u32, u32>,
) -> Vec<EnumerationRange<u32>> {
    let mut ranges = Vec::new();
    while !active.is_empty() {
        let mut children = Vec::new();
        let mut enumerations = Vec::new();
        protocol_round(responder, active, &mut children, &mut enumerations);
        ranges.append(&mut enumerations);
        active = children;
        std::mem::swap(&mut responder, &mut advertiser);
    }
    ranges
}

/// The keys where `a` and `b` disagree (present in only one, or present in both with different
/// values), restricted to what a set of [`EnumerationRange`]s actually covers.
fn differing_keys_in(
    ranges: &[EnumerationRange<u32>],
    a: &FingerprintTreeMap<u32, u32>,
    b: &FingerprintTreeMap<u32, u32>,
) -> BTreeSet<u32> {
    let mut differing = BTreeSet::new();
    for range in ranges {
        let a_items: BTreeSet<(u32, u32)> = a.enumerate(*range).map(|(k, v)| (*k, *v)).collect();
        let b_items: BTreeSet<(u32, u32)> = b.enumerate(*range).map(|(k, v)| (*k, *v)).collect();
        differing.extend(a_items.symmetric_difference(&b_items).map(|(k, _)| *k));
    }
    differing
}

/// The crate-root doc example's loop, over two full stores that start 50 keys apart —
/// no `reconcile_internal_testing`, no `reconcile`: `rbsr` + `rsos` alone.
#[test]
fn a_full_session_finds_every_difference() {
    let mut a = FingerprintTreeMap::new();
    let mut b = FingerprintTreeMap::new();
    for i in 0..200u32 {
        a.insert(i, i);
        b.insert(i, i);
    }
    for i in 200..250u32 {
        a.insert(i, i); // only `a` has these
    }

    let active = initial_ranges(&a);
    let ranges = converge(active, &b, &a);
    let differing = differing_keys_in(&ranges, &a, &b);

    assert_eq!(differing, (200..250u32).collect());
}

/// Subspace (prefix) reconciliation: seed the session with a `RangeAggregate` built through the
/// public [`RangeAggregate::new`] constructor over a bounded interval, rather than
/// [`initial_ranges`]' unbounded `(−∞, +∞)`. Only divergence *inside* that interval is found;
/// divergence outside it is invisible to this session by construction.
#[test]
fn a_bounded_starting_family_reconciles_only_its_subspace() {
    let mut a = FingerprintTreeMap::new();
    let mut b = FingerprintTreeMap::new();
    for i in 0..400u32 {
        a.insert(i, i);
        b.insert(i, i);
    }
    // A difference inside [100, 200) ...
    a.insert(150, 999);
    // ... and one outside it, which the bounded session below must never see.
    a.insert(350, 999);

    let start = 100u32;
    let end = 200u32;
    let subspace_aggregate = RsosView::aggregate(&a, start..end);
    let seed = RangeAggregate::new(Some(start), Some(end), subspace_aggregate);

    // The constructor round-trips through the accessors #289 asks for.
    assert_eq!(seed.start_bound(), std::ops::Bound::Included(&start));
    assert_eq!(seed.end_bound(), std::ops::Bound::Excluded(&end));
    assert_eq!(seed.aggregate(), &subspace_aggregate);

    let ranges = converge(vec![seed], &b, &a);
    let differing = differing_keys_in(&ranges, &a, &b);

    // Only the in-subspace difference (key 150) surfaces; key 350 was never in scope.
    assert_eq!(differing, BTreeSet::from([150]));
}
