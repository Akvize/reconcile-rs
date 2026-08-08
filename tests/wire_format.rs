// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Golden vector for the RBSR segment wire encoding.
//!
//! This lives in `reconcile` rather than in `rbsr` on purpose. `rbsr` owns the *type* that goes on
//! the wire; it owns no *encoding* — the codec is chosen here, in the adapter layer
//! (`gossip::bincode`), and `bincode` is a real dependency of this package and of nothing below it.
//! Putting the byte-level check where the codec already lives is what keeps a codec dependency out
//! of `rbsr` entirely, rather than admitting one and then carving an exception for it.
//!
//! The segment is built through `RangeAggregate::for_testing`, the `internal-testing` seam, because
//! chosen bounds are the whole point: `initial_ranges` only ever emits `(Unbounded, Unbounded)`, so a
//! vector built from it would never exercise the `Included`/`Excluded` encodings — nor catch a
//! reordering of `StartBound`/`EndBound`'s variants, which bincode writes positionally.

#![cfg(feature = "internal-testing")]

use bincode::{DefaultOptions, Deserializer, Serializer};
use serde::{Deserialize, Serialize};

use rbsr::RangeAggregate;
use rsos::{Aggregate, Fingerprint};

/// `RangeAggregate`'s encoding must be **byte-for-byte** what the pre-`Aggregate` layout
/// `{range, hash: Fingerprint, size: usize}` produced, so a node running this code and a node
/// running the previous release reconcile without either noticing.
///
/// The bytes below were captured from that earlier layout, under the same `DefaultOptions`
/// configuration the wire codec uses, before the two fields were collapsed into one nested
/// `Aggregate`. bincode writes struct fields sequentially with no framing and no field names, so
/// the nested struct is inlined and the encoding is unchanged — *provided* `Aggregate` declares
/// `fingerprint` before `size`. Reordering those two declarations breaks this test, which is
/// exactly the point (see `rsos::Aggregate`'s own note).
///
/// Reading the vector: `1` = the `StartBound::Included` discriminant, `7` = the start key; `1` =
/// the `EndBound::Excluded` discriminant, `42` = the end key; then the four `u64` fingerprint
/// limbs as bincode varints; then `251, 44, 1` = the varint for `size == 300`.
#[test]
fn wire_format_is_unchanged_by_the_aggregate_collapse() {
    const GOLDEN: &[u8] = &[
        1, 7, 1, 42, 253, 239, 205, 171, 137, 103, 69, 35, 1, 253, 16, 50, 84, 118, 152, 186, 220,
        254, 1, 2, 251, 44, 1,
    ];

    let segment = RangeAggregate::for_testing(
        Some(7u32),
        Some(42u32),
        Aggregate::new(
            300,
            Fingerprint([0x0123456789abcdef, 0xfedcba9876543210, 1, 2]),
        ),
    );

    let mut buf = Vec::new();
    segment
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    assert_eq!(
        buf, GOLDEN,
        "RangeAggregate's wire encoding changed — this is a protocol break, not a refactor"
    );

    let mut deserializer = Deserializer::from_slice(GOLDEN, DefaultOptions::new());
    let decoded = RangeAggregate::<u32>::deserialize(&mut deserializer).unwrap();
    assert_eq!(decoded, segment);
}
