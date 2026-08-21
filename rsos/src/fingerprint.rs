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
//! Wagner's balance problem over `ℤ/2²⁵⁶`, which a k-tree solves in subexponential time — reduction
//! mod `2^j` is a group homomorphism, so merging on low-order bits is exact and carries never disturb
//! a matched window. **What this type guarantees is therefore honest-model soundness, not
//! unforgeability**: anyone who can write to a replica can craft a collision. Demonstrated against
//! the RBSR driver in `rbsr/tests/wagner_false_convergence.rs`. What it would cost an attacker, and
//! which combiner change would restore resistance, are tracked with the finding rather than here.
//!
//! **The collision bound assumes a set, not a multiset**: every statement above holds only if
//! each live element is folded in exactly once. `FingerprintTreeMap::insert` on an already-present
//! key applies a signed `new_fp - old_fp` delta rather than a blind `combine` (the type's sole
//! mutation sink for the cached aggregate), so update-in-place, persistence reload and duplicate
//! wire delivery all stay single-fold; the latter is pinned by
//! `tests/proptest_fingerprint_tree_map/btreemap_oracle.rs`'s duplicate-delivery property. Under a genuine
//! multiplicity (an element folded `c` times without a matching retraction) the bound degrades to
//! `2^-(w - v₂(c))`, and vanishes outright under a `GF(2)`-linear combiner.
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
///
/// ```
/// use rsos::{lift, Fingerprint};
///
/// let a = lift(&1, &"one");
/// let b = lift(&2, &"two");
///
/// // combine/remove are inverses -- this is what lets a range's fingerprint be maintained
/// // incrementally as elements are inserted and removed, rather than rehashed from scratch.
/// let combined = a.combine(b);
/// assert_eq!(combined.remove(b), a);
/// assert_eq!(combined.remove(a).remove(b), Fingerprint::ZERO);
/// ```
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Fingerprint(pub [u64; 4]);

impl Fingerprint {
    /// The fingerprint of the empty range and the additive identity.
    pub const ZERO: Fingerprint = Fingerprint([0; 4]);

    /// Interpret 32 bytes (little-endian) as a fingerprint — the inverse of
    /// [`to_le_bytes`](Fingerprint::to_le_bytes). A third party can build a `lift`-compatible
    /// fingerprint from raw bytes (e.g. a BLAKE3 digest computed with the re-exported
    /// [`blake3`]) without reimplementing this limb decode.
    ///
    /// ```
    /// use rsos::Fingerprint;
    ///
    /// let fp = Fingerprint([1, 2, 3, 4]);
    /// assert_eq!(Fingerprint::from_le_bytes(&fp.to_le_bytes()), fp);
    /// ```
    ///
    /// Unrolled rather than looped over the four limbs: a fixed count of four is simpler written
    /// out than indexed, and it keeps this `const fn` free of a manually incremented loop counter
    /// — the shape a single mutated `+=` could turn into an infinite loop, rather than a
    /// fast-failing wrong answer.
    #[must_use]
    pub const fn from_le_bytes(bytes: &[u8; 32]) -> Fingerprint {
        Fingerprint([
            u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            u64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]),
            u64::from_le_bytes([
                bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22],
                bytes[23],
            ]),
            u64::from_le_bytes([
                bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30],
                bytes[31],
            ]),
        ])
    }

    /// The 32-byte little-endian encoding of this fingerprint — the inverse of
    /// [`from_le_bytes`](Fingerprint::from_le_bytes). Unrolled for the same reason as
    /// `from_le_bytes` above.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 32] {
        let [a, b, c, d] = self.0;
        let a = a.to_le_bytes();
        let b = b.to_le_bytes();
        let c = c.to_le_bytes();
        let d = d.to_le_bytes();
        [
            a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], b[0], b[1], b[2], b[3], b[4], b[5],
            b[6], b[7], c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], d[0], d[1], d[2], d[3],
            d[4], d[5], d[6], d[7],
        ]
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
        Fingerprint::from_le_bytes(self.0.finalize().as_bytes())
    }
}

/// Def. 3.4's lifting function `lift: U → M`: BLAKE3 over the
/// [canonically encoded](crate::encoding) key followed by the canonically encoded value.
///
/// Part of the wire protocol — see this module's golden vectors. The [`Serialize`] bound admits
/// keys and values std implements no [`Hash`](std::hash::Hash) for
/// ([`HashMap`](std::collections::HashMap), [`HashSet`](std::collections::HashSet)).
///
/// ```
/// use rsos::lift;
///
/// // Injective within a type: changing either half of the pair moves the fingerprint.
/// assert_ne!(lift(&1, &"a"), lift(&2, &"a"));
/// assert_ne!(lift(&1, &"a"), lift(&1, &"b"));
///
/// // Deterministic: the same pair always lifts to the same fingerprint.
/// assert_eq!(lift(&1, &"a"), lift(&1, &"a"));
/// ```
pub fn lift<K: Serialize + ?Sized, V: Serialize + ?Sized>(key: &K, value: &V) -> Fingerprint {
    let mut hasher = Blake3Hasher::new();
    hasher.absorb(key);
    hasher.absorb(value);
    hasher.fingerprint()
}

/// The canonical 256-bit digest of a single value — [`lift`] with no key half, same encoding.
///
/// ```
/// use rsos::{digest, lift};
///
/// // No key half: digesting a value is lift with a unit key.
/// assert_eq!(digest(&"Hello"), lift(&(), &"Hello"));
///
/// // Distinct values digest to distinct fingerprints.
/// assert_ne!(digest(&"Hello"), digest(&"Hell"));
/// ```
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
