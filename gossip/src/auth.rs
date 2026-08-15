// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-datagram message authentication. Unauthenticated by default: README "Security model".
//!
//! # Wire layout
//!
//! Disabled: `version (1 B) || protocol_messages`
//!
//! MAC mode: `tag (32 B) || seq (8 B LE) || stamp (8 B LE) || version (1 B) || protocol_messages`
//!
//! Encryption mode: `nonce (24 B) || encrypt(seq (8 B LE) || stamp (8 B LE) || version (1 B) || protocol_messages) || tag (16 B)`
//!
//! The version byte sits **after** the replay header, not before it: `decode_replay_header`
//! parses a fixed 16-byte prefix, so putting anything ahead of it would need every existing
//! authenticated-mode offset recomputed. It always ends up as the first byte `Payload::bytes`
//! exposes, which is what lets [`Payload::check_version`] treat all three modes uniformly.
//!
//! The replay header sits inside the authenticated/encrypted region in both cases; the wire
//! version byte sits inside it too, and — unlike the replay header, which is absent when
//! disabled — is present on **every** datagram regardless of authentication mode: a
//! mixed-version cluster must be diagnosable whether or not a cluster key is configured, and
//! unauthenticated is the default (`ARCHITECTURE.md` §8).
//!
//! The layering carries the security invariants in the types: [`Authenticator`] is the sole
//! producer of a [`Payload`], and message handling consumes `Payload<`[`Verified`]`>`, obtainable
//! only from [`Payload::verify_replay`] — so authenticate-before-decode
//! (`ARCHITECTURE.md` §5 invariant 5) and check-replay-before-handle are both compile-time.
//! [`Payload::check_version`] is the mandatory step between the two: version-checking runs on
//! authenticated bytes (so a forged version claim is rejected the same way a forged payload is),
//! but ahead of replay bookkeeping (so a differently-versioned peer never consumes a replay-filter
//! slot over a datagram this build cannot even interpret).

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::net::IpAddr;

use crate::replay::{ReplayFilter, Seq, Stamp, REPLAY_HEADER_LEN};

/// Length in bytes of the authentication tag prepended to every datagram.
pub const TAG_LEN: usize = 32;

/// Length in bytes of a cluster key.
pub const KEY_LEN: usize = 32;

/// Length in bytes of the wire-version byte prepended to every datagram's protected region,
/// regardless of authentication mode.
pub const VERSION_LEN: usize = 1;

/// The wire protocol version this build produces and accepts.
///
/// Bumping it is the sanctioned way to make a non-additive change to the `Message` wire format:
/// a peer running a different version is rejected with a distinguishable, counted reason
/// (`reconcile_datagrams_dropped_total{reason="version"}`) rather than silently misread or
/// indistinguishably dropped as malformed. There is currently no accepted-version *window* — a
/// mismatch of any kind is rejected; widening that is a policy change to make deliberately, not a
/// side effect of the next bump.
pub const WIRE_VERSION: u8 = 1;

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
/// The public boundary (`Config::cluster_key`, `Authenticator::new`) takes and returns
/// `ClusterKey`, never a bare `[u8; 32]` — AGENTS.md §4: type-owned parsing, an invalid instance
/// structurally impossible to hand to either.
///
/// `Debug` is redacting: it never prints the key material, so an accidental `{:?}` in a log
/// statement cannot leak it.
#[cfg_attr(feature = "zeroize", derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop))]
#[derive(Clone)]
pub struct ClusterKey([u8; KEY_LEN]);

impl ClusterKey {
    /// Wrap a raw 32-byte secret as a cluster key.
    pub fn new(bytes: [u8; KEY_LEN]) -> Self {
        ClusterKey(bytes)
    }

    /// Parse a cluster key from `2 * KEY_LEN` (64) hex characters, case-insensitive.
    ///
    /// The one parse this type exists to own — see AGENTS.md §4 — rather than every caller
    /// hand-rolling `u8::from_str_radix` over byte pairs (as, until #286, `examples/k8s/main.rs`
    /// did for `RECONCILE_CLUSTER_KEY`).
    pub fn from_hex(hex: &str) -> Result<Self, ClusterKeyError> {
        if hex.len() != KEY_LEN * 2 {
            return Err(ClusterKeyError::WrongHexLength(hex.len()));
        }
        let mut bytes = [0u8; KEY_LEN];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| ClusterKeyError::InvalidHexDigit)?;
        }
        Ok(ClusterKey(bytes))
    }

    fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for ClusterKey {
    /// Redacted: never prints the key material, whatever the format flags.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ClusterKey").field(&"<redacted>").finish()
    }
}

impl TryFrom<&[u8]> for ClusterKey {
    type Error = ClusterKeyError;

    /// `bytes` must be exactly `KEY_LEN` (32) bytes long.
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        <[u8; KEY_LEN]>::try_from(bytes)
            .map(ClusterKey)
            .map_err(|_| ClusterKeyError::WrongByteLength(bytes.len()))
    }
}

/// Why constructing a [`ClusterKey`] from untrusted input failed.
#[derive(Debug, Eq, PartialEq)]
pub enum ClusterKeyError {
    /// [`ClusterKey::from_hex`] got a string that was not exactly `2 * KEY_LEN` (64) characters.
    WrongHexLength(usize),
    /// [`ClusterKey::from_hex`] got a character outside `[0-9a-fA-F]`.
    InvalidHexDigit,
    /// [`TryFrom<&[u8]>`](ClusterKey#impl-TryFrom<%26[u8]>-for-ClusterKey) got a slice that was
    /// not exactly `KEY_LEN` (32) bytes.
    WrongByteLength(usize),
}

impl fmt::Display for ClusterKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClusterKeyError::WrongHexLength(got) => write!(
                f,
                "cluster key must be {} hex characters, got {got}",
                KEY_LEN * 2
            ),
            ClusterKeyError::InvalidHexDigit => {
                write!(
                    f,
                    "cluster key hex string contains a non-hex-digit character"
                )
            }
            ClusterKeyError::WrongByteLength(got) => {
                write!(f, "cluster key must be {KEY_LEN} bytes, got {got}")
            }
        }
    }
}

impl std::error::Error for ClusterKeyError {}

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
    /// Strip and check the leading wire-version byte. Call this before
    /// [`verify_replay`](Self::verify_replay) — see the module doc for why the ordering matters.
    ///
    /// `Err(actual)` on a mismatch (or an empty payload, reported as version `0`), carrying the
    /// version the peer actually sent so the caller can log it. The caller should count this
    /// distinctly from an authentication failure: [`WIRE_VERSION`] mismatches are a mixed-version
    /// cluster, not an attack or a malformed datagram.
    pub fn check_version(self) -> Result<Self, u8> {
        let version = *self.bytes.first().unwrap_or(&0);
        if version != WIRE_VERSION {
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
    /// Build an authenticator from an optional cluster key and whether to encrypt.
    ///
    /// # Panics
    ///
    /// If `encrypt` is `true` and the crate was built without the `encryption` feature — a loud
    /// failure rather than a silent downgrade.
    pub fn new(key: Option<ClusterKey>, encrypt: bool) -> Self {
        match (key, encrypt) {
            (None, _) => Authenticator::Disabled,
            (Some(key), false) => Authenticator::Enabled(key),
            #[cfg(feature = "encryption")]
            (Some(key), true) => Authenticator::Encrypted(key),
            #[cfg(not(feature = "encryption"))]
            (Some(_), true) => panic!(
                "reconcile: encryption requested but the crate was built without the \
                 `encryption` feature"
            ),
        }
    }

    /// Extra bytes a sealed datagram adds over the raw messages, for MTU accounting: crypto
    /// overhead plus the replay header, plus the wire-version byte present in every mode.
    pub fn overhead(&self) -> usize {
        match self {
            Authenticator::Disabled => VERSION_LEN,
            Authenticator::Enabled(_) => TAG_LEN + VERSION_LEN + REPLAY_HEADER_LEN,
            #[cfg(feature = "encryption")]
            Authenticator::Encrypted(_) => {
                AEAD_NONCE_LEN + VERSION_LEN + REPLAY_HEADER_LEN + AEAD_TAG_LEN
            }
        }
    }

    /// Frame an outgoing datagram: inject the wire-version byte (every mode) and, when
    /// enabled, the replay header.
    pub fn seal(&self, seq: Seq, stamp: Stamp, payload: &[u8]) -> Vec<u8> {
        match self {
            Authenticator::Disabled => {
                let mut framed = Vec::with_capacity(VERSION_LEN + payload.len());
                framed.push(WIRE_VERSION);
                framed.extend_from_slice(payload);
                framed
            }
            Authenticator::Enabled(key) => {
                let header = encode_replay_header(seq, stamp);
                let mut protected =
                    Vec::with_capacity(REPLAY_HEADER_LEN + VERSION_LEN + payload.len());
                protected.extend_from_slice(&header);
                protected.push(WIRE_VERSION);
                protected.extend_from_slice(payload);
                let tag = ClusterMac::tag(key, &protected);
                let mut framed = Vec::with_capacity(TAG_LEN + protected.len());
                framed.extend_from_slice(tag.as_bytes());
                framed.extend_from_slice(&protected);
                framed
            }
            #[cfg(feature = "encryption")]
            Authenticator::Encrypted(key) => {
                let header = encode_replay_header(seq, stamp);
                let mut plaintext =
                    Vec::with_capacity(REPLAY_HEADER_LEN + VERSION_LEN + payload.len());
                plaintext.extend_from_slice(&header);
                plaintext.push(WIRE_VERSION);
                plaintext.extend_from_slice(payload);
                encryption::seal(key, &plaintext)
            }
        }
    }

    /// Authenticate (and in encrypted mode decrypt) an incoming datagram.
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

    /// The full receive-side pipeline (`ARCHITECTURE.md` §5 invariant 5, module doc): check the
    /// wire version, then replay-check with a throwaway wide-open filter — these tests seal fixed
    /// near-epoch stamps, so the window must span the gap to real wall-clock time.
    fn verify(payload: Payload<'_, Authenticated>) -> Payload<'_, Verified> {
        let filter = ReplayFilter::new(Duration::from_secs(200 * 365 * 24 * 3600), true);
        let sender: IpAddr = "127.0.0.1".parse().unwrap();
        payload
            .check_version()
            .expect("this module's own seal() always stamps the current wire version")
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
        let auth = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false);
        let payload = b"some serialized message";
        let sealed = auth.seal(Seq::new(1), Stamp::new(12345), payload);
        assert_eq!(
            sealed.len(),
            TAG_LEN + REPLAY_HEADER_LEN + VERSION_LEN + payload.len()
        );
        let p = verify(auth.open(&sealed).expect("should open"));
        assert_eq!(p.as_bytes(), payload);
        assert_eq!(p.seq, Seq::new(1));
        assert_eq!(p.stamp, Stamp::new(12345));
    }

    #[test]
    fn open_too_short() {
        let auth = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false);
        assert!(auth.open(&[0u8; TAG_LEN + REPLAY_HEADER_LEN - 1]).is_none());
        assert!(auth.open(&[0u8; 10]).is_none());
        assert!(auth.open(&[]).is_none());
    }

    #[test]
    fn open_wrong_key() {
        let sealed = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false).seal(
            Seq::new(1),
            Stamp::new(99),
            b"payload",
        );
        assert!(
            Authenticator::new(Some(ClusterKey::new([0x22; KEY_LEN])), false)
                .open(&sealed)
                .is_none()
        );
    }

    /// Disabled still frames — the wire-version byte is present regardless of
    /// authentication mode, since unauthenticated is the default.
    #[test]
    fn disabled_still_stamps_the_wire_version() {
        let auth = Authenticator::new(None, false);
        assert!(matches!(auth, Authenticator::Disabled));
        assert_eq!(auth.overhead(), VERSION_LEN);
        let sealed = auth.seal(Seq::new(0), Stamp::new(0), b"payload");
        assert_eq!(sealed, [&[WIRE_VERSION], &b"payload"[..]].concat());
        let p = verify(auth.open(&sealed).expect("unauthenticated always clears"));
        assert_eq!(p.as_bytes(), b"payload");
        assert_eq!(p.seq, Seq::new(0));
        assert_eq!(p.stamp, Stamp::new(0));
    }

    /// The auth gate does not replay-check: a resealed datagram opens, carrying seq/stamp.
    #[test]
    fn replay_header_round_trips_seq_and_stamp() {
        let auth = Authenticator::new(Some(ClusterKey::new([0xAB; KEY_LEN])), false);
        let payload = b"hello";
        let sealed = auth.seal(Seq::new(42), Stamp::new(9999), payload);
        let p = verify(auth.open(&sealed).expect("valid tag"));
        assert_eq!(p.seq, Seq::new(42));
        assert_eq!(p.stamp, Stamp::new(9999));
        assert_eq!(p.as_bytes(), payload);
    }

    /// A peer on a different wire version is rejected distinguishably from an
    /// authentication failure — `open` still succeeds (the MAC/decrypt is valid), only
    /// `check_version` fails, and it reports the version actually received.
    #[test]
    fn version_mismatch_is_distinguishable_from_auth_failure() {
        let auth = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false);
        let sealed = auth.seal(Seq::new(1), Stamp::new(0), b"payload");
        // Flip the version byte in place: it sits right after the replay header, ahead of the
        // payload (module doc's wire-layout table).
        let mut tampered = sealed.clone();
        let version_offset = TAG_LEN + REPLAY_HEADER_LEN;
        assert_eq!(tampered[version_offset], WIRE_VERSION);
        tampered[version_offset] = WIRE_VERSION + 1;
        // The MAC no longer verifies over the tampered bytes either, so re-tag it — this test is
        // about `check_version`, not about tamper detection (already covered above).
        let key = ClusterKey::new([0x11; KEY_LEN]);
        let protected = &tampered[TAG_LEN..];
        let tag = ClusterMac::tag(&key, protected);
        tampered[..TAG_LEN].copy_from_slice(tag.as_bytes());

        let opened = auth.open(&tampered).expect("MAC now verifies");
        match opened.check_version() {
            Err(version) => assert_eq!(version, WIRE_VERSION + 1),
            Ok(_) => panic!("expected a version mismatch"),
        }

        // The untampered datagram still opens and checks clean, proving the mismatch above is
        // about the version byte specifically, not some other corruption.
        assert!(auth
            .open(&sealed)
            .expect("should open")
            .check_version()
            .is_ok());
    }

    /// An empty authenticated payload reports version `0` rather than panicking on the missing
    /// byte — still a clean, distinguishable rejection, not a crash.
    #[test]
    fn empty_payload_reports_version_zero() {
        let auth = Authenticator::new(None, false);
        let opened = auth.open(b"").expect("unauthenticated always clears");
        match opened.check_version() {
            Err(version) => assert_eq!(version, 0),
            Ok(_) => panic!("expected a version mismatch"),
        }
    }

    #[cfg(feature = "encryption")]
    mod encryption {
        use super::*;

        fn encryptor(byte: u8) -> Authenticator {
            Authenticator::new(Some(ClusterKey::new([byte; KEY_LEN])), true)
        }

        #[test]
        fn roundtrip_and_overhead() {
            let auth = encryptor(0x11);
            assert!(matches!(auth, Authenticator::Encrypted(_)));
            assert_eq!(
                auth.overhead(),
                super::AEAD_NONCE_LEN
                    + super::VERSION_LEN
                    + REPLAY_HEADER_LEN
                    + super::AEAD_TAG_LEN
            );

            let payload = b"some serialized message";
            let sealed = auth.seal(Seq::new(1), Stamp::new(555), payload);
            assert_eq!(
                sealed.len(),
                super::AEAD_NONCE_LEN
                    + REPLAY_HEADER_LEN
                    + super::VERSION_LEN
                    + payload.len()
                    + super::AEAD_TAG_LEN
            );
            let p = verify(auth.open(&sealed).expect("should decrypt"));
            assert_eq!(p.as_bytes(), payload);
            assert_eq!(p.seq, Seq::new(1));
            assert_eq!(p.stamp, Stamp::new(555));
        }

        #[test]
        fn ciphertext_hides_plaintext() {
            let payload = b"the quick brown fox jumps over the lazy dog";
            let sealed = encryptor(0x11).seal(Seq::new(1), Stamp::new(0), payload);
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
                auth.seal(Seq::new(1), Stamp::new(0), payload),
                auth.seal(Seq::new(2), Stamp::new(0), payload)
            );
        }

        #[test]
        fn tamper_is_rejected() {
            let auth = encryptor(0x11);
            let mut sealed = auth.seal(Seq::new(1), Stamp::new(0), b"payload");
            let last = sealed.len() - 1;
            sealed[last] ^= 0x01;
            assert!(auth.open(&sealed).is_none());
        }

        #[test]
        fn wrong_key_is_rejected() {
            let sealed = encryptor(0x11).seal(Seq::new(1), Stamp::new(0), b"payload");
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
            let sealed = auth.seal(Seq::new(77), Stamp::new(888888), b"data");
            let p = verify(auth.open(&sealed).expect("should decrypt"));
            assert_eq!(p.seq, Seq::new(77));
            assert_eq!(p.stamp, Stamp::new(888888));
            assert_eq!(p.as_bytes(), b"data");
        }
    }
}
