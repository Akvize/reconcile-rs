// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Per-datagram message authentication for the reconciliation protocol.
//!
//! The UDP protocol performs no authentication by itself: any host that can
//! send a datagram to the port can forge an update and poison the whole
//! cluster through last-write-wins (see the crate-level "Security model"
//! documentation). To close that vector, when a cluster key is configured
//! (see [`Config::with_cluster_key`](crate::reconcile_store::Config::with_cluster_key)),
//! every outgoing datagram is framed as `tag || payload` where `tag` is a
//! 32-byte keyed MAC over `payload`, and every incoming datagram is verified
//! **before** any deserialization. Datagrams whose tag is missing or invalid
//! are silently dropped.
//!
//! The MAC primitive is abstracted behind a Cargo feature so it can be swapped
//! without touching the protocol: `mac-blake3` (default, keyed BLAKE3) or
//! `mac-hmac` (HMAC-SHA256). All nodes in a cluster must use the same key and
//! the same backend.

/// Length in bytes of the authentication tag prepended to every datagram.
pub(crate) const TAG_LEN: usize = 32;

/// Length in bytes of a cluster key.
pub(crate) const KEY_LEN: usize = 32;

#[cfg(not(any(feature = "mac-blake3", feature = "mac-hmac")))]
compile_error!(
    "reconcile: no MAC backend selected. Enable feature `mac-blake3` (default) or `mac-hmac`."
);

// `mac-blake3` takes precedence when both backends are enabled (e.g. under
// `--all-features`), so that such builds still compile instead of hitting a
// hard error.
#[cfg(feature = "mac-blake3")]
mod backend {
    use super::{KEY_LEN, TAG_LEN};

    pub fn tag(key: &[u8; KEY_LEN], payload: &[u8]) -> [u8; TAG_LEN] {
        *blake3::keyed_hash(key, payload).as_bytes()
    }

    pub fn verify(key: &[u8; KEY_LEN], payload: &[u8], tag: &[u8]) -> bool {
        let Ok(tag) = <[u8; TAG_LEN]>::try_from(tag) else {
            return false;
        };
        // `blake3::Hash`'s `PartialEq` is constant-time.
        blake3::keyed_hash(key, payload) == blake3::Hash::from_bytes(tag)
    }
}

#[cfg(all(feature = "mac-hmac", not(feature = "mac-blake3")))]
mod backend {
    use super::{KEY_LEN, TAG_LEN};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    pub fn tag(key: &[u8; KEY_LEN], payload: &[u8]) -> [u8; TAG_LEN] {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(payload);
        // SHA-256 produces exactly 32 bytes, matching TAG_LEN: no truncation.
        mac.finalize().into_bytes().into()
    }

    pub fn verify(key: &[u8; KEY_LEN], payload: &[u8], tag: &[u8]) -> bool {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(payload);
        // `verify_slice` is constant-time and length-checked.
        mac.verify_slice(tag).is_ok()
    }
}

/// Compute a 32-byte authentication tag over `payload` under `key`.
pub(crate) fn tag(key: &[u8; KEY_LEN], payload: &[u8]) -> [u8; TAG_LEN] {
    backend::tag(key, payload)
}

/// Constant-time verification that `tag` authenticates `payload` under `key`.
pub(crate) fn verify(key: &[u8; KEY_LEN], payload: &[u8], tag: &[u8]) -> bool {
    backend::verify(key, payload, tag)
}

/// Frame an outgoing datagram as `tag(payload) || payload`.
pub(crate) fn seal(key: &[u8; KEY_LEN], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(TAG_LEN + payload.len());
    out.extend_from_slice(&tag(key, payload));
    out.extend_from_slice(payload);
    out
}

/// Verify and strip a received datagram framed as `tag || payload`.
///
/// Returns `Some(payload)` if the datagram is long enough and the tag is valid,
/// `None` otherwise (too short or invalid tag).
pub(crate) fn open<'a>(key: &[u8; KEY_LEN], datagram: &'a [u8]) -> Option<&'a [u8]> {
    if datagram.len() < TAG_LEN {
        return None;
    }
    let (tag, payload) = datagram.split_at(TAG_LEN);
    if verify(key, payload, tag) {
        Some(payload)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: [u8; KEY_LEN] = [0x11; KEY_LEN];
    const K2: [u8; KEY_LEN] = [0x22; KEY_LEN];

    #[test]
    fn tag_verify_roundtrip() {
        let t = tag(&K1, b"hello world");
        assert!(verify(&K1, b"hello world", &t));
    }

    #[test]
    fn tamper_detection() {
        let payload = b"the quick brown fox".to_vec();
        let t = tag(&K1, &payload);

        // Flip a payload byte.
        let mut bad_payload = payload.clone();
        bad_payload[0] ^= 0x01;
        assert!(!verify(&K1, &bad_payload, &t));

        // Flip a tag byte.
        let mut bad_tag = t;
        bad_tag[0] ^= 0x01;
        assert!(!verify(&K1, &payload, &bad_tag));
    }

    #[test]
    fn wrong_key_rejected() {
        let t = tag(&K1, b"payload");
        assert!(!verify(&K2, b"payload", &t));
    }

    #[test]
    fn seal_open_roundtrip() {
        let payload = b"some serialized message".to_vec();
        let sealed = seal(&K1, &payload);
        assert_eq!(sealed.len(), TAG_LEN + payload.len());
        assert_eq!(open(&K1, &sealed), Some(payload.as_slice()));
    }

    #[test]
    fn open_too_short() {
        // Fewer than TAG_LEN bytes can never carry a valid tag.
        assert_eq!(open(&K1, &[0u8; 10]), None);
        assert_eq!(open(&K1, &[]), None);
    }

    #[test]
    fn open_wrong_key() {
        let sealed = seal(&K1, b"payload");
        assert_eq!(open(&K2, &sealed), None);
    }
}
