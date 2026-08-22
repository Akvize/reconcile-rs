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
//!
//! Split across siblings by concern: `mac` owns the crate-private `Mac` backend trait, its two
//! `mac-*`-feature-gated implementations, and the `Tag` they produce; `key` owns
//! [`ClusterKey`]/[`ClusterKeyError`]/[`Keys`] construction and [`Authenticator`]'s own
//! construction and size accounting; `seal` and `open` each own one direction of the wire
//! (encrypt+authenticate on send, verify+decrypt on receive); `encryption` (feature `encryption`)
//! owns the XChaCha20-Poly1305 AEAD primitive both directions call into. This file keeps the
//! public type definitions (their module location is their `cargo public-api`-visible path — see
//! AGENTS.md §11) plus the module doc above every sibling shares.

use std::borrow::Cow;
use std::marker::PhantomData;

use crate::replay::{Seq, Stamp};

#[cfg(feature = "encryption")]
mod encryption;
mod key;
mod mac;
mod open;
mod seal;

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
pub const WIRE_VERSION: u8 = 2;

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
///
/// ```
/// use reconcile_gossip::auth::ClusterKey;
///
/// let key = ClusterKey::from_hex(
///     "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
/// )
/// .unwrap();
///
/// // Debug never prints the key material, even by accident.
/// assert_eq!(format!("{key:?}"), "ClusterKey(\"<redacted>\")");
///
/// // A key that isn't exactly 64 hex characters is rejected, not silently truncated/padded.
/// assert!(ClusterKey::from_hex("too short").is_err());
/// ```
#[cfg_attr(feature = "zeroize", derive(zeroize::Zeroize, zeroize::ZeroizeOnDrop))]
#[derive(Clone)]
pub struct ClusterKey([u8; KEY_LEN]);

/// Why constructing a [`ClusterKey`] from untrusted input failed.
///
/// ```
/// use reconcile_gossip::auth::{ClusterKey, ClusterKeyError};
///
/// let err = ClusterKey::from_hex("too short").unwrap_err();
/// assert_eq!(err, ClusterKeyError::WrongHexLength(9));
/// assert_eq!(
///     err.to_string(),
///     "cluster key must be 64 hex characters, got 9"
/// );
/// ```
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
#[derive(Debug)]
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

/// A [`ClusterKey`] this node seals outgoing datagrams with, plus zero or more additional keys it
/// still accepts on the verify path (#285) — the shape a rotation needs: roll out `also_accept:
/// [old_key]` cluster-wide, then once every peer has it, roll `primary` to the new key with the
/// old one demoted to `also_accept`, then finally drop it once every peer is on the new primary.
#[derive(Clone, Debug)]
pub struct Keys {
    /// The key `seal` always uses.
    pub primary: ClusterKey,
    /// Additional keys `open` accepts, tried in order after `primary` fails. Empty outside a
    /// rotation.
    pub also_accept: Vec<ClusterKey>,
}

/// Authentication policy and datagram framing for one node: the sole producer of [`Payload`].
#[derive(Clone, Debug)]
pub enum Authenticator {
    /// No cluster key configured: the protocol runs unauthenticated.
    Disabled,
    /// A cluster key is configured: datagrams are MAC-sealed and verified (plaintext payload).
    Enabled(Keys),
    /// A cluster key is configured and the `encryption` feature is active: datagrams are
    /// authenticated *and* encrypted with XChaCha20-Poly1305 over the cluster key.
    #[cfg(feature = "encryption")]
    Encrypted(Keys),
}

#[cfg(test)]
mod tests;
