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
//!
//! Split across siblings by concern: `serializer` owns the `ser::Serializer` impl for
//! `Serializer` — every scalar plus the entry point into each compound form; `seq`/`map` each own
//! the buffered `ser::Serialize*` impl for `SeqSerializer`/`MapSerializer`; `streaming` owns the
//! five straight-through impls for `Streaming` (tuples, tuple structs, tuple variants, structs,
//! struct variants); `error` owns [`Error`]'s trait impls. This file keeps the public
//! type definitions (their module location is their `cargo public-api`-visible path — see
//! AGENTS.md §11) plus the shared framing helpers every sibling draws on.

use serde::Serialize;

mod error;
mod map;
mod seq;
mod serializer;
mod streaming;

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
/// Only constructible by a hand-written [`Serialize`] impl calling [`serde::ser::Error::custom`];
/// the encoder itself never fails, which is why [`lift`](crate::lift)/[`digest`](crate::digest)
/// are infallible.
#[derive(Debug)]
pub struct Error(String);

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

#[cfg(test)]
mod tests;
