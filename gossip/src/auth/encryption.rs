// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! XChaCha20-Poly1305 authenticated encryption over the cluster key (`encryption` feature),
//! shared by `seal` and `open`.

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
