// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The **canonical encoding**: the byte source for element fingerprints.
//!
//! # Why this module exists
//!
//! A [`Fingerprint`](crate::Fingerprint) is a wire token: two nodes must derive the *same* 256-bit
//! value from the same element, forever, across Rust versions, platforms (32- vs 64-bit) and
//! endianness. Pinning the *hash function* (BLAKE3) only pins half of that. The other half is the
//! byte sequence fed into it.
//!
//! `rsos` used to take those bytes from [`std::hash::Hash`], feeding a `Hasher` adapter that
//! carefully wrote fixed little-endian integers. That was not enough: the adapter pins how the
//! bytes are *written*, not which bytes std's `Hash` impls *choose to write*. Rust explicitly does
//! not promise that `Hash for str`, `Hash for Option<T>` or any other impl keeps emitting the same
//! byte sequence across releases — so a future std could silently change every fingerprint in a
//! cluster, and a mixed-version cluster would stop converging. `Hash` is also not implemented for
//! [`HashMap`](std::collections::HashMap)/[`HashSet`](std::collections::HashSet), which made such
//! values unusable as `rsos` keys or values at all.
//!
//! So `rsos` owns the encoding end to end: this module is a [`serde::Serializer`] that writes a
//! canonical byte stream straight into BLAKE3. `serde` was already a dependency, and no codec crate
//! is involved — the crate stays a zero-infrastructure leaf.
//!
//! # The encoding
//!
//! The encoding is **injective**: no two distinct values of the same type produce the same byte
//! stream. That is what makes distinct elements distinct fingerprints; everything variable-length
//! is therefore length-prefixed rather than concatenated. Multi-byte lengths, counts and variant
//! indices are themselves fixed-width little-endian, so nothing is self-delimiting by accident.
//!
//! Injectivity is *within* a type, which is exactly what the protocol needs — a given store has one
//! key type and one value type. It cannot hold across types and is not claimed: `None::<u8>` and
//! `0u8` both encode to a single zero byte, and no self-describing scheme without type tags could
//! avoid that.
//!
//! | serde form | bytes |
//! |---|---|
//! | `bool` | one byte, `0` or `1` |
//! | `i8`…`i128`, `u8`…`u128` | fixed-width little-endian, the type's own width |
//! | `usize` / `isize` | serde forwards these to `u64` / `i64`, so 32- and 64-bit nodes agree |
//! | `f32` / `f64` | `to_bits().to_le_bytes()` (4 / 8 bytes) |
//! | `char` | the scalar value as `u32` little-endian |
//! | `str`, `bytes` | `u64` little-endian byte length, then the raw bytes |
//! | `None` | `0` |
//! | `Some(v)` | `1`, then `v` |
//! | `unit`, `unit_struct` | nothing |
//! | `newtype_struct` | the inner value alone |
//! | seq, tuple, `tuple_struct` | `u64` little-endian element count, then the elements |
//! | `map` | `u64` little-endian entry count, then the entries, sorted by encoded key |
//! | `struct` | the fields in declaration order — no names, no count |
//! | `unit_variant` | `u32` little-endian variant index |
//! | `newtype_variant` | variant index, then the inner value |
//! | `tuple_variant` | variant index, then element count, then the elements |
//! | `struct_variant` | variant index, then the fields in declaration order |
//!
//! Struct fields carry neither names nor a count because a struct's arity is fixed by its type: two
//! values of the *same* type always emit the same number of fields, so the stream stays
//! unambiguous. Enum variants carry the **index**, not the name, so renaming a variant is not a
//! wire break but reordering variants is.
//!
//! ## Maps are sorted by encoded key
//!
//! Map entries are emitted in ascending order of the **serialized bytes of the key**, not in
//! iteration order and not by `Ord`. Sorting on the encoded key is what lets a
//! [`HashMap`](std::collections::HashMap) fingerprint deterministically despite its
//! iteration order being unspecified and seed-dependent, and it needs no `Ord` bound on the key
//! type — which a `Serializer` could not require anyway. A consequence worth stating: a `HashMap`
//! and a [`BTreeMap`](std::collections::BTreeMap) holding the same entries fingerprint
//! *identically*.
//!
//! ## Floats: bit patterns, not `PartialEq`
//!
//! Floats are encoded by their raw bits, so the encoding disagrees with `PartialEq` in the two
//! places IEEE-754 disagrees with itself. `NaN != NaN`, yet two identical NaN bit patterns
//! fingerprint identically; `+0.0 == -0.0`, yet they fingerprint *differently*. This is a caveat of
//! summarizing floats by content, not a defect: any bit-exact canonical encoding has it, and the
//! alternative (normalizing zeros and NaNs) would break injectivity. Prefer integer or decimal key
//! types if this matters.
//!
//! ## `is_human_readable` is `false`
//!
//! The serializer reports itself as non-human-readable, so types that offer two representations
//! (notably [`IpAddr`](std::net::IpAddr)) take their compact binary one. The choice is arbitrary;
//! what matters is that it is fixed and part of the wire contract.

use std::fmt::{self, Display};

use serde::{ser, Serialize};

/// A byte sink the canonical encoder writes into.
///
/// Two sinks are used: a `blake3::Hasher` (the real one, for fingerprinting) and a `Vec<u8>` (for
/// the buffering the encoding needs — map keys that must be sorted before they are emitted, and
/// sequences whose length is not known up front).
pub trait Sink {
    /// Append `bytes` to the sink.
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

impl Sink for blake3::Hasher {
    fn put(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

/// The error type of the canonical encoder.
///
/// Writing into a byte sink cannot fail — there is no I/O, no allocation limit and no
/// representation a sink can reject — so the encoder itself never constructs one of these. The only
/// way to obtain an `Error` is for a hand-written [`Serialize`] impl to call
/// [`ser::Error::custom`] and refuse to serialize itself, which is a bug in *that* type rather than
/// a condition the fingerprint path can recover from. That is why
/// [`lift`](crate::lift)/[`digest`](crate::digest) are infallible and panic on it instead of
/// swallowing it.
#[derive(Debug)]
pub struct Error(String);

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "canonical encoding failed: {}", self.0)
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

/// Write `value`'s canonical encoding into `sink`.
///
/// See the [module docs](self) for the encoding itself.
pub fn encode_into<S: Sink, T: Serialize + ?Sized>(sink: &mut S, value: &T) -> Result<(), Error> {
    value.serialize(Serializer { sink })
}

/// Encode `value` canonically into a fresh byte buffer.
fn encode_to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    encode_into(&mut buf, value)?;
    Ok(buf)
}

/// The canonical [`serde::Serializer`]: writes the encoding described in the [module docs](self)
/// into a [`Sink`].
struct Serializer<'a, S: Sink> {
    sink: &'a mut S,
}

/// Writes each element straight through to the sink, with no framing of its own.
///
/// Used for every compound whose element count is already fixed by the time elements start
/// arriving: tuples, tuple structs, tuple variants (count written up front) and structs / struct
/// variants (arity fixed by the type, so no count at all).
struct Streaming<'a, S: Sink> {
    sink: &'a mut S,
}

/// A sequence: `u64` element count, then the elements.
///
/// A sequence that declares its length up front streams directly; one that does not is buffered so
/// the count can still be written *before* the elements.
enum SeqSerializer<'a, S: Sink> {
    Streaming(&'a mut S),
    Buffered {
        sink: &'a mut S,
        buf: Vec<u8>,
        count: u64,
    },
}

/// A map: `u64` entry count, then the entries sorted by their encoded key.
///
/// Both halves of every entry are buffered, because the sort order is only known once every key has
/// been encoded.
struct MapSerializer<'a, S: Sink> {
    sink: &'a mut S,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pending_key: Option<Vec<u8>>,
}

/// Little-endian element/byte counts are always 64-bit, independent of the host pointer width.
fn put_len<S: Sink>(sink: &mut S, len: u64) {
    sink.put(&len.to_le_bytes());
}

/// Enum variants are identified by their little-endian `u32` index, never by name.
fn put_variant<S: Sink>(sink: &mut S, index: u32) {
    sink.put(&index.to_le_bytes());
}

impl<'a, S: Sink> ser::Serializer for Serializer<'a, S> {
    type Ok = ();
    type Error = Error;

    type SerializeSeq = SeqSerializer<'a, S>;
    type SerializeTuple = Streaming<'a, S>;
    type SerializeTupleStruct = Streaming<'a, S>;
    type SerializeTupleVariant = Streaming<'a, S>;
    type SerializeMap = MapSerializer<'a, S>;
    type SerializeStruct = Streaming<'a, S>;
    type SerializeStructVariant = Streaming<'a, S>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.sink.put(&[u8::from(v)]);
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_i128(self, v: i128) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_u128(self, v: u128) -> Result<(), Error> {
        self.sink.put(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.sink.put(&v.to_bits().to_le_bytes());
        Ok(())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.sink.put(&v.to_bits().to_le_bytes());
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.sink.put(&(v as u32).to_le_bytes());
        Ok(())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.serialize_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        put_len(self.sink, v.len() as u64);
        self.sink.put(v);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.sink.put(&[0]);
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        self.sink.put(&[1]);
        encode_into(self.sink, value)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), Error> {
        put_variant(self.sink, variant_index);
        Ok(())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        put_variant(self.sink, variant_index);
        encode_into(self.sink, value)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SeqSerializer<'a, S>, Error> {
        match len {
            Some(len) => {
                put_len(self.sink, len as u64);
                Ok(SeqSerializer::Streaming(self.sink))
            }
            // Length unknown up front: buffer the elements so the count still precedes them.
            None => Ok(SeqSerializer::Buffered {
                sink: self.sink,
                buf: Vec::new(),
                count: 0,
            }),
        }
    }

    fn serialize_tuple(self, len: usize) -> Result<Streaming<'a, S>, Error> {
        put_len(self.sink, len as u64);
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Streaming<'a, S>, Error> {
        put_len(self.sink, len as u64);
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        len: usize,
    ) -> Result<Streaming<'a, S>, Error> {
        put_variant(self.sink, variant_index);
        put_len(self.sink, len as u64);
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<MapSerializer<'a, S>, Error> {
        Ok(MapSerializer {
            sink: self.sink,
            entries: Vec::new(),
            pending_key: None,
        })
    }

    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Streaming<'a, S>, Error> {
        // No count: a struct's arity is fixed by its type, so the stream stays unambiguous.
        Ok(Streaming { sink: self.sink })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Streaming<'a, S>, Error> {
        put_variant(self.sink, variant_index);
        Ok(Streaming { sink: self.sink })
    }
}

impl<S: Sink> ser::SerializeSeq for SeqSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        match self {
            SeqSerializer::Streaming(sink) => encode_into(*sink, value),
            SeqSerializer::Buffered { buf, count, .. } => {
                *count += 1;
                encode_into(buf, value)
            }
        }
    }

    fn end(self) -> Result<(), Error> {
        match self {
            SeqSerializer::Streaming(_) => Ok(()),
            SeqSerializer::Buffered {
                sink, buf, count, ..
            } => {
                put_len(sink, count);
                sink.put(&buf);
                Ok(())
            }
        }
    }
}

impl<S: Sink> ser::SerializeTuple for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTupleStruct for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeTupleVariant for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeStruct for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        // Field names are not encoded: declaration order plus the fixed arity identifies them.
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeStructVariant for Streaming<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        encode_into(self.sink, value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: Sink> ser::SerializeMap for MapSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.pending_key = Some(encode_to_vec(key)?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        // `serde` always drives `serialize_key` before `serialize_value` for an entry; a
        // hand-written `SerializeMap` caller that violates that contract would land here.
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("map value serialized before its key"))?;
        self.entries.push((key, encode_to_vec(value)?));
        Ok(())
    }

    fn end(mut self) -> Result<(), Error> {
        // Sort on the *encoded* key: makes an unordered map (`HashMap`) canonical without demanding
        // `Ord`, and makes it agree byte-for-byte with an ordered map holding the same entries.
        // Encoded keys are distinct because a map's keys are distinct and the encoding is injective,
        // so the comparison never has to fall back on the value.
        self.entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        put_len(self.sink, self.entries.len() as u64);
        for (key, value) in &self.entries {
            self.sink.put(key);
            self.sink.put(value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    fn enc<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        encode_to_vec(value).unwrap()
    }

    #[test]
    fn integers_are_fixed_width_little_endian() {
        assert_eq!(enc(&1u8), vec![1]);
        assert_eq!(enc(&1u16), vec![1, 0]);
        assert_eq!(enc(&1u32), vec![1, 0, 0, 0]);
        assert_eq!(enc(&1u64), vec![1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(enc(&(-1i32)), vec![0xff; 4]);
        // `usize` is forwarded to `u64` by serde, so pointer width does not leak.
        assert_eq!(enc(&1usize), enc(&1u64));
        assert_eq!(enc(&(-1isize)), enc(&(-1i64)));
    }

    #[test]
    fn strings_are_length_prefixed() {
        let mut expected = 2u64.to_le_bytes().to_vec();
        expected.extend_from_slice(b"ab");
        assert_eq!(enc("ab"), expected);
    }

    #[test]
    fn strings_are_unambiguously_framed() {
        // The classic framing ambiguity: without a length prefix both pairs would concatenate to
        // "abc".
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
        // `+0.0 == -0.0` under `PartialEq`, but their bit patterns differ.
        assert_ne!(enc(&0.0f64), enc(&(-0.0f64)));
        // `NaN != NaN`, but the same bit pattern encodes identically.
        assert_eq!(enc(&f64::NAN), enc(&f64::NAN));
    }

    #[test]
    fn chars_encode_as_u32() {
        assert_eq!(enc(&'é'), enc(&(u32::from('é'))));
    }

    #[test]
    fn seq_of_unknown_length_still_writes_its_count_first() {
        // `Iterator`-backed `collect_seq` has no size hint here, so this exercises the buffered arm.
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
}
