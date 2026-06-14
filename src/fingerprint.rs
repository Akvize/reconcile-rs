// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Range fingerprint primitive used by the reconciliation protocol.
//!
//! The reconciliation protocol compares two collections by exchanging a
//! *fingerprint* (a combined hash) of the elements in a range of keys. For this
//! to be correct and safe, the fingerprint must satisfy two properties:
//!
//! 1. **Algebraically strong combiner.** Fingerprints of sub-ranges are
//!    combined into the fingerprint of their union, and elements are added and
//!    removed incrementally as the tree mutates. The naive combiner — a 64-bit
//!    XOR of per-element hashes — is `GF(2)`-linear and self-inverse: cancelling
//!    or repeated element hashes vanish, and an adversary can *solve* (Gaussian
//!    elimination) for crafted elements that make a divergent range collide in
//!    fingerprint, causing silent missed differences. 64 bits also invites
//!    accidental birthday collisions (~2³²) over a cluster's lifetime.
//!
//!    Instead we use a **256-bit "hash-then-add" combiner**: each element hashes
//!    to a 256-bit value and fingerprints combine by **addition modulo 2²⁵⁶**
//!    (with carry propagation across the whole 256-bit word). This forms an
//!    abelian group — combine is `+`, remove is `-` — and, unlike XOR, addition
//!    with carries is *not* `GF(2)`-linear, defeating offline collision crafting.
//!    The 256-bit width pushes accidental birthday collisions to ~2¹²⁸.
//!
//! 2. **Stable, versioned hash function.** The fingerprint is the **wire
//!    reconciliation token**: two nodes must compute the *same* fingerprint for
//!    the same data, forever, across Rust versions, platforms (32- vs 64-bit),
//!    and endianness. `std`'s [`DefaultHasher`](std::collections::hash_map::DefaultHasher)
//!    is explicitly documented as unspecified and unstable across releases, so
//!    we pin the element hash to **BLAKE3** and feed integers in fixed
//!    little-endian width (see the `Blake3Hasher` adapter). The golden-vector tests at the
//!    bottom of this module freeze the wire format so any change that would
//!    break interoperability fails CI.
//!
//! See: A. Meyer, *Range-Based Set Reconciliation*
//! (arXiv:2212.13567); Clarke et al., *Incremental Multiset Hash Functions*
//! (ASIACRYPT 2003).

use std::hash::{Hash, Hasher};
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use serde::{Deserialize, Serialize};

/// A 256-bit range fingerprint, stored as four little-endian 64-bit limbs
/// (limb 0 is least significant).
///
/// Fingerprints form an abelian group under addition modulo 2²⁵⁶:
/// [`combine`](Fingerprint::combine)/`+` merges the fingerprints of disjoint
/// ranges (and adds a single element), while `-` removes an element again. The
/// identity [`ZERO`](Fingerprint::ZERO) is the fingerprint of the empty range.
///
/// NOTE: a *non-empty* range can legitimately fingerprint to [`ZERO`](Fingerprint::ZERO) (elements
/// whose hashes sum to a multiple of 2²⁵⁶). The reconciliation protocol must
/// therefore never treat `fingerprint == ZERO` as "empty"; emptiness is decided
/// on the element count.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint(pub [u64; 4]);

impl Fingerprint {
    /// The fingerprint of the empty range and the additive identity.
    pub const ZERO: Fingerprint = Fingerprint([0; 4]);

    /// Interpret 32 bytes (little-endian) as a fingerprint.
    fn from_bytes(bytes: &[u8; 32]) -> Fingerprint {
        let mut limbs = [0u64; 4];
        for (limb, chunk) in limbs.iter_mut().zip(bytes.chunks_exact(8)) {
            *limb = u64::from_le_bytes(chunk.try_into().unwrap());
        }
        Fingerprint(limbs)
    }

    /// Combine two fingerprints (addition modulo 2²⁵⁶, with carry propagation).
    #[must_use]
    pub fn combine(self, other: Fingerprint) -> Fingerprint {
        let mut out = [0u64; 4];
        let mut carry = 0u128;
        for (o, (&a, &b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            let sum = a as u128 + b as u128 + carry;
            *o = sum as u64;
            carry = sum >> 64;
        }
        Fingerprint(out)
    }

    /// Remove `other` from `self` (subtraction modulo 2²⁵⁶); the inverse of
    /// [`combine`](Fingerprint::combine).
    #[must_use]
    pub fn remove(self, other: Fingerprint) -> Fingerprint {
        let mut out = [0u64; 4];
        let mut borrow = 0i128;
        for (o, (&a, &b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            let diff = a as i128 - b as i128 - borrow;
            if diff < 0 {
                *o = (diff + (1i128 << 64)) as u64;
                borrow = 1;
            } else {
                *o = diff as u64;
                borrow = 0;
            }
        }
        Fingerprint(out)
    }
}

impl Add for Fingerprint {
    type Output = Fingerprint;
    fn add(self, rhs: Fingerprint) -> Fingerprint {
        self.combine(rhs)
    }
}

impl AddAssign for Fingerprint {
    fn add_assign(&mut self, rhs: Fingerprint) {
        *self = self.combine(rhs);
    }
}

impl Sub for Fingerprint {
    type Output = Fingerprint;
    fn sub(self, rhs: Fingerprint) -> Fingerprint {
        self.remove(rhs)
    }
}

impl SubAssign for Fingerprint {
    fn sub_assign(&mut self, rhs: Fingerprint) {
        *self = self.remove(rhs);
    }
}

impl Neg for Fingerprint {
    type Output = Fingerprint;
    fn neg(self) -> Fingerprint {
        Fingerprint::ZERO.remove(self)
    }
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Most-significant limb first, so the hex reads like a big-endian number.
        write!(
            f,
            "Fingerprint({:016x}{:016x}{:016x}{:016x})",
            self.0[3], self.0[2], self.0[1], self.0[0]
        )
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{:016x}{:016x}{:016x}{:016x}",
            self.0[3], self.0[2], self.0[1], self.0[0]
        )
    }
}

/// A [`Hasher`] that feeds bytes into BLAKE3 and yields a 256-bit
/// [`Fingerprint`].
///
/// Integer `write_*` methods are overridden to emit a **fixed little-endian**
/// byte width, independent of the host endianness and pointer width, so that a
/// 32-bit and a 64-bit node (or a big-endian and a little-endian one) feed
/// BLAKE3 the exact same bytes for the same value. This is what makes the
/// fingerprint a stable wire token.
struct Blake3Hasher(blake3::Hasher);

impl Blake3Hasher {
    fn new() -> Blake3Hasher {
        Blake3Hasher(blake3::Hasher::new())
    }

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::from_bytes(self.0.finalize().as_bytes())
    }
}

impl Hasher for Blake3Hasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    // Fixed-width little-endian integer encodings (portable across platforms).
    fn write_u8(&mut self, i: u8) {
        self.0.update(&[i]);
    }
    fn write_u16(&mut self, i: u16) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_u32(&mut self, i: u32) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_u64(&mut self, i: u64) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_u128(&mut self, i: u128) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_usize(&mut self, i: usize) {
        // Encode as a fixed 64-bit value so 32- and 64-bit nodes agree.
        self.0.update(&(i as u64).to_le_bytes());
    }
    fn write_i8(&mut self, i: i8) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_i16(&mut self, i: i16) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_i32(&mut self, i: i32) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_i64(&mut self, i: i64) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_i128(&mut self, i: i128) {
        self.0.update(&i.to_le_bytes());
    }
    fn write_isize(&mut self, i: isize) {
        self.0.update(&(i as i64).to_le_bytes());
    }

    fn finish(&self) -> u64 {
        // Not used to build fingerprints (we call `fingerprint()` instead), but
        // the trait requires it; return the low 64 bits of the digest.
        let bytes = self.0.finalize();
        u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap())
    }
}

/// Compute the 256-bit [`Fingerprint`] of a single key-value element.
///
/// This is the per-element hash that the [`HRTree`](crate::hrtree::HRTree)
/// combines into range fingerprints. It is BLAKE3 over the key bytes followed by
/// the value bytes, fed in a fixed, portable encoding (see the `Blake3Hasher` adapter),
/// and is part of the wire protocol — see the golden-vector tests.
///
/// # Domain separation is the caller's burden
///
/// The key and the value are hashed into **one** BLAKE3 stream back-to-back, with **no length
/// prefix or separator** between them. The encoding is therefore only collision-free across the
/// key/value boundary when each type's [`Hash`] impl is **self-delimiting** — i.e. the byte stream
/// it writes unambiguously encodes its own length. If both `K` and `V` can emit variable-length
/// byte runs without a length marker, two *different* elements can hash identically because the
/// boundary shifts: `k1 ‖ v1 == k2 ‖ v2` (e.g. `("ab", "c")` and `("a", "bc")`). Two elements that
/// collide here are indistinguishable to the range-diff protocol, so a real difference can go
/// undetected and the replicas silently fail to converge on those keys.
///
/// The standard library's `Hash` impls used as keys/values here are self-delimiting and safe:
/// integers and other fixed-width primitives write a fixed number of bytes; slices/`Vec` prepend
/// their length (`Hash::hash` calls `write_usize(len)` first); and `str`/`String` write a trailing
/// `0xff` sentinel after the bytes. A **custom**
/// `Hash` impl is safe as long as it is self-delimiting; if it concatenates variable-length fields
/// without lengths, give it a self-delimiting impl (e.g. `#[derive(Hash)]`, or hash a length before
/// each variable-length field) before using the type as a `K` or `V`.
pub fn hash<K: Hash, V: Hash>(key: &K, value: &V) -> Fingerprint {
    let mut hasher = Blake3Hasher::new();
    key.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.fingerprint()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_identity() {
        let f = hash(&42u64, &"hello");
        assert_eq!(f + Fingerprint::ZERO, f);
        assert_eq!(f - Fingerprint::ZERO, f);
        assert_eq!(Fingerprint::ZERO + f, f);
    }

    #[test]
    fn add_then_remove_is_identity() {
        let a = hash(&1u64, &10u64);
        let b = hash(&2u64, &20u64);
        let c = hash(&3u64, &30u64);
        let combined = a + b + c;
        assert_eq!(combined - b - a - c, Fingerprint::ZERO);
        assert_eq!(combined - c, a + b);
    }

    #[test]
    fn add_is_commutative_and_associative() {
        let a = hash(&1u64, &10u64);
        let b = hash(&2u64, &20u64);
        let c = hash(&3u64, &30u64);
        assert_eq!(a + b, b + a);
        assert_eq!((a + b) + c, a + (b + c));
    }

    #[test]
    fn neg_is_additive_inverse() {
        let a = hash(&7u64, &"x");
        assert_eq!(a + (-a), Fingerprint::ZERO);
        assert_eq!(-(-a), a);
    }

    #[test]
    fn add_propagates_carry_across_limbs() {
        let all_ones = Fingerprint([u64::MAX; 4]);
        // (2²⁵⁶ - 1) + 1 wraps to 0.
        assert_eq!(all_ones + Fingerprint([1, 0, 0, 0]), Fingerprint::ZERO);
        // Carry out of limb 0 lands in limb 1.
        assert_eq!(
            Fingerprint([u64::MAX, 0, 0, 0]) + Fingerprint([1, 0, 0, 0]),
            Fingerprint([0, 1, 0, 0])
        );
    }

    #[test]
    fn sub_borrows_across_limbs() {
        // 0 - 1 wraps to 2²⁵⁶ - 1 (all limbs MAX).
        assert_eq!(
            Fingerprint::ZERO - Fingerprint([1, 0, 0, 0]),
            Fingerprint([u64::MAX; 4])
        );
    }

    // --- Golden vectors: freeze the wire format. ---
    //
    // These pin the exact bytes that go on the wire. If a change to the element
    // hash (BLAKE3, the feeding order/encoding) or the combiner ever alters
    // them, this test fails — that change would silently break interoperability
    // between nodes and must be a deliberate, versioned wire-format bump.

    #[test]
    fn golden_element_hash() {
        // BLAKE3(key=50u64 little-endian || value="Hello" || 0xff str terminator).
        assert_eq!(
            hash(&50u64, &"Hello"),
            Fingerprint([
                0x3a24_dc5c_8162_625b,
                0x8096_8c6a_b597_489b,
                0x105e_ba4e_6a69_90c8,
                0x4e24_03d1_e7ce_04f7,
            ])
        );
    }

    #[test]
    fn golden_combined_fingerprint() {
        // Order-independent combination of three elements (the building block of
        // a range fingerprint).
        let combined =
            hash(&25u64, &"World!") + hash(&50u64, &"Hello") + hash(&75u64, &"Everyone!");
        assert_eq!(
            combined,
            Fingerprint([
                0x6be8_b71e_bc22_e801,
                0x6c53_2dca_19b5_e70c,
                0x7422_4b2c_43a0_0032,
                0xacf7_6a81_40c9_c730,
            ])
        );
    }
}
