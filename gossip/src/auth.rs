// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-datagram message authentication. Unauthenticated by default: README "Security model".
//!
//! # Wire layout (authenticated modes)
//!
//! MAC mode: `tag (32 B) || seq (8 B LE) || stamp (8 B LE) || protocol_messages`
//!
//! Encryption mode: `nonce (24 B) || encrypt(seq (8 B LE) || stamp (8 B LE) || protocol_messages) || tag (16 B)`
//!
//! The replay header sits inside the authenticated/encrypted region in both cases.
//!
//! The layering carries the security invariants in the types: [`Authenticator`] is the sole
//! producer of a [`Payload`], and message handling consumes `Payload<`[`Verified`]`>`, obtainable
//! only from [`Payload::verify_replay`] — so authenticate-before-decode
//! (`ARCHITECTURE.md` §5 invariant 5) and check-replay-before-handle are both compile-time.

use std::borrow::Cow;
use std::marker::PhantomData;
use std::net::IpAddr;

use crate::replay::{ReplayFilter, Seq, Stamp, REPLAY_HEADER_LEN};

/// Length in bytes of the authentication tag prepended to every datagram.
pub const TAG_LEN: usize = 32;

/// Length in bytes of a cluster key.
pub const KEY_LEN: usize = 32;

/// Length in bytes of the XChaCha20-Poly1305 nonce prepended to each encrypted datagram.
///
/// 192 bits: safe to draw at random per datagram, so no per-peer counter is needed.
#[cfg(feature = "encryption")]
pub const AEAD_NONCE_LEN: usize = 24;

/// Length in bytes of the XChaCha20-Poly1305 (Poly1305) authentication tag.
#[cfg(feature = "encryption")]
pub const AEAD_TAG_LEN: usize = 16;

#[cfg(not(any(feature = "mac-blake3", feature = "mac-hmac")))]
compile_error!(
    "gossip: no MAC backend selected. Enable feature `mac-blake3` (default) or `mac-hmac` — \
     either on this crate directly, or via the identically-named unification feature on `reconcile`."
);

/// A shared cluster secret. Constructing one is the only way to enable authentication.
///
/// `Clone` but not `Copy`: the `zeroize` feature gives it a wiping `Drop`, which `Copy` forbids.
#[cfg_attr(feature = "zeroize", derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop))]
#[derive(Clone)]
pub struct ClusterKey([u8; KEY_LEN]);

impl ClusterKey {
    /// Wrap a raw 32-byte secret as a cluster key.
    pub fn new(bytes: [u8; KEY_LEN]) -> Self {
        ClusterKey(bytes)
    }

    fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// A MAC tag. Can only be produced by a [`Mac`] backend.
pub struct Tag([u8; TAG_LEN]);

impl Tag {
    fn as_bytes(&self) -> &[u8; TAG_LEN] {
        &self.0
    }
}

/// [`Payload`] state: cleared the MAC/AEAD gate, but not yet checked against the per-peer replay
/// filter. The only state [`Authenticator::open`] can produce.
pub struct Authenticated;

/// [`Payload`] state: cleared the replay filter too (or exempt because the connection runs
/// unauthenticated). The only state [`Payload::as_bytes`] accepts, and the only state message
/// handling (`handle_messages`) accepts — obtainable solely through
/// [`Payload::verify_replay`].
pub struct Verified;

/// A datagram payload that has cleared the authentication gate, obtainable only from
/// [`Authenticator::open`].
///
/// `State` is [`Authenticated`] or [`Verified`], so the order of the two checks is a compile-time
/// property. `seq`/`stamp` are [`Seq::NONE`]/[`Stamp::NONE`] in unauthenticated mode. [`Cow`]
/// because the MAC path borrows the receive buffer and the encrypted path owns a plaintext.
pub struct Payload<'a, State = Authenticated> {
    bytes: Cow<'a, [u8]>,
    /// Sender sequence number extracted from the replay header, or [`Seq::NONE`] in
    /// unauthenticated mode.
    pub seq: Seq,
    /// Sender wall-clock stamp from the replay header, or [`Stamp::NONE`] in unauthenticated
    /// mode.
    pub stamp: Stamp,
    _state: PhantomData<State>,
}

impl<'a> Payload<'a, Authenticated> {
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

/// Build a replay header: `seq (8 bytes LE) || stamp (8 bytes LE)`.
fn encode_replay_header(seq: Seq, stamp: Stamp) -> [u8; REPLAY_HEADER_LEN] {
    let mut header = [0u8; REPLAY_HEADER_LEN];
    header[..8].copy_from_slice(&seq.to_le_bytes());
    header[8..].copy_from_slice(&stamp.to_le_bytes());
    header
}

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

/// The keyed MAC primitive: one backend per `mac-*` feature, aliased as [`ClusterMac`].
pub trait Mac {
    /// Compute the authentication tag of `message` under `key`.
    fn tag(key: &ClusterKey, message: &[u8]) -> Tag;

    /// Constant-time check that `tag` authenticates `message` under `key`.
    ///
    /// `tag` is untrusted wire input; a wrong length yields `false`.
    fn verify(key: &ClusterKey, message: &[u8], tag: &[u8]) -> bool;
}

/// [`Mac`] backend keyed on BLAKE3, the default (`mac-blake3` feature).
#[cfg(feature = "mac-blake3")]
pub struct Blake3Mac;

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

/// [`Mac`] backend keyed on HMAC-SHA256 (`mac-hmac` feature).
#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
pub struct HmacSha256Mac;

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

// `mac-blake3` wins when both are enabled, so `--all-features` still compiles.
/// The [`Mac`] backend selected at compile time by the `mac-*` Cargo features.
#[cfg(feature = "mac-blake3")]
pub type ClusterMac = Blake3Mac;
/// The [`Mac`] backend selected at compile time by the `mac-*` Cargo features.
#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
pub type ClusterMac = HmacSha256Mac;

/// Authentication policy and datagram framing for one node: the sole producer of [`Payload`].
#[derive(Clone)]
pub enum Authenticator {
    /// No cluster key configured: the protocol runs unauthenticated.
    Disabled,
    /// A cluster key is configured: datagrams are MAC-sealed and verified (plaintext payload).
    Enabled(ClusterKey),
    /// A cluster key is configured and the `encryption` feature is active: datagrams are
    /// authenticated *and* encrypted with XChaCha20-Poly1305 over the cluster key.
    #[cfg(feature = "encryption")]
    Encrypted(ClusterKey),
}

impl Authenticator {
    /// Build an authenticator from an optional raw cluster key and whether to encrypt.
    ///
    /// # Panics
    ///
    /// If `encrypt` is `true` and the crate was built without the `encryption` feature — a loud
    /// failure rather than a silent downgrade.
    pub fn new(key: Option<[u8; KEY_LEN]>, encrypt: bool) -> Self {
        match (key, encrypt) {
            (None, _) => Authenticator::Disabled,
            (Some(bytes), false) => Authenticator::Enabled(ClusterKey::new(bytes)),
            #[cfg(feature = "encryption")]
            (Some(bytes), true) => Authenticator::Encrypted(ClusterKey::new(bytes)),
            #[cfg(not(feature = "encryption"))]
            (Some(_), true) => panic!(
                "reconcile: encryption requested but the crate was built without the \
                 `encryption` feature"
            ),
        }
    }

    /// Extra bytes a sealed datagram adds over the raw messages, for MTU accounting: crypto
    /// overhead plus the replay header.
    pub fn overhead(&self) -> usize {
        match self {
            Authenticator::Disabled => 0,
            Authenticator::Enabled(_) => TAG_LEN + REPLAY_HEADER_LEN,
            #[cfg(feature = "encryption")]
            Authenticator::Encrypted(_) => AEAD_NONCE_LEN + REPLAY_HEADER_LEN + AEAD_TAG_LEN,
        }
    }

    /// Frame an outgoing datagram, injecting the replay header.
    ///
    /// `None` when disabled: the caller sends `payload` unchanged.
    pub fn seal(&self, seq: Seq, stamp: Stamp, payload: &[u8]) -> Option<Vec<u8>> {
        match self {
            Authenticator::Disabled => None,
            Authenticator::Enabled(key) => {
                let header = encode_replay_header(seq, stamp);
                let mut protected = Vec::with_capacity(REPLAY_HEADER_LEN + payload.len());
                protected.extend_from_slice(&header);
                protected.extend_from_slice(payload);
                let tag = ClusterMac::tag(key, &protected);
                let mut framed = Vec::with_capacity(TAG_LEN + protected.len());
                framed.extend_from_slice(tag.as_bytes());
                framed.extend_from_slice(&protected);
                Some(framed)
            }
            #[cfg(feature = "encryption")]
            Authenticator::Encrypted(key) => {
                let header = encode_replay_header(seq, stamp);
                let mut plaintext = Vec::with_capacity(REPLAY_HEADER_LEN + payload.len());
                plaintext.extend_from_slice(&header);
                plaintext.extend_from_slice(payload);
                Some(encryption::seal(key, &plaintext))
            }
        }
    }

    /// Authenticate (and in encrypted mode decrypt) an incoming datagram.
    ///
    /// Produces [`Authenticated`], never [`Verified`]: the caller must still
    /// [`Payload::verify_replay`]. `None` on any failure, and the caller drops it silently.
    pub fn open<'a>(&self, datagram: &'a [u8]) -> Option<Payload<'a, Authenticated>> {
        match self {
            Authenticator::Disabled => Some(Payload {
                bytes: Cow::Borrowed(datagram),
                seq: Seq::NONE,
                stamp: Stamp::NONE,
                _state: PhantomData,
            }),
            Authenticator::Enabled(key) => {
                if datagram.len() < TAG_LEN + REPLAY_HEADER_LEN {
                    return None;
                }
                let (tag, protected) = datagram.split_at(TAG_LEN);
                if !ClusterMac::verify(key, protected, tag) {
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
            Authenticator::Encrypted(key) => {
                let plaintext = encryption::open(key, datagram)?;
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

/// XChaCha20-Poly1305 authenticated encryption over the cluster key.
#[cfg(feature = "encryption")]
mod encryption {
    use chacha20poly1305::aead::{Aead, OsRng};
    use chacha20poly1305::{AeadCore, Key, KeyInit, XChaCha20Poly1305, XNonce};

    use super::{ClusterKey, AEAD_NONCE_LEN, AEAD_TAG_LEN};

    fn cipher(key: &ClusterKey) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(key.as_bytes()))
    }

    /// Encrypt `payload`, returning `nonce || ciphertext || tag`.
    pub(super) fn seal(key: &ClusterKey, payload: &[u8]) -> Vec<u8> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        // Encryption only fails past multi-gigabyte plaintexts, unreachable in a datagram.
        let ciphertext = cipher(key)
            .encrypt(&nonce, payload)
            .expect("XChaCha20-Poly1305 encryption of a datagram-sized payload cannot fail");
        let mut framed = Vec::with_capacity(AEAD_NONCE_LEN + ciphertext.len());
        framed.extend_from_slice(nonce.as_slice());
        framed.extend_from_slice(&ciphertext);
        framed
    }

    /// Decrypt a `nonce || ciphertext || tag` datagram, returning the plaintext, or `None` if it is
    /// too short or fails authentication.
    pub(super) fn open(key: &ClusterKey, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() < AEAD_NONCE_LEN + AEAD_TAG_LEN {
            return None;
        }
        let (nonce, ciphertext) = datagram.split_at(AEAD_NONCE_LEN);
        cipher(key)
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use super::*;
    use crate::replay::{ReplayFilter, REPLAY_HEADER_LEN};

    fn key(byte: u8) -> ClusterKey {
        ClusterKey::new([byte; KEY_LEN])
    }

    /// Replay-check with a throwaway wide-open filter: these tests seal fixed near-epoch stamps,
    /// so the window must span the gap to real wall-clock time.
    fn verify(payload: Payload<'_, Authenticated>) -> Payload<'_, Verified> {
        let filter = ReplayFilter::new(Duration::from_secs(200 * 365 * 24 * 3600), true);
        let sender: IpAddr = "127.0.0.1".parse().unwrap();
        payload
            .verify_replay(&filter, sender)
            .expect("a freshly-sealed datagram must clear the replay check")
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

        let mut bad_payload = payload.clone();
        bad_payload[0] ^= 0x01;
        assert!(!ClusterMac::verify(&k, &bad_payload, t.as_bytes()));

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
        let auth = Authenticator::new(Some([0x11; KEY_LEN]), false);
        let payload = b"some serialized message";
        let sealed = auth
            .seal(Seq::new(1), Stamp::new(12345), payload)
            .expect("enabled");
        assert_eq!(sealed.len(), TAG_LEN + REPLAY_HEADER_LEN + payload.len());
        let p = verify(auth.open(&sealed).expect("should open"));
        assert_eq!(p.as_bytes(), payload);
        assert_eq!(p.seq, Seq::new(1));
        assert_eq!(p.stamp, Stamp::new(12345));
    }

    #[test]
    fn open_too_short() {
        let auth = Authenticator::new(Some([0x11; KEY_LEN]), false);
        assert!(auth.open(&[0u8; TAG_LEN + REPLAY_HEADER_LEN - 1]).is_none());
        assert!(auth.open(&[0u8; 10]).is_none());
        assert!(auth.open(&[]).is_none());
    }

    #[test]
    fn open_wrong_key() {
        let sealed = Authenticator::new(Some([0x11; KEY_LEN]), false)
            .seal(Seq::new(1), Stamp::new(99), b"payload")
            .expect("enabled");
        assert!(Authenticator::new(Some([0x22; KEY_LEN]), false)
            .open(&sealed)
            .is_none());
    }

    #[test]
    fn disabled_passes_through_and_does_not_seal() {
        let auth = Authenticator::new(None, false);
        assert!(matches!(auth, Authenticator::Disabled));
        assert_eq!(auth.overhead(), 0);
        assert!(auth.seal(Seq::new(0), Stamp::new(0), b"payload").is_none());
        let p = verify(
            auth.open(b"raw bytes")
                .expect("unauthenticated always clears"),
        );
        assert_eq!(p.as_bytes(), b"raw bytes");
        assert_eq!(p.seq, Seq::new(0));
        assert_eq!(p.stamp, Stamp::new(0));
    }

    /// The auth gate does not replay-check: a resealed datagram opens, carrying seq/stamp.
    #[test]
    fn replay_header_round_trips_seq_and_stamp() {
        let auth = Authenticator::new(Some([0xAB; KEY_LEN]), false);
        let payload = b"hello";
        let sealed = auth
            .seal(Seq::new(42), Stamp::new(9999), payload)
            .expect("enabled");
        let p = verify(auth.open(&sealed).expect("valid tag"));
        assert_eq!(p.seq, Seq::new(42));
        assert_eq!(p.stamp, Stamp::new(9999));
        assert_eq!(p.as_bytes(), payload);
    }

    #[cfg(feature = "encryption")]
    mod encryption {
        use super::*;

        fn encryptor(byte: u8) -> Authenticator {
            Authenticator::new(Some([byte; KEY_LEN]), true)
        }

        #[test]
        fn roundtrip_and_overhead() {
            let auth = encryptor(0x11);
            assert!(matches!(auth, Authenticator::Encrypted(_)));
            assert_eq!(
                auth.overhead(),
                super::AEAD_NONCE_LEN + REPLAY_HEADER_LEN + super::AEAD_TAG_LEN
            );

            let payload = b"some serialized message";
            let sealed = auth
                .seal(Seq::new(1), Stamp::new(555), payload)
                .expect("encrypted");
            assert_eq!(
                sealed.len(),
                super::AEAD_NONCE_LEN + REPLAY_HEADER_LEN + payload.len() + super::AEAD_TAG_LEN
            );
            let p = verify(auth.open(&sealed).expect("should decrypt"));
            assert_eq!(p.as_bytes(), payload);
            assert_eq!(p.seq, Seq::new(1));
            assert_eq!(p.stamp, Stamp::new(555));
        }

        #[test]
        fn ciphertext_hides_plaintext() {
            let payload = b"the quick brown fox jumps over the lazy dog";
            let sealed = encryptor(0x11)
                .seal(Seq::new(1), Stamp::new(0), payload)
                .expect("encrypted");
            assert!(!sealed
                .windows(payload.len())
                .any(|window| window == payload));
        }

        #[test]
        fn fresh_nonce_per_datagram() {
            // Random nonce: two identical messages must not be distinguishable as such.
            let auth = encryptor(0x11);
            let payload = b"identical payload";
            assert_ne!(
                auth.seal(Seq::new(1), Stamp::new(0), payload)
                    .expect("encrypted"),
                auth.seal(Seq::new(2), Stamp::new(0), payload)
                    .expect("encrypted")
            );
        }

        #[test]
        fn tamper_is_rejected() {
            let auth = encryptor(0x11);
            let mut sealed = auth
                .seal(Seq::new(1), Stamp::new(0), b"payload")
                .expect("encrypted");
            let last = sealed.len() - 1;
            sealed[last] ^= 0x01;
            assert!(auth.open(&sealed).is_none());
        }

        #[test]
        fn wrong_key_is_rejected() {
            let sealed = encryptor(0x11)
                .seal(Seq::new(1), Stamp::new(0), b"payload")
                .expect("encrypted");
            assert!(encryptor(0x22).open(&sealed).is_none());
        }

        #[test]
        fn truncated_is_rejected() {
            let auth = encryptor(0x11);
            assert!(auth
                .open(&[0u8; super::AEAD_NONCE_LEN + REPLAY_HEADER_LEN + super::AEAD_TAG_LEN - 1])
                .is_none());
            assert!(auth.open(&[]).is_none());
        }

        #[test]
        fn replay_header_survives_encryption() {
            let auth = encryptor(0x33);
            let sealed = auth
                .seal(Seq::new(77), Stamp::new(888888), b"data")
                .expect("encrypted");
            let p = verify(auth.open(&sealed).expect("should decrypt"));
            assert_eq!(p.seq, Seq::new(77));
            assert_eq!(p.stamp, Stamp::new(888888));
            assert_eq!(p.as_bytes(), b"data");
        }
    }
}
