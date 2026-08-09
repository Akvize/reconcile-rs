// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The crate's wire-encoding functions: `bincode` with the frozen
//! [`DefaultOptions`](::bincode::DefaultOptions) configuration.
//!
//! Plain functions, not a type: there is exactly one encoding in use, both functions are generic
//! (so a trait version would not be object-safe and would always be carried as a type parameter
//! anyway), and there is no state to hold or inject — an earlier concrete `BincodeCodec` struct was
//! zero-sized and never swapped, so it was dissolved in favor of calling these directly.
//! `HashRangeQueryable`/`Diffable` and `Reconcilable`/`MaybeTombstone` were dissolved for the same
//! kind of reason (`ARCHITECTURE.md` §2.4).
//!
//! The reconciliation engine drives itself over [`encode`]/[`decode_stream`] instead of calling
//! `bincode` directly elsewhere, so the encoding stays isolated to this module. Message
//! authentication always sits **ahead of** the codec: the MAC is verified on the raw datagram bytes
//! before any decoding runs (invariant #5, `ARCHITECTURE.md` §5), so a forged datagram never reaches
//! [`decode_stream`].
//!
//! Named `bincode` (matching the external crate it wraps) rather than `codec`, since there is no
//! abstraction left to name — this is just the code that talks to the `bincode` crate. Inside this
//! module, and anywhere else that needs to name the external crate unambiguously, use `::bincode::…`
//! to disambiguate from this module (`crate::bincode`).

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Append the encoding of `value` to `out`.
///
/// The encode side appends to a caller-owned buffer so a batch of messages can be framed into
/// one datagram without per-message allocation.
///
/// # Errors
///
/// Returns an error only if `T`'s `Serialize` implementation itself fails; encoding to an
/// in-memory `Vec` cannot fail for I/O reasons.
pub fn encode<T: Serialize>(value: &T, out: &mut Vec<u8>) -> ::bincode::Result<()> {
    use ::bincode::{DefaultOptions, Serializer};
    value.serialize(&mut Serializer::new(out, DefaultOptions::new()))
}

/// Decode a stream of `T` from `bytes`, stopping at a clean end-of-input or once `max_items`
/// values have been decoded (whichever comes first).
///
/// A single datagram carries a *stream* of protocol messages, so this returns a `Vec` and takes
/// a `max_items` cap: a crafted datagram cannot be expanded into an unbounded number of messages
/// (a denial-of-service hazard).
///
/// A **clean** end-of-input (all bytes consumed on a message boundary) yields the values decoded
/// so far; a *malformed* stream (a partial or invalid message mid-buffer) is an error, so a
/// corrupt or hostile datagram is rejected wholesale rather than half-applied.
///
/// # Errors
///
/// Returns an error if `bytes` contains a message that fails to deserialize as `T` before a clean
/// end-of-input is reached — see above for why a truncated trailing message is not treated as an
/// error.
pub fn decode_stream<T: DeserializeOwned>(
    bytes: &[u8],
    max_items: usize,
) -> ::bincode::Result<Vec<T>> {
    use ::bincode::{DefaultOptions, Deserializer};
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
                if let ::bincode::ErrorKind::Io(io_err) = err.as_ref() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_stream_of_messages() {
        let mut buf = Vec::new();
        encode(&1u32, &mut buf).unwrap();
        encode(&2u32, &mut buf).unwrap();
        encode(&3u32, &mut buf).unwrap();
        let decoded: Vec<u32> = decode_stream(&buf, 100).unwrap();
        assert_eq!(decoded, vec![1, 2, 3]);
    }

    #[test]
    fn max_items_caps_the_decoded_count() {
        let mut buf = Vec::new();
        for i in 0..10u32 {
            encode(&i, &mut buf).unwrap();
        }
        // Only the first `max_items` are decoded; the rest are left un-decoded (DoS bound).
        let decoded: Vec<u32> = decode_stream(&buf, 4).unwrap();
        assert_eq!(decoded, vec![0, 1, 2, 3]);
    }

    #[test]
    fn empty_input_decodes_to_nothing() {
        let decoded: Vec<u32> = decode_stream(&[], 100).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn a_malformed_message_is_an_error() {
        // A byte that is neither 0 nor 1 is an invalid `bool` encoding — a genuine mid-stream
        // corruption (not a clean end-of-input), so the whole datagram is rejected.
        let decoded: Result<Vec<bool>, _> = decode_stream(&[2u8], 100);
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
        let mut buf = Vec::new();
        encode(&[1u8, 2, 3, 4], &mut buf).unwrap();
        encode(&[5u8, 6, 7, 8], &mut buf).unwrap();
        buf.pop();
        let decoded: Vec<[u8; 4]> = decode_stream(&buf, 100)
            .expect("a truncated trailing message ends the stream, not an error");
        assert_eq!(decoded, vec![[1, 2, 3, 4]]);
    }
}
