// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Verify+decrypt on receive: [`Authenticator::open`] and the [`Payload`] state machine that
//! carries a datagram from authenticated through version-checked to replay-checked. The
//! send-side counterpart is `seal`.

use std::borrow::Cow;
use std::marker::PhantomData;
use std::net::IpAddr;

use super::mac::{ClusterMac, Mac};
use super::{Authenticated, Authenticator, Payload, Verified, VERSION_LEN};
use crate::replay::{ReplayFilter, Seq, Stamp, REPLAY_HEADER_LEN};

/// Parse a replay header from the front of a byte slice.
///
/// Returns `(seq, stamp, rest)` or `None` if the slice is shorter than the header.
fn decode_replay_header(data: &[u8]) -> Option<(Seq, Stamp, &[u8])> {
    if data.len() < REPLAY_HEADER_LEN {
        return None;
    }
    let seq = Seq::from_le_bytes(data[..8].try_into().unwrap());
    let stamp = Stamp::from_le_bytes(data[8..16].try_into().unwrap());
    Some((seq, stamp, &data[REPLAY_HEADER_LEN..]))
}

impl<'a> Payload<'a, Authenticated> {
    /// Strip and check the leading wire-version byte. Call this before
    /// [`verify_replay`](Self::verify_replay) — see the module doc for why the ordering matters.
    ///
    /// `Err(actual)` on a mismatch (or an empty payload, reported as version `0`), carrying the
    /// version the peer actually sent so the caller can log it. The caller should count this
    /// distinctly from an authentication failure: [`super::WIRE_VERSION`] mismatches are a
    /// mixed-version cluster, not an attack or a malformed datagram.
    pub fn check_version(self) -> Result<Self, u8> {
        let version = *self.bytes.first().unwrap_or(&0);
        if version != super::WIRE_VERSION {
            return Err(version);
        }
        let bytes = match self.bytes {
            Cow::Borrowed(b) => Cow::Borrowed(&b[VERSION_LEN..]),
            Cow::Owned(mut b) => {
                b.drain(..VERSION_LEN);
                Cow::Owned(b)
            }
        };
        Ok(Payload {
            bytes,
            seq: self.seq,
            stamp: self.stamp,
            _state: PhantomData,
        })
    }

    /// The sole path from [`Authenticated`] to [`Verified`].
    ///
    /// `None` when the datagram is a replay, a duplicate, or outside the freshness window — the
    /// caller drops it silently. A disabled [`ReplayFilter`] accepts unconditionally.
    pub fn verify_replay(
        self,
        filter: &ReplayFilter,
        sender: IpAddr,
    ) -> Option<Payload<'a, Verified>> {
        if !filter.check_and_record(sender, self.seq, self.stamp) {
            return None;
        }
        Some(Payload {
            bytes: self.bytes,
            seq: self.seq,
            stamp: self.stamp,
            _state: PhantomData,
        })
    }
}

impl Payload<'_, Verified> {
    /// The decoded, authenticated, replay-checked message bytes, ready for [`crate::bincode`].
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Authenticator {
    /// Authenticate (and in encrypted mode decrypt) an incoming datagram.
    ///
    /// Tries [`Keys::primary`](super::Keys::primary) then each `also_accept` key in order, so a
    /// mid-rotation cluster — some peers still sealing with the outgoing key — keeps verifying
    /// until every sender has moved (#137).
    ///
    /// Produces [`Authenticated`], never [`Verified`]: the caller must still
    /// [`Payload::check_version`] then [`Payload::verify_replay`]. `None` on any authentication
    /// failure, and the caller drops it silently — a wire-version mismatch is reported
    /// separately, by `check_version`, once authentication has already cleared it.
    pub fn open<'a>(&self, datagram: &'a [u8]) -> Option<Payload<'a, Authenticated>> {
        match self {
            Authenticator::Disabled => Some(Payload {
                bytes: Cow::Borrowed(datagram),
                seq: Seq::NONE,
                stamp: Stamp::NONE,
                _state: PhantomData,
            }),
            Authenticator::Enabled(keys) => {
                if datagram.len() < super::TAG_LEN + REPLAY_HEADER_LEN {
                    return None;
                }
                let (tag, protected) = datagram.split_at(super::TAG_LEN);
                if !keys
                    .iter()
                    .any(|key| ClusterMac::verify(key, protected, tag))
                {
                    return None;
                }
                let (seq, stamp, messages) = decode_replay_header(protected)?;
                Some(Payload {
                    bytes: Cow::Borrowed(messages),
                    seq,
                    stamp,
                    _state: PhantomData,
                })
            }
            #[cfg(feature = "encryption")]
            Authenticator::Encrypted(keys) => {
                let plaintext = keys
                    .iter()
                    .find_map(|key| super::encryption::open(key, datagram))?;
                let (seq, stamp, messages) = decode_replay_header(&plaintext)?;
                Some(Payload {
                    bytes: Cow::Owned(messages.to_vec()),
                    seq,
                    stamp,
                    _state: PhantomData,
                })
            }
        }
    }
}
