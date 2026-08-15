// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `rsos::FingerprintTreeMap::range`'s return type must be nameable through the `reconcile`
//! facade, without a direct `rsos` dependency (#291).

use reconcile::{FingerprintTreeMap, ItemRange};

/// Proves `ItemRange` can be named as a struct field / return type via `reconcile::ItemRange`,
/// not only reachable anonymously through `impl Trait` or turbofish.
struct Holder<'a> {
    range: ItemRange<'a, u64, u64, std::ops::Range<u64>>,
}

#[test]
fn item_range_is_nameable_and_stores_in_a_typed_binding() {
    let tree: FingerprintTreeMap<u64, u64> = (0..20).map(|k| (k, k * 2)).collect();

    let holder = Holder {
        range: tree.range(5..15),
    };

    assert_eq!(holder.range.len(), 10);
    assert_eq!(
        holder.range.clone().map(|(k, _)| *k).collect::<Vec<_>>(),
        (5..15).collect::<Vec<u64>>()
    );
    let debugged = format!("{:?}", tree.range(5..15));
    assert!(debugged.contains('5'), "Debug output was: {debugged}");
}
