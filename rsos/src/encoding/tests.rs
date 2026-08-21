// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::collections::{BTreeMap, HashMap};

use serde::ser;

use super::*;

fn enc<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
    encode_to_vec(value).unwrap()
}

#[test]
fn integers_are_fixed_width_little_endian() {
    assert_eq!(enc(&1i8), vec![1]);
    assert_eq!(enc(&1u8), vec![1]);
    assert_eq!(enc(&1i16), 1i16.to_le_bytes().to_vec());
    assert_eq!(enc(&1u16), vec![1, 0]);
    assert_eq!(enc(&1u32), vec![1, 0, 0, 0]);
    assert_eq!(enc(&1i64), 1i64.to_le_bytes().to_vec());
    assert_eq!(enc(&1u64), vec![1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(enc(&1i128), 1i128.to_le_bytes().to_vec());
    assert_eq!(enc(&1u128), 1u128.to_le_bytes().to_vec());
    assert_eq!(enc(&(-1i32)), vec![0xff; 4]);
    assert_eq!(enc(&1usize), enc(&1u64));
    assert_eq!(enc(&(-1isize)), enc(&(-1i64)));
}

#[test]
fn bools_encode_as_a_single_byte() {
    assert_eq!(enc(&true), vec![1]);
    assert_eq!(enc(&false), vec![0]);
}

/// The wire format is fixed and self-describing, never adapted to a human-facing target (e.g.
/// JSON) — a mutant flipping `is_human_readable` to `true` would still round-trip through
/// `serde`'s own derives, but silently changes what `#[serde(with = ...)]`-style adapters do.
#[test]
fn serializer_reports_binary_not_human_readable() {
    struct ChecksHumanReadable;
    impl Serialize for ChecksHumanReadable {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            assert!(!s.is_human_readable());
            s.serialize_unit()
        }
    }
    enc(&ChecksHumanReadable);
}

#[test]
fn strings_are_length_prefixed() {
    let mut expected = 2u64.to_le_bytes().to_vec();
    expected.extend_from_slice(b"ab");
    assert_eq!(enc("ab"), expected);
}

#[test]
fn strings_are_unambiguously_framed() {
    assert_ne!(enc(&("ab", "c")), enc(&("a", "bc")));
}

#[test]
fn options_are_tagged() {
    assert_eq!(enc(&None::<u8>), vec![0]);
    assert_eq!(enc(&Some(0u8)), vec![1, 0]);
    assert_ne!(enc(&None::<u8>), enc(&Some(0u8)));
}

#[test]
fn unit_and_newtype_add_no_framing() {
    #[derive(Serialize)]
    struct Unit;
    #[derive(Serialize)]
    struct Newtype(u32);

    assert_eq!(enc(&()), Vec::<u8>::new());
    assert_eq!(enc(&Unit), Vec::<u8>::new());
    assert_eq!(enc(&Newtype(7)), enc(&7u32));
}

#[test]
fn floats_use_raw_bits() {
    assert_eq!(enc(&1.5f64), 1.5f64.to_bits().to_le_bytes().to_vec());
    assert_eq!(enc(&1.5f32), 1.5f32.to_bits().to_le_bytes().to_vec());
    assert_ne!(enc(&0.0f64), enc(&(-0.0f64)));
    assert_eq!(enc(&f64::NAN), enc(&f64::NAN));
}

#[test]
fn chars_encode_as_u32() {
    assert_eq!(enc(&'é'), enc(&(u32::from('é'))));
}

#[test]
fn seq_of_unknown_length_still_writes_its_count_first() {
    // No size hint: exercises the buffered arm.
    struct Unsized(Vec<u8>);
    impl Serialize for Unsized {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(self.0.iter().filter(|_| true))
        }
    }
    assert_eq!(enc(&Unsized(vec![1, 2, 3])), enc(&vec![1u8, 2, 3]));
}

#[test]
fn map_is_sorted_by_encoded_key_and_order_independent() {
    let mut a = HashMap::new();
    a.insert("one", 1u32);
    a.insert("two", 2u32);
    a.insert("three", 3u32);

    let mut b = HashMap::new();
    b.insert("three", 3u32);
    b.insert("one", 1u32);
    b.insert("two", 2u32);

    assert_eq!(enc(&a), enc(&b));

    let ordered: BTreeMap<&str, u32> = a.iter().map(|(k, v)| (*k, *v)).collect();
    assert_eq!(enc(&a), enc(&ordered));
}

/// A no-op `end` would still pass the two tests above (both sides would encode as equally
/// empty), so pin the actual on-wire content directly: a length prefix, then each entry's
/// encoded key and value, in sorted-key order.
#[test]
fn map_encodes_its_length_and_every_entry() {
    let mut m = BTreeMap::new();
    m.insert(1u32, 10u32);
    m.insert(2u32, 20u32);

    let mut expected = 2u64.to_le_bytes().to_vec();
    expected.extend(enc(&1u32));
    expected.extend(enc(&10u32));
    expected.extend(enc(&2u32));
    expected.extend(enc(&20u32));
    assert_eq!(enc(&m), expected);
}

#[test]
fn structs_encode_fields_in_declaration_order_without_names() {
    #[derive(Serialize)]
    struct Pair {
        a: u32,
        b: u32,
    }
    let mut expected = enc(&1u32);
    expected.extend(enc(&2u32));
    assert_eq!(enc(&Pair { a: 1, b: 2 }), expected);
}

#[test]
fn enum_variants_carry_their_index() {
    #[derive(Serialize)]
    enum E {
        A(u32),
        B(u32),
    }
    assert_eq!(
        enc(&E::A(1)),
        [0u32.to_le_bytes(), 1u32.to_le_bytes()].concat()
    );
    assert_ne!(enc(&E::A(1)), enc(&E::B(1)));
}

/// A fieldless variant is `serialize_unit_variant`, not `serialize_newtype_variant` (the case
/// [`enum_variants_carry_their_index`] above exercises) — its only payload is the index.
#[test]
fn unit_variants_carry_their_index() {
    #[derive(Serialize)]
    enum U {
        A,
        B,
    }
    assert_eq!(enc(&U::A), 0u32.to_le_bytes().to_vec());
    assert_eq!(enc(&U::B), 1u32.to_le_bytes().to_vec());
    assert_ne!(enc(&U::A), enc(&U::B));
}

/// `serde` treats a one-field unnamed struct/variant as newtype (already covered above) and a
/// two-or-more-field one as a genuine tuple struct/variant, routed through a different `ser`
/// trait (`SerializeTupleStruct`/`SerializeTupleVariant`) and a different impl (`Streaming`,
/// not the entry-point `Serializer` itself) — exercise that path directly so a no-op
/// `serialize_field` there is caught, not silently covered by the newtype case.
#[test]
fn multi_field_tuple_structs_and_variants_encode_every_field() {
    #[derive(Serialize)]
    struct Pair(u32, u32);
    assert_eq!(
        enc(&Pair(1, 2)),
        [2u64.to_le_bytes().to_vec(), enc(&1u32), enc(&2u32)].concat()
    );

    #[derive(Serialize)]
    enum TV {
        A(u32, u32),
    }
    assert_eq!(
        enc(&TV::A(1, 2)),
        [
            0u32.to_le_bytes().to_vec(),
            2u64.to_le_bytes().to_vec(),
            enc(&1u32),
            enc(&2u32)
        ]
        .concat()
    );
}

/// A named-field enum variant is a struct variant regardless of field count (unlike a tuple
/// struct/variant, one field is already enough) — its own `ser` trait/impl pair, distinct from
/// both the newtype-variant and tuple-variant cases above.
#[test]
fn struct_variants_encode_their_index_and_fields() {
    #[derive(Serialize)]
    enum SV {
        A { x: u32 },
    }
    assert_eq!(
        enc(&SV::A { x: 5 }),
        [0u32.to_le_bytes().to_vec(), enc(&5u32)].concat()
    );
}

#[test]
fn nested_sequences_are_unambiguous() {
    assert_ne!(enc(&vec![vec![1u8, 2]]), enc(&vec![vec![1u8], vec![2u8]]));
}

#[test]
fn a_refusing_serialize_impl_surfaces_as_an_error() {
    struct Refuses;
    impl Serialize for Refuses {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
            Err(ser::Error::custom("nope"))
        }
    }
    let err = encode_to_vec(&Refuses).unwrap_err();
    assert!(err.to_string().contains("nope"));
}
