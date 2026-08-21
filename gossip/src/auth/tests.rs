// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::IpAddr;
use std::time::Duration;

use super::mac::{ClusterMac, Mac};
use super::*;
use crate::replay::{ReplayFilter, REPLAY_HEADER_LEN};

fn key(byte: u8) -> ClusterKey {
    ClusterKey::new([byte; KEY_LEN])
}

#[test]
fn from_hex_decodes_each_byte_pair_at_its_own_position() {
    let expected: Vec<u8> = (0..KEY_LEN as u8).collect();
    let hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
    let decoded = ClusterKey::from_hex(&hex).expect("64 valid hex chars must parse");
    assert_eq!(decoded.as_bytes().as_slice(), expected.as_slice());
}

#[test]
fn from_hex_rejects_wrong_length() {
    let too_short = "a".repeat(KEY_LEN * 2 - 1);
    assert_eq!(
        ClusterKey::from_hex(&too_short).unwrap_err(),
        ClusterKeyError::WrongHexLength(KEY_LEN * 2 - 1)
    );
}

#[test]
fn from_hex_rejects_a_non_hex_digit() {
    let mut hex = "a".repeat(KEY_LEN * 2);
    hex.replace_range(0..1, "g");
    assert_eq!(
        ClusterKey::from_hex(&hex).unwrap_err(),
        ClusterKeyError::InvalidHexDigit
    );
}

#[test]
fn debug_never_prints_key_material() {
    let k = key(0xAB);
    assert_eq!(format!("{k:?}"), "ClusterKey(\"<redacted>\")");
}

#[test]
fn cluster_key_error_display_messages() {
    assert_eq!(
        ClusterKeyError::WrongHexLength(10).to_string(),
        "cluster key must be 64 hex characters, got 10"
    );
    assert_eq!(
        ClusterKeyError::InvalidHexDigit.to_string(),
        "cluster key hex string contains a non-hex-digit character"
    );
    assert_eq!(
        ClusterKeyError::WrongByteLength(5).to_string(),
        "cluster key must be 32 bytes, got 5"
    );
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

/// #285/#137's rollout: a receiver mid-rotation (`primary` = new key, `also_accept` = [old
/// key]) still verifies a sender that has not yet moved off the old key, and still seals with
/// the new one. Once `also_accept` is empty again, the old key is rejected.
#[test]
fn also_accept_verifies_a_sender_still_on_the_old_key() {
    let old_key = key(0x11);
    let new_key = key(0x22);

    let old_sender = Authenticator::new(Some(old_key.clone()), false);
    let rotating_receiver = Authenticator::with_rotation(
        Some(Keys {
            primary: new_key.clone(),
            also_accept: vec![old_key.clone()],
        }),
        false,
    );

    let payload = b"mid-rotation message";
    let sealed_with_old_key = old_sender.seal(Seq::new(1), Stamp::new(1), payload);
    let p = verify(
        rotating_receiver
            .open(&sealed_with_old_key)
            .expect("also_accept must still verify the old key"),
    );
    assert_eq!(p.as_bytes(), payload);

    // The receiver still seals with `primary`, the new key, not an `also_accept` one.
    let sealed_by_receiver = rotating_receiver.seal(Seq::new(2), Stamp::new(2), payload);
    assert!(
        !ClusterMac::verify(
            &old_key,
            &sealed_by_receiver[TAG_LEN..],
            &sealed_by_receiver[..TAG_LEN]
        ),
        "seal() must never use an also_accept key"
    );
    assert!(ClusterMac::verify(
        &new_key,
        &sealed_by_receiver[TAG_LEN..],
        &sealed_by_receiver[..TAG_LEN]
    ));

    // Rotation complete: the old key is no longer accepted.
    let settled_receiver = Authenticator::new(Some(new_key), false);
    assert!(settled_receiver.open(&sealed_with_old_key).is_none());
}

#[test]
fn open_too_short() {
    let auth = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false);
    assert!(auth.open(&[0u8; TAG_LEN + REPLAY_HEADER_LEN - 1]).is_none());
    assert!(auth.open(&[0u8; 10]).is_none());
    assert!(auth.open(&[]).is_none());
}

/// `open_too_short`'s `TAG_LEN + REPLAY_HEADER_LEN - 1` case sits just below the real threshold
/// (48), too close to also catch the sum itself going wrong: `TAG_LEN - REPLAY_HEADER_LEN` (16)
/// rejects the same way for every length this crate's other tests use. A length strictly between
/// the two (16..48, here 20) is short enough that a correct datagram must still be rejected by
/// the length check alone, but long enough that a broken sum would let it fall through to
/// `datagram.split_at(TAG_LEN)` and panic on the out-of-bounds split instead of returning `None`.
#[test]
fn open_rejects_a_datagram_between_the_two_length_thresholds() {
    let auth = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false);
    assert!(auth.open(&[0u8; 20]).is_none());
}

/// The flip side of `open_too_short`: a datagram of exactly `TAG_LEN + REPLAY_HEADER_LEN` bytes
/// (a valid tag over a replay header with no trailing message bytes at all) must still open —
/// the length check is a strict `<`, not `<=`.
#[test]
fn open_accepts_the_exact_minimum_length_datagram() {
    let k = key(0x11);
    let seq = Seq::new(7);
    let stamp = Stamp::new(9);
    let mut protected = Vec::new();
    protected.extend_from_slice(&seq.to_le_bytes());
    protected.extend_from_slice(&stamp.to_le_bytes());
    assert_eq!(protected.len(), REPLAY_HEADER_LEN);
    let tag = ClusterMac::tag(&k, &protected);
    let mut datagram = Vec::new();
    datagram.extend_from_slice(tag.as_bytes());
    datagram.extend_from_slice(&protected);
    assert_eq!(datagram.len(), TAG_LEN + REPLAY_HEADER_LEN);

    let auth = Authenticator::new(Some(k), false);
    let opened = auth
        .open(&datagram)
        .expect("exactly TAG_LEN + REPLAY_HEADER_LEN bytes with a valid tag must open");
    assert_eq!(opened.seq, seq);
    assert_eq!(opened.stamp, stamp);
    assert!(opened.bytes.is_empty());
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

/// `overhead()` for `Enabled` is a plain sum of three independent constants — assert the exact
/// value (not just that it is `> 0`) so a `+`/`-` slip in any one term is caught.
#[test]
fn enabled_overhead_is_the_sum_of_tag_version_and_replay_header() {
    let auth = Authenticator::new(Some(ClusterKey::new([0x11; KEY_LEN])), false);
    assert_eq!(auth.overhead(), TAG_LEN + VERSION_LEN + REPLAY_HEADER_LEN);
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
            super::AEAD_NONCE_LEN + super::VERSION_LEN + REPLAY_HEADER_LEN + super::AEAD_TAG_LEN
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

    /// The flip side of `truncated_is_rejected`: a datagram of exactly `AEAD_NONCE_LEN +
    /// AEAD_TAG_LEN` bytes — an encrypted *empty* payload, nonce + tag and nothing else — must
    /// still decrypt. The length check in `encryption::open` is a strict `<`, not `<=`.
    #[test]
    fn open_accepts_the_exact_minimum_length_datagram() {
        let k = key(0x11);
        let sealed = crate::auth::encryption::seal(&k, b"");
        assert_eq!(sealed.len(), super::AEAD_NONCE_LEN + super::AEAD_TAG_LEN);
        assert_eq!(crate::auth::encryption::open(&k, &sealed), Some(Vec::new()));
    }

    /// As `open_rejects_a_datagram_between_the_two_length_thresholds` (the plaintext-MAC path's
    /// own version of this): `AEAD_NONCE_LEN - AEAD_TAG_LEN` (8) rejects a too-short datagram for
    /// the same reason `AEAD_NONCE_LEN + AEAD_TAG_LEN` (40) does whenever the length used is
    /// below both, so a length strictly between them (8..40, here 15) is needed to catch the sum
    /// itself going wrong — a broken `-` would let it fall through to
    /// `datagram.split_at(AEAD_NONCE_LEN)` and panic instead of returning `None`.
    #[test]
    fn open_rejects_a_datagram_between_the_two_length_thresholds() {
        let k = key(0x11);
        assert_eq!(crate::auth::encryption::open(&k, &[0u8; 15]), None);
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

    /// As `also_accept_verifies_a_sender_still_on_the_old_key`, over the encrypted path
    /// (#285/#137).
    #[test]
    fn also_accept_decrypts_a_sender_still_on_the_old_key() {
        let old_key = key(0x11);
        let new_key = key(0x22);

        let old_sender = Authenticator::new(Some(old_key.clone()), true);
        let rotating_receiver = Authenticator::with_rotation(
            Some(Keys {
                primary: new_key,
                also_accept: vec![old_key],
            }),
            true,
        );

        let payload = b"mid-rotation encrypted message";
        let sealed = old_sender.seal(Seq::new(1), Stamp::new(1), payload);
        let p = verify(
            rotating_receiver
                .open(&sealed)
                .expect("also_accept must still decrypt under the old key"),
        );
        assert_eq!(p.as_bytes(), payload);
    }
}
