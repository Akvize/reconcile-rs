// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Encrypt+authenticate on send: [`Authenticator::seal`] and the replay-header encoding it
//! frames every enabled datagram with. The receive-side counterpart is `open`.

use super::mac::{ClusterMac, Mac};
use super::{Authenticator, WIRE_VERSION};
use crate::replay::{Seq, Stamp, REPLAY_HEADER_LEN};

/// Build a replay header: `seq (8 bytes LE) || stamp (8 bytes LE)`.
fn encode_replay_header(seq: Seq, stamp: Stamp) -> [u8; REPLAY_HEADER_LEN] {
    let mut header = [0u8; REPLAY_HEADER_LEN];
    header[..8].copy_from_slice(&seq.to_le_bytes());
    header[8..].copy_from_slice(&stamp.to_le_bytes());
    header
}

impl Authenticator {
    /// Frame an outgoing datagram: inject the wire-version byte (every mode) and, when
    /// enabled, the replay header. Always seals with [`Keys::primary`](super::Keys::primary),
    /// never an `also_accept` key — a rotation moves senders once every peer's verify path
    /// already accepts the new key.
    pub fn seal(&self, seq: Seq, stamp: Stamp, payload: &[u8]) -> Vec<u8> {
        match self {
            Authenticator::Disabled => {
                let mut framed = Vec::with_capacity(super::VERSION_LEN + payload.len());
                framed.push(WIRE_VERSION);
                framed.extend_from_slice(payload);
                framed
            }
            Authenticator::Enabled(keys) => {
                let header = encode_replay_header(seq, stamp);
                let mut protected =
                    Vec::with_capacity(REPLAY_HEADER_LEN + super::VERSION_LEN + payload.len());
                protected.extend_from_slice(&header);
                protected.push(WIRE_VERSION);
                protected.extend_from_slice(payload);
                let tag = ClusterMac::tag(&keys.primary, &protected);
                let mut framed = Vec::with_capacity(super::TAG_LEN + protected.len());
                framed.extend_from_slice(tag.as_bytes());
                framed.extend_from_slice(&protected);
                framed
            }
            #[cfg(feature = "encryption")]
            Authenticator::Encrypted(keys) => {
                let header = encode_replay_header(seq, stamp);
                let mut plaintext =
                    Vec::with_capacity(REPLAY_HEADER_LEN + super::VERSION_LEN + payload.len());
                plaintext.extend_from_slice(&header);
                plaintext.push(WIRE_VERSION);
                plaintext.extend_from_slice(payload);
                super::encryption::seal(&keys.primary, &plaintext)
            }
        }
    }
}
