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
//! Plain functions, not a port (`ARCHITECTURE.md` §3.2). Authentication sits **ahead** of the
//! codec, so a forged datagram never reaches [`decode_stream`] (`ARCHITECTURE.md` §5 invariant 5).
//! `::bincode::…` names the external crate.

use std::error::Error as StdError;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Why [`encode`] failed. Opaque wrapper over the external `bincode` crate's error (#297): a
/// public signature naming `::bincode::Error` directly would force every dependent onto this
/// crate's exact `bincode` version for a type they never construct or match on — `encode` only
/// fails when `T`'s `Serialize` implementation does, so callers observe this as `Debug`/`Display`
/// or via [`std::error::Error::source`], never by matching a variant.
#[derive(Debug)]
pub struct EncodeError(::bincode::Error);

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bincode encode failed: {}", self.0)
    }
}

impl StdError for EncodeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.0)
    }
}

/// Why [`decode_stream`] failed: a message failed to deserialize before a clean end-of-input.
/// Opaque for the same reason as [`EncodeError`].
#[derive(Debug)]
pub struct DecodeError(::bincode::Error);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bincode decode failed: {}", self.0)
    }
}

impl StdError for DecodeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.0)
    }
}

/// Append the encoding of `value` to a caller-owned buffer, so a batch frames into one datagram
/// without per-message allocation.
///
/// # Errors
///
/// Only if `T`'s `Serialize` implementation fails.
pub fn encode<T: Serialize>(value: &T, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    use ::bincode::{DefaultOptions, Serializer};
    value
        .serialize(&mut Serializer::new(out, DefaultOptions::new()))
        .map_err(EncodeError)
}

/// Decode a stream of `T` from `bytes`, stopping at a clean end-of-input or at `max_items` —
/// the cap that keeps a crafted datagram from expanding into unboundedly many messages.
///
/// # Errors
///
/// If a message fails to deserialize before a clean end-of-input: a corrupt datagram is rejected
/// wholesale, never half-applied.
pub fn decode_stream<T: DeserializeOwned>(
    bytes: &[u8],
    max_items: usize,
) -> Result<Vec<T>, DecodeError> {
    use ::bincode::{DefaultOptions, Deserializer};
    let mut deserializer = Deserializer::from_slice(bytes, DefaultOptions::new());
    let mut out = Vec::new();
    while out.len() < max_items {
        match T::deserialize(&mut deserializer) {
            Ok(value) => out.push(value),
            Err(err) => {
                // A clean end-of-stream surfaces as `UnexpectedEof`: success, not corruption.
                if let ::bincode::ErrorKind::Io(io_err) = err.as_ref() {
                    if io_err.kind() == std::io::ErrorKind::UnexpectedEof {
                        break;
                    }
                }
                return Err(DecodeError(err));
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
        let decoded: Result<Vec<bool>, _> = decode_stream(&[2u8], 100);
        assert!(
            decoded.is_err(),
            "a malformed message must be rejected, not silently accepted"
        );
    }

    #[test]
    fn a_truncated_trailing_message_ends_the_stream_leniently() {
        // A cut-short final message is indistinguishable from a clean boundary in a
        // non-self-describing stream, so the intact prefix is returned.
        let mut buf = Vec::new();
        encode(&[1u8, 2, 3, 4], &mut buf).unwrap();
        encode(&[5u8, 6, 7, 8], &mut buf).unwrap();
        buf.pop();
        let decoded: Vec<[u8; 4]> = decode_stream(&buf, 100)
            .expect("a truncated trailing message ends the stream, not an error");
        assert_eq!(decoded, vec![[1, 2, 3, 4]]);
    }
}
