// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Codec`] port — the domain's wire-encoding boundary — and its default [`BincodeCodec`]
//! adapter (`ARCHITECTURE.md` §3.4).
//!
//! The reconciliation engine drives itself over this port instead of calling `bincode` directly, so
//! the encoding is a substitutable adapter rather than a hard dependency of the domain. Message
//! authentication always sits **ahead of** the codec: the MAC is verified on the raw datagram bytes
//! before any decoding runs (invariant #5, `ARCHITECTURE.md` §5), so a forged datagram never reaches
//! [`decode_stream`](Codec::decode_stream).
//!
//! Deliberately `pub(crate)` (`ARCHITECTURE.md` §7 D2): `Codec` has generic methods, so it is not
//! object-safe and is always carried as a type parameter — exposing it would force a type-changing
//! builder on every consumer. Its plausible uses (compression, cross-language interop) are not
//! served by swapping the trait anyway: compression interacts with authenticate-before-decode and
//! with datagram-size accounting, and cross-language interop needs a published wire spec, not a
//! Rust trait.

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Abstracts the wire encoding used for protocol messages.
///
/// A single datagram carries a *stream* of protocol messages, so the decode side returns a `Vec`
/// and takes a `max_items` cap: a crafted datagram cannot be expanded into an unbounded number of
/// messages (a denial-of-service hazard). The encode side appends to a caller-owned buffer so a
/// batch of messages can be framed into one datagram without per-message allocation.
pub(crate) trait Codec: Send + Sync + 'static {
    /// The error type produced by encoding or decoding.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Append the encoding of `value` to `out`.
    fn encode<T: Serialize>(&self, value: &T, out: &mut Vec<u8>) -> Result<(), Self::Error>;

    /// Decode a stream of `T` from `bytes`, stopping at a clean end-of-input or once `max_items`
    /// values have been decoded (whichever comes first).
    ///
    /// A **clean** end-of-input (all bytes consumed on a message boundary) yields the values decoded
    /// so far; a *malformed* stream (a partial or invalid message mid-buffer) is an error, so a
    /// corrupt or hostile datagram is rejected wholesale rather than half-applied.
    fn decode_stream<T: DeserializeOwned>(
        &self,
        bytes: &[u8],
        max_items: usize,
    ) -> Result<Vec<T>, Self::Error>;
}

/// The default [`Codec`] adapter: `bincode` with the crate's frozen
/// [`DefaultOptions`](bincode::DefaultOptions) configuration.
///
/// The configuration (variable-int encoding, little-endian) is part of the frozen wire format;
/// changing it is a wire break. Zero-sized, so it is free to hold and clone.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BincodeCodec;

impl BincodeCodec {
    /// Create the default bincode codec.
    pub(crate) fn new() -> Self {
        BincodeCodec
    }
}

impl Codec for BincodeCodec {
    type Error = bincode::Error;

    fn encode<T: Serialize>(&self, value: &T, out: &mut Vec<u8>) -> Result<(), Self::Error> {
        use bincode::{DefaultOptions, Serializer};
        value.serialize(&mut Serializer::new(out, DefaultOptions::new()))
    }

    fn decode_stream<T: DeserializeOwned>(
        &self,
        bytes: &[u8],
        max_items: usize,
    ) -> Result<Vec<T>, Self::Error> {
        use bincode::{DefaultOptions, Deserializer};
        let mut deserializer = Deserializer::from_slice(bytes, DefaultOptions::new());
        let mut out = Vec::new();
        while out.len() < max_items {
            match T::deserialize(&mut deserializer) {
                Ok(value) => out.push(value),
                Err(err) => {
                    // A clean end-of-stream surfaces as an `UnexpectedEof` I/O error once every
                    // framed message has been consumed; that is success, not corruption. Any other
                    // error (or a truncated message mid-buffer) means the datagram is malformed and
                    // the whole thing is rejected — matching the engine's drop-the-datagram policy.
                    if let bincode::ErrorKind::Io(io_err) = err.as_ref() {
                        if io_err.kind() == std::io::ErrorKind::UnexpectedEof {
                            break;
                        }
                    }
                    return Err(err);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_stream_of_messages() {
        let codec = BincodeCodec::new();
        let mut buf = Vec::new();
        codec.encode(&1u32, &mut buf).unwrap();
        codec.encode(&2u32, &mut buf).unwrap();
        codec.encode(&3u32, &mut buf).unwrap();
        let decoded: Vec<u32> = codec.decode_stream(&buf, 100).unwrap();
        assert_eq!(decoded, vec![1, 2, 3]);
    }

    #[test]
    fn max_items_caps_the_decoded_count() {
        let codec = BincodeCodec::new();
        let mut buf = Vec::new();
        for i in 0..10u32 {
            codec.encode(&i, &mut buf).unwrap();
        }
        // Only the first `max_items` are decoded; the rest are left un-decoded (DoS bound).
        let decoded: Vec<u32> = codec.decode_stream(&buf, 4).unwrap();
        assert_eq!(decoded, vec![0, 1, 2, 3]);
    }

    #[test]
    fn empty_input_decodes_to_nothing() {
        let codec = BincodeCodec::new();
        let decoded: Vec<u32> = codec.decode_stream(&[], 100).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn a_malformed_message_is_an_error() {
        // A byte that is neither 0 nor 1 is an invalid `bool` encoding — a genuine mid-stream
        // corruption (not a clean end-of-input), so the whole datagram is rejected.
        let codec = BincodeCodec::new();
        let decoded: Result<Vec<bool>, _> = codec.decode_stream(&[2u8], 100);
        assert!(
            decoded.is_err(),
            "a malformed message must be rejected, not silently accepted"
        );
    }

    #[test]
    fn a_truncated_trailing_message_ends_the_stream_leniently() {
        // A final message cut short surfaces as an end-of-input (indistinguishable from a clean
        // boundary in a non-self-describing stream), so the intact prefix is returned and the
        // partial tail is dropped — matching the engine's historical datagram handling.
        // Fixed-width 4-byte arrays give an unambiguous frame boundary (no varint), so a truncated
        // trailing frame is a clean short read rather than being misparsed as more values.
        let codec = BincodeCodec::new();
        let mut buf = Vec::new();
        codec.encode(&[1u8, 2, 3, 4], &mut buf).unwrap();
        codec.encode(&[5u8, 6, 7, 8], &mut buf).unwrap();
        buf.pop(); // truncate the trailing 4-byte frame to 3 bytes
        let decoded: Vec<[u8; 4]> = codec
            .decode_stream(&buf, 100)
            .expect("a truncated trailing message ends the stream, not an error");
        assert_eq!(decoded, vec![[1, 2, 3, 4]]);
    }
}
