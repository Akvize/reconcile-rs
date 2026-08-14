// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Range fingerprint primitive: `ARCHITECTURE.md` §5 invariant 1, §6.
//!
//! `[u64; 4]`, per-element BLAKE3 over the [canonical encoding](crate::encoding), combined by
//! addition mod 2²⁵⁶ — an abelian group whose carries are not `GF(2)`-linear, unlike the XOR
//! combiner it must never become. Hash function *and* input encoding are both pinned here; either
//! one changing is a wire break, frozen by this module's golden vectors.
//!
//! Non-`GF(2)`-linearity defeats the linear-algebra collision search that sinks XOR, but it is **not**
//! collision resistance against a *chosen-input* (writing) adversary: finding a colliding multiset is
//! Wagner's balance problem over `ℤ/2²⁵⁶`, solvable in ~2³¹ work (a subexponential k-tree, no error
//! term — carries never disturb a matched low window). So this fingerprint is sound in the honest
//! model but forgeable by anyone who can write, unless the lift is *keyed*. Demonstrated against the
//! RBSR driver in `rbsr/tests/wagner_false_convergence.rs`.
//!
//! Meyer, arXiv:2212.13567; Clarke et al., *Incremental Multiset Hash Functions* (ASIACRYPT 2003).

use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use serde::{Deserialize, Serialize};

use crate::encoding;

/// A 256-bit range fingerprint: four little-endian 64-bit limbs, limb 0 least significant.
///
/// An abelian group under addition mod 2²⁵⁶ — `+`/[`combine`](Fingerprint::combine) merges
/// disjoint ranges, `-` removes, [`ZERO`](Fingerprint::ZERO) is the identity.
///
/// A non-empty range can fingerprint to [`ZERO`](Fingerprint::ZERO); never decide emptiness on
/// the fingerprint, only on the element count.
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

/// A BLAKE3 accumulator fed exclusively through the [canonical encoding](crate::encoding).
struct Blake3Hasher(blake3::Hasher);

impl Blake3Hasher {
    fn new() -> Blake3Hasher {
        Blake3Hasher(blake3::Hasher::new())
    }

    /// Absorb `value`'s canonical encoding.
    ///
    /// # Panics
    ///
    /// Only if a hand-written [`Serialize`] impl fails — surfaced loudly, never folded into a
    /// wrong fingerprint.
    fn absorb<T: Serialize + ?Sized>(&mut self, value: &T) {
        encoding::encode_into(&mut self.0, value).expect("canonical encoding cannot fail");
    }

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::from_bytes(self.0.finalize().as_bytes())
    }
}

/// Def. 3.4's lifting function `lift: U → M`: BLAKE3 over the
/// [canonically encoded](crate::encoding) key followed by the canonically encoded value.
///
/// Part of the wire protocol — see this module's golden vectors. The [`Serialize`] bound admits
/// keys and values std implements no [`Hash`](std::hash::Hash) for
/// ([`HashMap`](std::collections::HashMap), [`HashSet`](std::collections::HashSet)).
pub fn lift<K: Serialize + ?Sized, V: Serialize + ?Sized>(key: &K, value: &V) -> Fingerprint {
    let mut hasher = Blake3Hasher::new();
    hasher.absorb(key);
    hasher.absorb(value);
    hasher.fingerprint()
}

/// The canonical 256-bit digest of a single value — [`lift`] with no key half, same encoding.
pub fn digest<T: Serialize + ?Sized>(value: &T) -> Fingerprint {
    let mut hasher = Blake3Hasher::new();
    hasher.absorb(value);
    hasher.fingerprint()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_identity() {
        let f = lift(&42u64, &"hello");
        assert_eq!(f + Fingerprint::ZERO, f);
        assert_eq!(f - Fingerprint::ZERO, f);
        assert_eq!(Fingerprint::ZERO + f, f);
    }

    #[test]
    fn add_then_remove_is_identity() {
        let a = lift(&1u64, &10u64);
        let b = lift(&2u64, &20u64);
        let c = lift(&3u64, &30u64);
        let combined = a + b + c;
        assert_eq!(combined - b - a - c, Fingerprint::ZERO);
        assert_eq!(combined - c, a + b);
    }

    #[test]
    fn add_is_commutative_and_associative() {
        let a = lift(&1u64, &10u64);
        let b = lift(&2u64, &20u64);
        let c = lift(&3u64, &30u64);
        assert_eq!(a + b, b + a);
        assert_eq!((a + b) + c, a + (b + c));
    }

    #[test]
    fn neg_is_additive_inverse() {
        let a = lift(&7u64, &"x");
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

    // Golden vectors: changing these is a wire break, not a refactor.

    #[test]
    fn golden_element_hash() {
        assert_eq!(
            lift(&50u64, &"Hello"),
            Fingerprint([
                0x5983_c089_4de2_aacf,
                0xa3b7_5857_a517_c2a4,
                0xf30c_219d_d2d5_d655,
                0xc269_e4a2_cb9e_3aa1,
            ])
        );
    }

    #[test]
    fn golden_combined_fingerprint() {
        let combined =
            lift(&25u64, &"World!") + lift(&50u64, &"Hello") + lift(&75u64, &"Everyone!");
        assert_eq!(
            combined,
            Fingerprint([
                0x44d8_8232_ba37_b808,
                0x3917_4386_159c_3900,
                0xd744_1273_6509_2edc,
                0x0d4a_f5d8_5402_598c,
            ])
        );
    }

    // Encoding properties as the protocol sees them; `encoding`'s tests check the bytes.

    #[test]
    fn framing_is_unambiguous() {
        assert_ne!(lift(&"ab", &"c"), lift(&"a", &"bc"));
        assert_ne!(
            lift(&0u8, &vec![vec![1u8, 2]]),
            lift(&0u8, &vec![vec![1u8], vec![2u8]])
        );
        assert_ne!(lift(&vec![1u8, 2], &()), lift(&(vec![1u8], vec![2u8]), &()));
    }

    #[test]
    fn hash_maps_fingerprint_independently_of_insertion_order() {
        use std::collections::{BTreeMap, HashMap};

        let mut forward = HashMap::new();
        for i in 0..16u32 {
            forward.insert(i, i * 3);
        }
        let mut backward = HashMap::new();
        for i in (0..16u32).rev() {
            backward.insert(i, i * 3);
        }
        assert_eq!(lift(&0u8, &forward), lift(&0u8, &backward));

        let ordered: BTreeMap<u32, u32> = forward.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(lift(&0u8, &forward), lift(&0u8, &ordered));
    }

    #[test]
    fn enum_variants_are_distinguished_by_index() {
        #[derive(Serialize)]
        enum Payload {
            A(u32),
            B(u32),
        }
        assert_ne!(
            lift(&0u8, &Payload::A(7)),
            lift(&0u8, &Payload::B(7)),
            "same payload under different variants must differ"
        );
    }

    #[test]
    fn none_does_not_collide_with_a_value() {
        assert_ne!(lift(&0u8, &None::<u8>), lift(&0u8, &Some(0u8)));
        assert_ne!(
            lift(&0u8, &None::<Vec<u8>>),
            lift(&0u8, &Some(Vec::<u8>::new()))
        );
        assert_ne!(
            lift(&0u8, &Some(None::<u8>)),
            lift(&0u8, &None::<Option<u8>>)
        );

        // Injectivity holds within a type, not across types.
        assert_eq!(lift(&0u8, &None::<u8>), lift(&0u8, &0u8));
    }

    #[test]
    fn integers_of_different_widths_differ() {
        assert_ne!(lift(&0u8, &1u32), lift(&0u8, &1u64));
        assert_ne!(lift(&0u8, &1u8), lift(&0u8, &1u16));
        assert_eq!(lift(&0u8, &1u32), lift(&0u8, &1i32));
    }

    #[test]
    fn floats_follow_bit_patterns_not_partial_eq() {
        assert_ne!(lift(&0u8, &0.0f64), lift(&0u8, &-0.0f64));
        assert_eq!(lift(&0u8, &f64::NAN), lift(&0u8, &f64::NAN));
    }

    #[test]
    fn digest_is_lift_without_a_key_half() {
        assert_eq!(digest(&"Hello"), lift(&(), &"Hello"));
        assert_ne!(digest(&"Hello"), digest(&"Hell"));
    }
}
