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
//! The segment is built through `RangeAggregate::new`, because chosen bounds are the whole point:
//! `initial_ranges` only ever emits `(Unbounded, Unbounded)`, so a vector built from it would never
//! exercise the `Included`/`Excluded` encodings — nor catch a reordering of `StartBound`/
//! `EndBound`'s variants, which bincode writes positionally.

#![cfg(reconcile_internal_testing)]

use bincode::{DefaultOptions, Deserializer, Serializer};
use serde::{Deserialize, Serialize};

use rbsr::RangeAggregate;
use rsos::{Aggregate, Fingerprint};

/// `RangeAggregate`'s golden encoding, under the wire codec's `DefaultOptions`.
///
/// bincode inlines the nested `Aggregate` in declaration order, so these bytes hold only while
/// `Aggregate` declares `fingerprint` before `size` — reordering breaks this test, which is the
/// point.
///
/// Reading the vector: `1` = `StartBound::Included`, `7` = start key; `1` = `EndBound::Excluded`,
/// `42` = end key; four `u64` fingerprint limbs as varints; `251, 44, 1` = `size == 300`.
#[test]
fn wire_format_is_unchanged_by_the_aggregate_collapse() {
    const GOLDEN: &[u8] = &[
        1, 7, 1, 42, 253, 239, 205, 171, 137, 103, 69, 35, 1, 253, 16, 50, 84, 118, 152, 186, 220,
        254, 1, 2, 251, 44, 1,
    ];

    let segment = RangeAggregate::new(
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

/// Golden vector for the **envelope** `gossip::auth::Authenticator::seal` produces — the wire
/// version byte's placement, not the `Message`/`RangeAggregate` body above. Both
/// authenticated and unauthenticated layouts are pinned: the version byte is present, and at the
/// same relative position (right after the replay header, ahead of the payload) in both.
#[test]
fn envelope_pins_the_wire_version_byte() {
    use gossip::auth::{Authenticator, ClusterKey, KEY_LEN, TAG_LEN};
    use gossip::replay::{Seq, Stamp, REPLAY_HEADER_LEN};

    let payload = b"payload";

    // Disabled: `version || payload`, no header, no tag.
    let disabled = Authenticator::new(None, false).seal(Seq::new(1), Stamp::new(1), payload);
    let mut expected_disabled = vec![gossip::auth::WIRE_VERSION];
    expected_disabled.extend_from_slice(payload);
    assert_eq!(
        disabled, expected_disabled,
        "unauthenticated envelope's wire-version placement changed — this is a protocol break"
    );

    // Enabled (MAC): `tag(32) || seq(8 LE) || stamp(8 LE) || version(1) || payload`. The tag
    // itself is keyed and non-deterministic across implementations only in the sense that this
    // vector pins BLAKE3 (the default backend) specifically — a `mac-hmac` build produces a
    // different tag over the identical protected region, which is exactly what this vector does
    // NOT need to pin: only the plaintext framing (position of seq/stamp/version/payload) is the
    // wire contract; the tag is opaque by design.
    let sealed = Authenticator::new(Some(ClusterKey::new([0x42; KEY_LEN])), false).seal(
        Seq::new(0x0102030405060708),
        Stamp::new(0x1112131415161718),
        payload,
    );
    assert_eq!(
        sealed.len(),
        TAG_LEN + REPLAY_HEADER_LEN + 1 + payload.len()
    );
    let (tag, protected) = sealed.split_at(TAG_LEN);
    assert_ne!(tag, [0u8; TAG_LEN], "sanity: a real tag was written");
    assert_eq!(
        &protected[..8],
        &0x0102030405060708u64.to_le_bytes(),
        "seq must be the first 8 bytes of the protected region, little-endian"
    );
    assert_eq!(
        &protected[8..16],
        &0x1112131415161718u64.to_le_bytes(),
        "stamp must be the next 8 bytes of the protected region, little-endian"
    );
    assert_eq!(
        protected[16],
        gossip::auth::WIRE_VERSION,
        "the wire-version byte must sit immediately after the replay header"
    );
    assert_eq!(
        &protected[17..],
        payload,
        "the payload must immediately follow the wire-version byte"
    );
}
