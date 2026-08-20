// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The keyed MAC backends selected by the `mac-*` Cargo features (AGENTS.md §6), plus the tag
//! type they produce.

use super::{ClusterKey, TAG_LEN};

/// A MAC tag. Can only be produced by a [`Mac`] backend.
///
/// Crate-private, along with [`Mac`] itself — see [`Mac`]'s docs for why (#285).
pub(crate) struct Tag([u8; TAG_LEN]);

impl Tag {
    pub(super) fn as_bytes(&self) -> &[u8; TAG_LEN] {
        &self.0
    }
}

/// The keyed MAC primitive: one backend per `mac-*` feature, aliased as [`ClusterMac`].
///
/// Crate-private (#285): `Tag`'s field and `ClusterKey`'s bytes are both unreachable outside this
/// crate, so an external implementation could only ever be `todo!()` — a trait that looks
/// implementable and is not is worse than one that is honestly closed. Third-party MAC backends
/// are not a supported extension point; open this (and `Tag`/`ClusterKey::as_bytes`) deliberately,
/// with a compiling external example, if that changes.
pub(crate) trait Mac {
    /// Compute the authentication tag of `message` under `key`.
    fn tag(key: &ClusterKey, message: &[u8]) -> Tag;

    /// Constant-time check that `tag` authenticates `message` under `key`.
    ///
    /// `tag` is untrusted wire input; a wrong length yields `false`.
    fn verify(key: &ClusterKey, message: &[u8], tag: &[u8]) -> bool;
}

/// [`Mac`] backend keyed on BLAKE3, the default (`mac-blake3` feature).
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

/// [`Mac`] backend keyed on HMAC-SHA256 (`mac-hmac` feature).
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

// `mac-blake3` wins when both are enabled, so `--all-features` still compiles.
/// The [`Mac`] backend selected at compile time by the `mac-*` Cargo features.
#[cfg(feature = "mac-blake3")]
pub(crate) type ClusterMac = Blake3Mac;
/// The [`Mac`] backend selected at compile time by the `mac-*` Cargo features.
#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
pub(crate) type ClusterMac = HmacSha256Mac;
