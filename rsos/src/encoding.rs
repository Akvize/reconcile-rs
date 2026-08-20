// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The **canonical encoding**: the byte source for element fingerprints
//! (`ARCHITECTURE.md` §6).
//!
//! A [`serde::Serializer`] writing straight into BLAKE3, **injective within a type** — everything
//! variable-length is length-prefixed; counts, lengths and variant indices are fixed-width
//! little-endian. Injectivity across types is not claimed (`None::<u8>` and `0u8` both encode to
//! one zero byte).
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
//! Renaming an enum variant is not a wire break; reordering variants is. Maps sort on the
//! **encoded** key, so a `HashMap` and a `BTreeMap` with the same entries encode identically and
//! no `Ord` bound is needed. Floats encode by bit pattern, so `+0.0`/`-0.0` differ and two equal
//! NaN patterns agree. `is_human_readable` is fixed at `false` and is part of the wire contract.

use std::fmt::{self, Display};

use serde::{ser, Serialize};

/// A byte sink the canonical encoder writes into: `blake3::Hasher`, or `Vec<u8>` for buffering.
///
/// ```
/// use rsos::encoding::{encode_into, Sink};
///
/// // A minimal sink that only counts bytes, without storing them.
/// struct ByteCounter(usize);
/// impl Sink for ByteCounter {
///     fn put(&mut self, bytes: &[u8]) {
///         self.0 += bytes.len();
///     }
/// }
///
/// let mut counter = ByteCounter(0);
/// encode_into(&mut counter, "hello").unwrap();
///
/// // An 8-byte little-endian length prefix, then the 5 payload bytes -- any `Sink` sees exactly
/// // what the `Vec<u8>` impl would.
/// assert_eq!(counter.0, 8 + 5);
/// ```
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
/// Only constructible by a hand-written [`Serialize`] impl calling [`ser::Error::custom`]; the
/// encoder itself never fails, which is why [`lift`](crate::lift)/[`digest`](crate::digest) are
/// infallible.
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
/// ```
/// use rsos::encoding::encode_into;
///
/// let mut bytes = Vec::new();
/// encode_into(&mut bytes, "ab").unwrap();
///
/// // `str`/`bytes` are length-prefixed -- a `u64` little-endian count, then the raw bytes -- which
/// // is what keeps ("ab", "c") and ("a", "bc") from encoding to the same bytes downstream.
/// let mut expected = 2u64.to_le_bytes().to_vec();
/// expected.extend_from_slice(b"ab");
/// assert_eq!(bytes, expected);
/// ```
pub fn encode_into<S: Sink, T: Serialize + ?Sized>(sink: &mut S, value: &T) -> Result<(), Error> {
    value.serialize(Serializer { sink })
}

/// Encode `value` canonically into a fresh byte buffer.
///
/// Equivalent to [`encode_into`] against a fresh `Vec<u8>` — public because that composition is
/// three lines any dependent can already write, so keeping this one private buys no protection,
/// only an extra round trip for a caller that just wants the bytes.
pub fn encode_to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    encode_into(&mut buf, value)?;
    Ok(buf)
}

/// The canonical [`serde::Serializer`] (see the [module docs](self)), writing into a [`Sink`].
struct Serializer<'a, S: Sink> {
    sink: &'a mut S,
}

/// Elements straight through, no framing: compounds whose arity is already fixed.
struct Streaming<'a, S: Sink> {
    sink: &'a mut S,
}

/// A sequence: `u64` element count, then the elements. Buffered when the length is unknown up
/// front, so the count still precedes the elements.
enum SeqSerializer<'a, S: Sink> {
    Streaming(&'a mut S),
    Buffered {
        sink: &'a mut S,
        buf: Vec<u8>,
        count: u64,
    },
}

/// A map: `u64` entry count, then the entries sorted by encoded key — buffered, since the order
/// is known only once every key is encoded.
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
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("map value serialized before its key"))?;
        self.entries.push((key, encode_to_vec(value)?));
        Ok(())
    }

    fn end(mut self) -> Result<(), Error> {
        // Sort on the encoded key: canonical without an `Ord` bound.
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
