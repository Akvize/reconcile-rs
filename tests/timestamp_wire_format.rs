// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Golden vectors pinning [`Timestamp`]'s encodings.
//!
//! `Timestamp` goes on the wire inside every `Entry`, and its canonical encoding feeds every
//! element fingerprint — so its byte layout is protocol, not implementation. Two changes to the
//! *type* are meant to be *purely* compile-time, leaving those bytes alone:
//!
//! * wrapping its components in newtypes (`PhysicalTime`/`LogicalCounter`/`NodeId`) — serde encodes
//!   a newtype struct as its inner value alone; and
//! * grouping the two clock components into a nested [`Hlc`], so the shape is
//!   `{{physical, logical}, node_id}` rather than a flat `{physical, logical, node_id}` — both
//!   `bincode` and `rsos`'s canonical encoding write a struct as its fields in declaration order
//!   with no framing and no length prefix, so an inlined nested struct is byte-for-byte the
//!   flattening of itself. (This is the same property `RangeAggregate`/`Aggregate` already relies
//!   on.)
//!
//! "Meant to be" is not evidence, hence this file: the constants below were captured before either
//! change and have not moved since.
//!
//! It lives in `reconcile` rather than in `lww-register` for the same reason `wire_format.rs`
//! does: `lww-register` owns the *type*, not the *encoding*. `bincode` is a legitimate dependency
//! here (`gossip::bincode` is where the codec is chosen) and must never become one of the domain
//! crate.

use bincode::{DefaultOptions, Deserializer, Serializer};
use serde::{Deserialize, Serialize};

use reconcile::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use reconcile::entry::Entry;
use rsos::{lift, Fingerprint};

/// The timestamp both vectors below are taken from: distinct, non-trivial components, so a
/// transposition changes the bytes.
fn sample_stamp() -> Timestamp {
    Timestamp::new(
        Hlc::new(
            PhysicalTime::from_millis(0x0123_4567_89ab_cdef),
            LogicalCounter::new(0x1122_3344),
        ),
        NodeId::new(0xfeed_face_dead_beef),
    )
}

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    value
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    buf
}

/// `Timestamp`'s bincode encoding under the wire codec's `DefaultOptions`: `physical`, `logical`
/// and `node_id` as back-to-back varints, unchanged by the newtypes or the `Hlc` nesting.
#[test]
fn timestamp_bincode_encoding_is_unchanged() {
    const GOLDEN: &[u8] = &[
        253, 239, 205, 171, 137, 103, 69, 35, 1, 252, 68, 51, 34, 17, 253, 239, 190, 173, 222, 206,
        250, 237, 254,
    ];

    let stamp = sample_stamp();
    assert_eq!(
        encode(&stamp),
        GOLDEN,
        "Timestamp's wire encoding changed — this is a protocol break, not a refactor"
    );

    let mut deserializer = Deserializer::from_slice(GOLDEN, DefaultOptions::new());
    assert_eq!(Timestamp::deserialize(&mut deserializer).unwrap(), stamp);
}

/// The same, for a `Timestamp` in the position it actually occupies on the wire: inside an
/// `Entry`, ahead of the `State` discriminant and the value.
#[test]
fn entry_bincode_encoding_is_unchanged() {
    const GOLDEN: &[u8] = &[
        253, 239, 205, 171, 137, 103, 69, 35, 1, 252, 68, 51, 34, 17, 253, 239, 190, 173, 222, 206,
        250, 237, 254, 0, 251, 57, 48,
    ];

    let entry = Entry::present(sample_stamp(), 12345u32);
    assert_eq!(
        encode(&entry),
        GOLDEN,
        "Entry's wire encoding changed — this is a protocol break, not a refactor"
    );

    let mut deserializer = Deserializer::from_slice(GOLDEN, DefaultOptions::new());
    assert_eq!(
        Entry::<Timestamp, u32>::deserialize(&mut deserializer).unwrap(),
        entry
    );
}

/// The reconciliation token itself: the element fingerprint `rsos` derives from an `Entry`
/// through its canonical encoding. Two nodes must agree on this value forever, so a change here
/// is a cluster-wide divergence, not a refactor.
#[test]
fn entry_fingerprint_is_unchanged() {
    const GOLDEN: Fingerprint = Fingerprint([
        0xbaa4_af17_b48b_79a7,
        0x7ac3_63a5_54df_3f18,
        0x6d7e_6ffe_ace1_e413,
        0xec7f_5fad_ea96_0d52,
    ]);

    let entry = Entry::present(sample_stamp(), 12345u32);
    assert_eq!(
        lift(&7u32, &entry),
        GOLDEN,
        "the fingerprint of an Entry changed — replicas on either side of this change \
         would never converge"
    );
}
