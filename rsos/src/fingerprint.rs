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
//! 2. **Stable, versioned hash function *and* stable input encoding.** The
//!    fingerprint is the **wire reconciliation token**: two nodes must compute
//!    the *same* fingerprint for the same data, forever, across Rust versions,
//!    platforms (32- vs 64-bit), and endianness. That takes two halves, and
//!    pinning only one of them is not enough.
//!
//!    The *hash function* is the first half: `std`'s
//!    [`DefaultHasher`](std::collections::hash_map::DefaultHasher) is explicitly
//!    documented as unspecified and unstable across releases, so the element
//!    hash is pinned to **BLAKE3**.
//!
//!    The *input encoding* is the other half, and it is the one this crate used
//!    to get wrong. Feeding BLAKE3 through a [`std::hash::Hasher`] adapter pins
//!    how bytes are written but not which bytes std's `Hash` impls choose to
//!    write — and Rust makes no stability promise about that. `Hash for str`,
//!    `Hash for Option<T>` or any other impl may change its byte sequence in a
//!    future release, which would change every fingerprint in a cluster and
//!    leave a mixed-version cluster re-exchanging ranges forever: exactly the
//!    failure this property is meant to prevent. `Hash` is also not implemented
//!    for [`HashMap`](std::collections::HashMap)/[`HashSet`](std::collections::HashSet),
//!    so such values were unusable.
//!
//!    So `rsos` owns the encoding end to end: [`lift`] serializes key and value
//!    through the crate's own injective, length-prefixed
//!    [canonical encoding](crate::canonical) (a `serde::Serializer` writing
//!    straight into BLAKE3 — no codec crate involved) rather than through
//!    `Hash`. Only with both halves owned here is "stable across Rust versions"
//!    a claim this crate can actually make. The golden-vector tests at the
//!    bottom of this module freeze the wire format so any change that would
//!    break interoperability fails CI.
//!
//! See: A. Meyer, *Range-Based Set Reconciliation*
//! (arXiv:2212.13567); Clarke et al., *Incremental Multiset Hash Functions*
//! (ASIACRYPT 2003).

use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

use serde::{Deserialize, Serialize};

use crate::canonical;

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

/// A BLAKE3 accumulator that yields a 256-bit [`Fingerprint`].
///
/// Bytes reach it exclusively through the crate's [canonical encoding](crate::canonical), which is
/// what makes the digest a stable wire token: the encoding fixes integer widths and endianness,
/// length-prefixes everything variable-length, and orders map entries — none of which the hash
/// function itself can guarantee.
struct Blake3Hasher(blake3::Hasher);

impl Blake3Hasher {
    fn new() -> Blake3Hasher {
        Blake3Hasher(blake3::Hasher::new())
    }

    /// Absorb `value`'s canonical encoding.
    ///
    /// Infallible in practice: writing into a BLAKE3 accumulator has nothing that can fail, so the
    /// only way this panics is a hand-written [`Serialize`] impl that refuses to serialize itself
    /// (`serde::ser::Error::custom`) — a bug in that type, surfaced loudly rather than folded into
    /// a wrong fingerprint. See [`canonical::Error`](crate::canonical::Error).
    fn absorb<T: Serialize + ?Sized>(&mut self, value: &T) {
        canonical::encode_into(&mut self.0, value).expect("canonical encoding cannot fail");
    }

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint::from_bytes(self.0.finalize().as_bytes())
    }
}

/// The **lifting function**: map a single key-value element into the summary monoid
/// [`Fingerprint`], i.e. Def. 3.4's `lift: U → M`.
///
/// Named `lift`, not `hash`, for two reasons. It is the paper's own term — Def. 3.4 of
/// arXiv:2603.19820 defines a *lifting function* from an element to the monoid `M`, and the
/// Meyer/Willow-ecosystem reference implementation (`earthstar-project/range-reconcile`) names the
/// same operation `lift` in its `BYOLiftingMonoid` (`lift`/`combine`/`neutral`), the vocabulary
/// this crate's root docs already cite for the future generic-summary work. And it never collided
/// with [`std::hash::Hash::hash`] — which, since the move to the canonical encoding, it no longer
/// calls at all.
///
/// This is the per-element summary that the
/// [`FingerprintTreeMap`](crate::fingerprint_tree_map::FingerprintTreeMap) combines into range
/// fingerprints. It is BLAKE3 over the [canonically encoded](crate::canonical) key followed by the
/// canonically encoded value, and is part of the wire protocol — see the golden-vector tests.
///
/// The bound is [`Serialize`], not [`Hash`](std::hash::Hash): the encoding is this crate's own, so
/// types std does not implement `Hash` for (notably [`HashMap`](std::collections::HashMap) and
/// [`HashSet`](std::collections::HashSet)) are usable as keys and values, and no future change to
/// a std `Hash` impl can move a fingerprint.
pub fn lift<K: Serialize + ?Sized, V: Serialize + ?Sized>(key: &K, value: &V) -> Fingerprint {
    let mut hasher = Blake3Hasher::new();
    hasher.absorb(key);
    hasher.absorb(value);
    hasher.fingerprint()
}

/// The canonical 256-bit digest of a *single* value — [`lift`] with no key half.
///
/// Same encoding, same stability guarantee; used where a deterministic, cross-node content token
/// for one value is needed rather than a per-element range summary.
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

    // --- Golden vectors: freeze the wire format. ---
    //
    // These pin the exact bytes that go on the wire. If a change to the element
    // hash (BLAKE3, the feeding order/encoding) or the combiner ever alters
    // them, this test fails — that change would silently break interoperability
    // between nodes and must be a deliberate, versioned wire-format bump.
    //
    // The values below are NOT the ones this file carried before. The previous vectors were
    // computed from `std::hash::Hash`'s byte stream; moving the input encoding to the crate's own
    // canonical serde encoding changes every element fingerprint, so the old constants are gone on
    // purpose. That is a deliberate, documented wire break — see the module docs and PROGRESS.md:
    // a node on the new code and one on the old code never agree on a range fingerprint and would
    // re-exchange indefinitely, so this is not a rolling upgrade.

    #[test]
    fn golden_element_hash() {
        // BLAKE3 over the canonical encoding of the key (50u64, 8 bytes little-endian) followed by
        // the canonical encoding of the value ("Hello": u64 length 5, then the 5 bytes).
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
        // Order-independent combination of three elements (the building block of
        // a range fingerprint).
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

    // --- Properties of the canonical encoding, seen through `lift`. ---
    //
    // `canonical`'s own unit tests check the byte stream directly; these check that the properties
    // survive the trip through BLAKE3, which is the form the protocol actually relies on.

    #[test]
    fn framing_is_unambiguous() {
        // The classic ambiguity: unprefixed concatenation would make both of these "abc".
        assert_ne!(lift(&"ab", &"c"), lift(&"a", &"bc"));
        // Same shape one level down, in a sequence.
        assert_ne!(
            lift(&0u8, &vec![vec![1u8, 2]]),
            lift(&0u8, &vec![vec![1u8], vec![2u8]])
        );
        // And between a two-element sequence and a pair of one-element ones.
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

        // ...and agrees with the ordered map holding the same entries. `HashMap` has no `Hash`
        // impl at all, so neither of these was even expressible before the canonical encoding.
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
        // `None` encodes as the single byte 0, `Some(0u8)` as 1 then 0: the tag is what keeps the
        // "absent" case from being read as the payload of a present one.
        assert_ne!(lift(&0u8, &None::<u8>), lift(&0u8, &Some(0u8)));
        // The same, where the payload is itself empty: `Some(vec![])` is the tag plus a zero-length
        // prefix, never bare nothing.
        assert_ne!(
            lift(&0u8, &None::<Vec<u8>>),
            lift(&0u8, &Some(Vec::<u8>::new()))
        );
        // Nested options stay distinct all the way down.
        assert_ne!(
            lift(&0u8, &Some(None::<u8>)),
            lift(&0u8, &None::<Option<u8>>)
        );

        // Injectivity is a property *within* a type, which is all the protocol needs: a given tree
        // has one `K` and one `V`. Across types it cannot hold and is not claimed — `None::<u8>`
        // and `0u8` both encode to a single zero byte.
        assert_eq!(lift(&0u8, &None::<u8>), lift(&0u8, &0u8));
    }

    #[test]
    fn integers_of_different_widths_differ() {
        // Fixed-width, not varint: the same numeric value at two widths is two different elements.
        assert_ne!(lift(&0u8, &1u32), lift(&0u8, &1u64));
        assert_ne!(lift(&0u8, &1u8), lift(&0u8, &1u16));
        // Signed and unsigned of the same width coincide on non-negative values, by construction.
        assert_eq!(lift(&0u8, &1u32), lift(&0u8, &1i32));
    }

    #[test]
    fn floats_follow_bit_patterns_not_partial_eq() {
        // `+0.0 == -0.0`, but their bit patterns — and so their fingerprints — differ.
        assert_ne!(lift(&0u8, &0.0f64), lift(&0u8, &-0.0f64));
        // `NaN != NaN`, but the same bit pattern fingerprints identically.
        assert_eq!(lift(&0u8, &f64::NAN), lift(&0u8, &f64::NAN));
    }

    #[test]
    fn digest_is_lift_without_a_key_half() {
        // `digest` exists for single-value content tokens; it must be the same encoding.
        assert_eq!(digest(&"Hello"), lift(&(), &"Hello"));
        assert_ne!(digest(&"Hello"), digest(&"Hell"));
    }
}
