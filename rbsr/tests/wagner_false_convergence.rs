// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Wagner's k-tree driven against the real [`protocol_round`], at reduced summary width.
//!
//! The claim under test: the additive combiner is a k-sum instance, and the k-tree applies to
//! `ℤ/2^w` *with no error term*, because reduction mod `2^j` is a group homomorphism and merging on
//! low-order bits is therefore exact. A planted solution then makes the driver SKIP a range on two
//! stores that genuinely differ — a **false convergence**, silent and permanent.
//!
//! This is the regression guard on the fingerprint's advertised strength: it fails the day the
//! combiner is claimed to resist a chosen-input adversary without being keyed. See
//! [`rsos::fingerprint`]'s module docs for what the shipped combiner does and does not promise.
//!
//! Only the **width** is scaled down. The lift is the shipped [`rsos::digest`] (BLAKE3 over the
//! canonical encoding) reduced mod `2^w`, the algebra is addition mod `2^w`, and the driver is
//! `rbsr`'s own, unmodified — [`NarrowStore`] enters through [`RsosView`], the third-party-backend
//! seam the trait documents. Extrapolation to `w = 256` is by the cost formula, not by assertion;
//! `wagner_cost_matches_the_k_tree_formula` pins the formula against measured work.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::ops::{Bound, RangeBounds};

use rbsr::{initial_ranges, protocol_round, RangeAggregate, RsosView};
use rsos::{digest, Aggregate, Fingerprint};

// ---------------------------------------------------------------------------------------------
// The reduced-width instance of `rsos`'s algebra
// ---------------------------------------------------------------------------------------------

/// Low `width` bits set. `width` is always in `1..=64` here.
fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// The shipped lift, reduced mod `2^width`.
///
/// Reduction is the homomorphism `ℤ/2^256 → ℤ/2^width`, so this is the *same* algebra at a width
/// where the attack is reproducible in a test — not a different construction.
fn lift(key: u64, width: u32) -> u64 {
    digest(&key).0[0] & mask(width)
}

/// A store summarizing with `Σ mod 2^width` instead of `Σ mod 2^256`.
///
/// Carried in limb 0 of a [`Fingerprint`] so the driver compares the real wire type. Keys are kept
/// sorted and unique, which is all `rank`/`select` need.
struct NarrowStore {
    width: u32,
    keys: Vec<u64>,
}

impl NarrowStore {
    fn new(width: u32, mut keys: Vec<u64>) -> NarrowStore {
        keys.sort_unstable();
        keys.dedup();
        NarrowStore { width, keys }
    }

    /// Half-open index span of `range`, empty when the bounds invert.
    fn span<R: RangeBounds<u64>>(&self, range: &R) -> (usize, usize) {
        let start = match range.start_bound() {
            Bound::Unbounded => 0,
            Bound::Included(k) => self.keys.partition_point(|x| x < k),
            Bound::Excluded(k) => self.keys.partition_point(|x| x <= k),
        };
        let end = match range.end_bound() {
            Bound::Unbounded => self.keys.len(),
            Bound::Included(k) => self.keys.partition_point(|x| x <= k),
            Bound::Excluded(k) => self.keys.partition_point(|x| x < k),
        };
        (start, end.max(start))
    }
}

impl RsosView<u64> for NarrowStore {
    fn size(&self) -> usize {
        self.keys.len()
    }

    fn aggregate<R: RangeBounds<u64>>(&self, range: R) -> Aggregate {
        let (start, end) = self.span(&range);
        let slice = &self.keys[start..end];
        let sum = slice.iter().fold(0u64, |acc, &key| {
            acc.wrapping_add(lift(key, self.width)) & mask(self.width)
        });
        Aggregate::new(slice.len(), Fingerprint([sum, 0, 0, 0]))
    }

    fn rank(&self, z: &u64) -> usize {
        self.keys.partition_point(|x| x < z)
    }

    fn select(&self, r: usize) -> &u64 {
        &self.keys[r]
    }
}

// ---------------------------------------------------------------------------------------------
// Wagner's k-tree over ℤ/2^width
// ---------------------------------------------------------------------------------------------

/// A partial sum and the signed keys that produced it.
///
/// `true` in the key list means the key is destined for peer A, `false` for peer B — the sign
/// folded into `value` at level 0.
#[derive(Clone)]
struct Partial {
    value: u64,
    keys: Vec<(u64, bool)>,
}

/// The parameters of one k-tree run, and the work it costs.
struct KTree {
    width: u32,
    /// `log2` of the list count, so `k = 2^t` lists and `t` merge levels.
    t: u32,
    /// Bits cancelled per merge level: `⌊width / (t + 1)⌋`.
    j: u32,
    /// Level-0 candidates per list. One bit of oversampling over the `2^j` the analysis asks for.
    list_size: usize,
    /// Level-0 lift evaluations — the attacker's offline cost.
    work: usize,
}

impl KTree {
    fn new(width: u32, t: u32) -> KTree {
        let j = width / (t + 1);
        let list_size = 1usize << (j + 1);
        KTree {
            width,
            t,
            j,
            list_size,
            work: (1usize << t) * list_size,
        }
    }

    /// Level-0 keys, namespaced so lists are disjoint from each other and from honest data.
    ///
    /// Bit 63 marks a planted key; `attempt` reseeds the whole search without touching the lift.
    fn key_of(&self, attempt: u64, list: usize, index: usize) -> u64 {
        (1u64 << 63) | (attempt << 52) | ((list as u64) << 40) | (index as u64)
    }

    /// Find `k` distinct keys whose signed lifts sum to zero mod `2^width`, split evenly between
    /// the two peers. `None` if every attempt came up empty.
    ///
    /// Positive lists feed peer A, negative lists peer B, so `|P_A| = |P_B| = k/2` by construction
    /// and the count component of the aggregate matches — Theorem 2 does not stand in the way.
    fn solve(&self, attempts: u64) -> Option<(Vec<u64>, Vec<u64>)> {
        (0..attempts).find_map(|attempt| self.solve_once(attempt))
    }

    fn solve_once(&self, attempt: u64) -> Option<(Vec<u64>, Vec<u64>)> {
        let k = 1usize << self.t;
        let modulus = mask(self.width);

        // Level 0: half the lists positive, half negated, sign folded into the value.
        let mut lists: Vec<Vec<Partial>> = (0..k)
            .map(|list| {
                let positive = list < k / 2;
                (0..self.list_size)
                    .map(|index| {
                        let key = self.key_of(attempt, list, index);
                        let raw = lift(key, self.width);
                        // Negation mod 2^width, exact for raw == 0 too.
                        let value = if positive {
                            raw
                        } else {
                            raw.wrapping_neg() & modulus
                        };
                        Partial {
                            value,
                            keys: vec![(key, positive)],
                        }
                    })
                    .collect()
            })
            .collect();

        // Merge levels: after level `l`, every surviving sum is zero on its low `l * j` bits.
        for level in 1..=self.t {
            let window = mask(level * self.j);
            lists = lists
                .chunks(2)
                .map(|pair| self.join(&pair[0], &pair[1], window))
                .collect();
        }

        // One list left, zero on `t * j` bits. A full solution is zero on all of them.
        let solution = lists[0].iter().find(|partial| partial.value == 0)?;
        let split = |wanted: bool| -> Vec<u64> {
            solution
                .keys
                .iter()
                .filter(|(_, positive)| *positive == wanted)
                .map(|(key, _)| *key)
                .collect()
        };
        Some((split(true), split(false)))
    }

    /// Keep the sums that cancel on `window`, capped so list sizes stay stable across levels.
    ///
    /// The lookup is exact: `(a + b) mod 2^w ≡ 0 (mod 2^j)` iff `a ≡ −b (mod 2^j)`, because
    /// reduction mod `2^j` is a homomorphism — carries leave the window upward and never re-enter
    /// it. This is the step §6.1 claims has no error term, and the assertion at the end of the
    /// test is what checks it end to end.
    fn join(&self, left: &[Partial], right: &[Partial], window: u64) -> Vec<Partial> {
        let mut index: HashMap<u64, Vec<&Partial>> = HashMap::new();
        for partial in left {
            index
                .entry(partial.value & window)
                .or_default()
                .push(partial);
        }
        let modulus = mask(self.width);
        let mut out = Vec::with_capacity(self.list_size);
        for b in right {
            let wanted = (b.value & window).wrapping_neg() & window;
            let Some(matches) = index.get(&wanted) else {
                continue;
            };
            for a in matches {
                if out.len() == self.list_size {
                    return out;
                }
                let mut keys = a.keys.clone();
                keys.extend_from_slice(&b.keys);
                out.push(Partial {
                    value: a.value.wrapping_add(b.value) & modulus,
                    keys,
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Driving the real protocol
// ---------------------------------------------------------------------------------------------

/// Honest data both peers hold, disjoint from the planted namespace (bit 63 clear).
fn honest() -> Vec<u64> {
    (0..500u64).collect()
}

/// One round of the real driver: peer A advertises its whole store, peer B answers.
///
/// Returns `true` when B SKIPped the outer range — the protocol declaring convergence.
fn declares_convergence(a: &NarrowStore, b: &NarrowStore) -> bool {
    let active: Vec<RangeAggregate<u64>> = initial_ranges(a);
    let mut children = Vec::new();
    let mut enumerations = Vec::new();
    let outcome = protocol_round(b, active, &mut children, &mut enumerations);
    children.is_empty() && enumerations.is_empty() && outcome.skipped() == 1
}

/// Two stores sharing `honest()`, differing only in what was planted on each side.
fn planted(width: u32, on_a: &[u64], on_b: &[u64]) -> (NarrowStore, NarrowStore) {
    let build = |extra: &[u64]| {
        let mut keys = honest();
        keys.extend_from_slice(extra);
        NarrowStore::new(width, keys)
    };
    (build(on_a), build(on_b))
}

/// The widths E3 runs at, with the k-tree shape used for each. `j = 8` throughout, so the cost per
/// list is constant and the list *count* carries the width — the slice of the trade-off curve that
/// keeps the test fast while still exercising every merge level.
const CONFIGURATIONS: [(u32, u32); 3] = [(32, 3), (48, 5), (64, 7)];

/// **The experiment.** A planted k-sum solution makes the driver SKIP the outer range while the
/// two stores genuinely differ.
#[test]
fn planted_k_sum_makes_the_protocol_declare_convergence_on_differing_stores() {
    for (width, t) in CONFIGURATIONS {
        let k_tree = KTree::new(width, t);
        let (on_a, on_b) = k_tree
            .solve(8)
            .unwrap_or_else(|| panic!("w={width}: k-tree found no solution in 8 attempts"));

        assert_eq!(
            on_a.len(),
            on_b.len(),
            "w={width}: the plant must be count-balanced, or Theorem 2 rejects it for free"
        );
        assert_eq!(on_a.len() + on_b.len(), 1usize << t);

        let (a, b) = planted(width, &on_a, &on_b);
        assert_ne!(
            a.keys, b.keys,
            "w={width}: the stores must genuinely differ"
        );
        assert_eq!(
            a.aggregate(..),
            b.aggregate(..),
            "w={width}: the k-tree solution must make the outer aggregates collide — \
             this is the step that fails if merging on low bits is not exact"
        );

        assert!(
            declares_convergence(&a, &b),
            "w={width}: the driver refused to SKIP a colliding outer range"
        );

        println!(
            "w={width}: k={} planted keys, j={}, {} lift evaluations offline",
            1usize << t,
            k_tree.j,
            k_tree.work
        );
    }
}

/// The control. Same shape, same sizes, keys that were never solved for — the driver must refine.
///
/// Without this the test above would pass against a driver that SKIPs everything.
#[test]
fn an_unsolved_plant_of_the_same_shape_is_refined_not_skipped() {
    for (width, t) in CONFIGURATIONS {
        let k_tree = KTree::new(width, t);
        let k = 1usize << t;
        let keys: Vec<u64> = (0..k)
            .map(|list| k_tree.key_of(u64::from(u32::MAX), list, 0))
            .collect();
        let (on_a, on_b) = keys.split_at(k / 2);

        let (a, b) = planted(width, on_a, on_b);
        assert!(
            !declares_convergence(&a, &b),
            "w={width}: an unsolved plant must not collide"
        );
    }
}

/// Theorem 2, mechanically: an unbalanced difference is never SKIPped, whatever the summary does.
///
/// The plant here is a *solved* one with a single key removed from peer B, so the fingerprints no
/// longer agree and neither do the counts. Pinned because §5 claims this holds with probability 1
/// and with no hypothesis on the lift — a claim that outlives any width.
#[test]
fn an_unbalanced_difference_is_never_skipped() {
    for (width, t) in CONFIGURATIONS {
        let k_tree = KTree::new(width, t);
        let (on_a, mut on_b) = k_tree.solve(8).expect("k-tree found no solution");
        on_b.pop();

        let (a, b) = planted(width, &on_a, &on_b);
        assert_ne!(a.size(), b.size());
        assert!(
            !declares_convergence(&a, &b),
            "w={width}: a count mismatch must be detected with certainty"
        );
    }
}

/// The cost formula §6.1 extrapolates to `w = 256` with, checked against what the runs above spend.
///
/// Work is `2^(t + width/(t+1))`; minimizing over `t` gives `t + 1 = √width` and `2^(2√width − 1)`.
/// At `width = 256` that is `t = 15`, `k = 32 768` and `2³¹` — the number §6.1 quotes. This test
/// pins the arithmetic, not the attack: the attack itself is pinned at the widths above.
#[test]
fn wagner_cost_matches_the_k_tree_formula() {
    for (width, t) in CONFIGURATIONS {
        let k_tree = KTree::new(width, t);
        // The one bit of oversampling is deliberate; the analysis asks for 2^j per list.
        assert_eq!(k_tree.work, (1usize << t) * (1usize << (k_tree.j + 1)));
        assert_eq!(k_tree.j, width / (t + 1));
    }

    // The optimum is at t + 1 = √width, where the exponent is 2√width − 1. Integer division makes
    // the neighbouring `t` tie with it, so what is pinned is the achieved minimum, not the argmin.
    let exponent = |width: u32, t: u32| t + width / (t + 1);
    for width in [64u32, 144, 256] {
        let root = (f64::from(width)).sqrt() as u32;
        assert_eq!(
            exponent(width, root - 1),
            2 * root - 1,
            "w={width}: t + 1 = √w does not cost 2^(2√w − 1)"
        );
        assert_eq!(
            (1..width).map(|t| exponent(width, t)).min().unwrap(),
            2 * root - 1,
            "w={width}: some other list count beats 2^(2√w − 1)"
        );
    }
}
