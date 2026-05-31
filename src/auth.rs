// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-datagram message authentication for the reconciliation protocol.
//!
//! The UDP protocol performs no authentication by itself: any host that can send a datagram to the
//! port can forge an update and poison the whole cluster through last-write-wins (see the
//! crate-level "Security model" documentation). To close that vector, when a cluster key is
//! configured (see
//! [`Config::with_cluster_key`](crate::reconcile_store::Config::with_cluster_key)), every outgoing
//! datagram is framed as `tag || payload` where `tag` is a keyed MAC over `payload`, and every
//! incoming datagram is verified **before** any deserialization.
//!
//! # Design
//!
//! The module is layered so the type system carries the security invariants ("parse, don't
//! validate"):
//!
//! - [`Mac`] is the cryptographic primitive — a compile-time-selected trait with one concrete
//!   backend per Cargo feature ([`Blake3Mac`] for `mac-blake3`, the default, or `HmacSha256Mac`
//!   for `mac-hmac`). [`ClusterMac`] aliases the active backend.
//! - [`ClusterKey`] and [`Tag`] are newtypes around raw byte arrays so keys, tags and arbitrary
//!   buffers cannot be confused.
//! - [`Authenticator`] holds the policy (authentication enabled or not) plus framing. It is the
//!   only producer of a [`Payload`].
//! - [`Payload`] is an opaque wrapper that can only be obtained from [`Authenticator::open`].
//!   Because message handling consumes a `Payload` rather than `&[u8]`, it is structurally
//!   impossible to deserialize bytes that have not cleared the authentication gate.

/// Length in bytes of the authentication tag prepended to every datagram.
pub(crate) const TAG_LEN: usize = 32;

/// Length in bytes of a cluster key.
pub(crate) const KEY_LEN: usize = 32;

#[cfg(not(any(feature = "mac-blake3", feature = "mac-hmac")))]
compile_error!(
    "reconcile: no MAC backend selected. Enable feature `mac-blake3` (default) or `mac-hmac`."
);

/// A shared cluster secret. Constructing one is the only way to enable authentication.
#[derive(Clone, Copy)]
pub(crate) struct ClusterKey([u8; KEY_LEN]);

impl ClusterKey {
    pub(crate) fn new(bytes: [u8; KEY_LEN]) -> Self {
        ClusterKey(bytes)
    }

    fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// A MAC tag. Can only be produced by a [`Mac`] backend.
pub(crate) struct Tag([u8; TAG_LEN]);

impl Tag {
    fn as_bytes(&self) -> &[u8; TAG_LEN] {
        &self.0
    }
}

/// A datagram payload that has cleared the authentication gate — either its tag was verified, or
/// the store is running in (explicitly) unauthenticated mode. The rest of the engine can only
/// obtain one through [`Authenticator::open`], so message handling cannot, by construction, run on
/// bytes that were not cleared first ("parse, don't validate").
pub(crate) struct Payload<'a>(&'a [u8]);

impl<'a> Payload<'a> {
    pub(crate) fn as_bytes(&self) -> &'a [u8] {
        self.0
    }
}

/// The keyed MAC primitive used to authenticate datagrams.
///
/// The active backend is selected at compile time through the `mac-*` Cargo features and aliased
/// as [`ClusterMac`]; this trait makes the contract that every backend must satisfy explicit and
/// compiler-checked, rather than relying on convention.
pub(crate) trait Mac {
    /// Compute the authentication tag of `message` under `key`.
    fn tag(key: &ClusterKey, message: &[u8]) -> Tag;

    /// Constant-time check that `tag` authenticates `message` under `key`.
    ///
    /// `tag` is the untrusted on-the-wire slice; an incorrect length yields `false`.
    fn verify(key: &ClusterKey, message: &[u8], tag: &[u8]) -> bool;
}

#[cfg(feature = "mac-blake3")]
pub(crate) struct Blake3Mac;

#[cfg(feature = "mac-blake3")]
impl Mac for Blake3Mac {
    fn tag(key: &ClusterKey, message: &[u8]) -> Tag {
        Tag(*blake3::keyed_hash(key.as_bytes(), message).as_bytes())
    }

    fn verify(key: &ClusterKey, message: &[u8], tag: &[u8]) -> bool {
        let Ok(tag) = <[u8; TAG_LEN]>::try_from(tag) else {
            return false;
        };
        // `blake3::Hash`'s `PartialEq` is constant-time.
        blake3::keyed_hash(key.as_bytes(), message) == blake3::Hash::from_bytes(tag)
    }
}

// Compiled only when it is actually the selected backend (`mac-blake3` takes precedence), so an
// `--all-features` build does not carry an unused struct.
#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
pub(crate) struct HmacSha256Mac;

#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
impl Mac for HmacSha256Mac {
    fn tag(key: &ClusterKey, message: &[u8]) -> Tag {
        use hmac::{Hmac, Mac as _};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(message);
        // SHA-256 produces exactly TAG_LEN bytes: no truncation.
        Tag(mac.finalize().into_bytes().into())
    }

    fn verify(key: &ClusterKey, message: &[u8], tag: &[u8]) -> bool {
        use hmac::{Hmac, Mac as _};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(message);
        // `verify_slice` is constant-time and length-checked.
        mac.verify_slice(tag).is_ok()
    }
}

// `mac-blake3` takes precedence when both backends are enabled (e.g. under `--all-features`), so
// such builds still compile instead of hitting a hard error.
#[cfg(feature = "mac-blake3")]
pub(crate) type ClusterMac = Blake3Mac;
#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
pub(crate) type ClusterMac = HmacSha256Mac;

/// Authentication policy and datagram framing for one node.
///
/// Holds the cluster key (or the absence thereof) and is the sole producer of [`Payload`] values.
#[derive(Clone, Copy)]
pub(crate) enum Authenticator {
    /// No cluster key configured: the protocol runs unauthenticated.
    Disabled,
    /// A cluster key is configured: datagrams are sealed and verified.
    Enabled(ClusterKey),
}

impl Authenticator {
    /// Build an authenticator from an optional raw cluster key.
    pub(crate) fn new(key: Option<[u8; KEY_LEN]>) -> Self {
        match key {
            Some(bytes) => Authenticator::Enabled(ClusterKey::new(bytes)),
            None => Authenticator::Disabled,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        matches!(self, Authenticator::Enabled(_))
    }

    /// Number of extra bytes a sealed datagram adds, for MTU/buffer accounting.
    pub(crate) fn overhead(&self) -> usize {
        match self {
            Authenticator::Disabled => 0,
            Authenticator::Enabled(_) => TAG_LEN,
        }
    }

    /// Frame an outgoing datagram as `tag(payload) || payload`.
    ///
    /// Returns `Some(framed)` when enabled, or `None` when disabled (the caller then sends
    /// `payload` unchanged, byte-for-byte identical to the unauthenticated protocol).
    pub(crate) fn seal(&self, payload: &[u8]) -> Option<Vec<u8>> {
        match self {
            Authenticator::Disabled => None,
            Authenticator::Enabled(key) => {
                let tag = ClusterMac::tag(key, payload);
                let mut framed = Vec::with_capacity(TAG_LEN + payload.len());
                framed.extend_from_slice(tag.as_bytes());
                framed.extend_from_slice(payload);
                Some(framed)
            }
        }
    }

    /// Authenticate an incoming datagram, returning the [`Payload`] cleared for processing.
    ///
    /// When enabled, the datagram must be `tag || payload` with a valid tag; otherwise (too short
    /// or invalid tag) `None` is returned and the caller drops it silently. When disabled, the
    /// whole datagram is returned as the payload.
    pub(crate) fn open<'a>(&self, datagram: &'a [u8]) -> Option<Payload<'a>> {
        match self {
            Authenticator::Disabled => Some(Payload(datagram)),
            Authenticator::Enabled(key) => {
                if datagram.len() < TAG_LEN {
                    return None;
                }
                let (tag, payload) = datagram.split_at(TAG_LEN);
                ClusterMac::verify(key, payload, tag).then_some(Payload(payload))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> ClusterKey {
        ClusterKey::new([byte; KEY_LEN])
    }

    #[test]
    fn tag_verify_roundtrip() {
        let k = key(0x11);
        let t = ClusterMac::tag(&k, b"hello world");
        assert!(ClusterMac::verify(&k, b"hello world", t.as_bytes()));
    }

    #[test]
    fn tamper_detection() {
        let k = key(0x11);
        let payload = b"the quick brown fox".to_vec();
        let t = ClusterMac::tag(&k, &payload);

        // Flip a payload byte.
        let mut bad_payload = payload.clone();
        bad_payload[0] ^= 0x01;
        assert!(!ClusterMac::verify(&k, &bad_payload, t.as_bytes()));

        // Flip a tag byte.
        let mut bad_tag = *t.as_bytes();
        bad_tag[0] ^= 0x01;
        assert!(!ClusterMac::verify(&k, &payload, &bad_tag));
    }

    #[test]
    fn wrong_key_rejected() {
        let t = ClusterMac::tag(&key(0x11), b"payload");
        assert!(!ClusterMac::verify(&key(0x22), b"payload", t.as_bytes()));
    }

    #[test]
    fn seal_open_roundtrip() {
        let auth = Authenticator::new(Some([0x11; KEY_LEN]));
        let payload = b"some serialized message";
        let sealed = auth.seal(payload).expect("enabled");
        assert_eq!(sealed.len(), TAG_LEN + payload.len());
        assert_eq!(auth.open(&sealed).map(|p| p.as_bytes()), Some(&payload[..]));
    }

    #[test]
    fn open_too_short() {
        let auth = Authenticator::new(Some([0x11; KEY_LEN]));
        // Fewer than TAG_LEN bytes can never carry a valid tag.
        assert!(auth.open(&[0u8; 10]).is_none());
        assert!(auth.open(&[]).is_none());
    }

    #[test]
    fn open_wrong_key() {
        let sealed = Authenticator::new(Some([0x11; KEY_LEN]))
            .seal(b"payload")
            .expect("enabled");
        assert!(Authenticator::new(Some([0x22; KEY_LEN]))
            .open(&sealed)
            .is_none());
    }

    #[test]
    fn disabled_passes_through_and_does_not_seal() {
        let auth = Authenticator::new(None);
        assert!(!auth.is_enabled());
        assert_eq!(auth.overhead(), 0);
        assert!(auth.seal(b"payload").is_none());
        // Any datagram clears the gate unchanged in unauthenticated mode.
        assert_eq!(
            auth.open(b"raw bytes").map(|p| p.as_bytes()),
            Some(&b"raw bytes"[..])
        );
    }
}
