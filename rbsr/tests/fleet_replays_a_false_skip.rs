// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A fleet does not *resample* a summary collision. It replays it.
//!
//! The claim under test is an **identity, not a rate**, and it does not rest on this crate's choice
//! of fingerprint. Every range-fingerprint scheme in the RBSR family guarantees that equal contents
//! over a range yield equal fingerprints — that is what lets a matched range be recognised, so a
//! scheme without it could not terminate the recursion. Here it is immediate ([`rsos::lift`] is
//! BLAKE3 over the canonical encoding of `(key, value)`, folded over the range and consulting
//! nothing else); Meyer & Scherer's non-homomorphic construction states it as the design
//! requirement instead ("if they do store the same set in a range, their clamped subtrees will be
//! equal, and hence have equal root hashes"). With [`rbsr::Comparison::agrees`] comparing the whole
//! bundled aggregate, a verdict is therefore a pure function of `(local content over the range,
//! advertised remote aggregate)`, and two peers holding identical content are, to the driver,
//! *the same peer*:
//!
//! ```text
//! A vs B₁ … B_k, every B_i holding identical content over r
//!   ⇒ identical aggregate advertised, against identical local content
//!   ⇒ identical verdict, for every i — all skip, or none does
//! ```
//!
//! What anti-entropy varies is only **which peer is contacted** — every known local peer every
//! round, a shuffled `remote_fanout` subset across networks (`reconcile`'s
//! `src/replica/reconciliation.rs`; `RandomProbe` discovers *addresses*, it never picks a partner)
//! — and never what a contacted pair computes. The intuition this refutes is not folklore: it is
//! Meyer, arXiv:2212.13567 §5.1, offering as a mitigation that "in systems where nodes repeatedly
//! synchronize with different other nodes, a single fingerprint collision in a single
//! synchronization session would merely delay propagation of information rather than stop it
//! completely". The load-bearing words are *different other nodes* — an assumption about contents,
//! not about peers. It imports the independence of a *partner draw* into the *comparison*; on the
//! local network there is not even a draw to import, since `A` meets all `N - 1` peers every round
//! and computes the same verdict `N - 1` times. So the count of distinct
//! verdicts a fleet can reach is governed by its number of distinct **content classes**, not by
//! `N` — which `fleet_size_does_not_change_the_verdict_count` below asserts directly: hold the
//! class count fixed, grow `N`, require the distinct-session count not to move.
//!
//! The consequence is why this is a regression guard and not a benchmark: over a range where a
//! converged fleet holds one divergence there are **2** classes, so redundancy buys **zero**
//! retries precisely when the fleet is healthy — and the range stays wrong for every peer that
//! later syncs from either side.
//!
//! Only the summary **width** is scaled down, as in `wagner_false_convergence.rs`: the lift is the
//! shipped [`rsos::digest`] reduced mod `2^w` — reduction is a group homomorphism, so this is the
//! same algebra, not a different construction — and the driver is `rbsr`'s own, unmodified,
//! entered through [`RsosView`]. The plant is a single-element swap found by birthday search
//! rather than by a k-tree: *how* a collision arises is `wagner_false_convergence.rs`'s subject,
//! and nothing here depends on it.
//!
//! **Scope.** The replay property assumes range boundaries are chosen deterministically from the
//! peers' states, which is what [`RsosView::select`] does here. Meyer §5.1 also proposes randomly
//! shifting them per session, which *would* decorrelate — below the outer range, where a collision
//! is decided before any boundary is drawn. Meyer's §6 pseudorandom treaps do **not** decorrelate:
//! `priority(u)` is a fixed hash of the item, and the construction relies on the resulting treaps
//! being unique.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::ops::{Bound, RangeBounds};

use rbsr::{
    initial_ranges, protocol_round, EnumerationRange, RangeAggregate, RoundOutcome, RsosView,
};
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
fn lift(key: u64, width: u32) -> u64 {
    digest(&key).0[0] & mask(width)
}

/// A store summarizing with `Σ mod 2^width` instead of `Σ mod 2^256`.
///
/// Carried in limb 0 of a [`Fingerprint`] so the driver compares the real wire type. Keys are kept
/// sorted and unique, which is all `rank`/`select` need.
#[derive(Clone)]
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
// The plant: one content class per peer, exactly one of them colliding
// ---------------------------------------------------------------------------------------------

/// Summary width. Wide enough that the birthday search is a real search, narrow enough that it
/// finishes in milliseconds.
const WIDTH: u32 = 32;

/// The planted namespace: bit 63 set, so every plant sorts above all of [`honest`].
const PLANT_BASE: u64 = 1 << 63;

/// Honest data every peer holds, disjoint from the planted namespace.
fn honest() -> Vec<u64> {
    (0..500u64).collect()
}

/// One content class: the honest data plus a single planted key, and optionally a batch of keys
/// written to *both* peers.
fn store_with(width: u32, plant: u64, shared: &[u64]) -> NarrowStore {
    let mut keys = honest();
    keys.push(plant);
    keys.extend_from_slice(shared);
    NarrowStore::new(width, keys)
}

/// One content class, with no shared writes on top.
fn store(width: u32, plant: u64) -> NarrowStore {
    store_with(width, plant, &[])
}

/// Two distinct planted keys whose lifts agree mod `2^width`, by birthday search.
///
/// Swapping one for the other leaves both halves of the bundled aggregate untouched — same element
/// count, same sum — which is the whole of what [`rbsr::Comparison::agrees`] reads.
fn colliding_pair(width: u32) -> (u64, u64) {
    let mut seen: HashMap<u64, u64> = HashMap::new();
    for i in 0..(1u64 << 20) {
        let key = PLANT_BASE | i;
        if let Some(&first) = seen.get(&lift(key, width)) {
            return (first, key);
        }
        seen.insert(lift(key, width), key);
    }
    panic!("w={width}: no colliding pair within the searched namespace");
}

/// One planted key per content class. Class 0 is `A`'s own; class 1 collides with it; classes ≥ 2
/// are ordinary divergences the driver must refine.
///
/// The non-colliding classes are *asserted* distinct from class 0's lift rather than assumed: a
/// second, accidental collision would silently turn a control peer into another false SKIP.
fn class_plants(width: u32) -> Vec<u64> {
    let (own, collider) = colliding_pair(width);
    let mut plants = vec![own, collider];
    for i in 0..8u64 {
        let plant = PLANT_BASE | (1 << 40) | i;
        assert_ne!(
            lift(plant, width),
            lift(own, width),
            "control plant {plant:#x} accidentally collides with class 0 — pick another"
        );
        plants.push(plant);
    }
    plants
}

// ---------------------------------------------------------------------------------------------
// One anti-entropy session, driven to termination
// ---------------------------------------------------------------------------------------------

/// A drive that has not terminated by here is a bug, not a slow case: refinement is `O(log n)`
/// rounds over 501 elements.
const MAX_ROUNDS: usize = 64;

/// Everything one session between two peers produced — the unit two peers of the same content
/// class must agree on, down to the emitted ranges.
#[derive(Debug, PartialEq, Eq)]
struct Session {
    rounds: usize,
    outcome: RoundOutcome,
    enumerations: Vec<EnumerationRange<u64>>,
}

impl Session {
    /// The driver resolved the outer range outright, offering nothing: the SKIP it declares when
    /// the two aggregates agree.
    fn declared_convergence(&self) -> bool {
        self.rounds == 1
            && self.outcome.skipped() == 1
            && self.outcome.children() == 0
            && self.enumerations.is_empty()
    }

    /// Some range was offered for enumeration, so the difference is on its way to being repaired.
    fn repaired(&self) -> bool {
        !self.enumerations.is_empty()
    }
}

/// `a` advertises its whole store; the two peers alternate rounds until nothing is left active.
///
/// Neither store is mutated — this measures what the protocol *decides*, which is exactly what the
/// conjecture is about.
fn session(a: &NarrowStore, b: &NarrowStore) -> Session {
    let mut active: Vec<RangeAggregate<u64>> = initial_ranges(a);
    let (mut responder, mut advertiser) = (b, a);
    let mut outcome = RoundOutcome::default();
    let mut enumerations = Vec::new();
    let mut rounds = 0;

    while !active.is_empty() {
        rounds += 1;
        assert!(
            rounds <= MAX_ROUNDS,
            "the drive did not terminate in {MAX_ROUNDS} rounds"
        );
        let mut children = Vec::new();
        outcome += protocol_round(responder, active, &mut children, &mut enumerations);
        active = children;
        std::mem::swap(&mut responder, &mut advertiser);
    }

    Session {
        rounds,
        outcome,
        enumerations,
    }
}

/// `n` counterparts for `A`, drawn round-robin from content classes `1..classes`.
fn fleet(width: u32, plants: &[u64], classes: usize, n: usize) -> Vec<NarrowStore> {
    (0..n)
        .map(|i| store(width, plants[1 + i % (classes - 1)]))
        .collect()
}

/// How many *distinct* sessions a fleet produced.
fn distinct(sessions: &[Session]) -> usize {
    let mut seen: Vec<&Session> = Vec::new();
    for s in sessions {
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    seen.len()
}

/// Fleet sizes swept wherever the claim is "independent of `N`".
const FLEET_SIZES: [usize; 5] = [1, 2, 8, 16, 32];

// ---------------------------------------------------------------------------------------------
// The experiments
// ---------------------------------------------------------------------------------------------

/// **The result.** In a converged fleet — 2 content classes over the divergent range — every one
/// of the `N` counterparts replays the *same* false SKIP. Redundancy buys zero retries.
#[test]
fn a_converged_fleet_replays_one_false_skip_however_large_it_is() {
    let plants = class_plants(WIDTH);
    let a = store(WIDTH, plants[0]);

    for n in FLEET_SIZES {
        let peers = fleet(WIDTH, &plants, 2, n);
        let sessions: Vec<Session> = peers.iter().map(|b| session(&a, b)).collect();

        for (i, (peer, s)) in peers.iter().zip(&sessions).enumerate() {
            assert_ne!(
                a.keys, peer.keys,
                "N={n}, peer {i}: the stores must genuinely differ, or the SKIP is not false"
            );
            assert_eq!(
                a.aggregate(..),
                peer.aggregate(..),
                "N={n}, peer {i}: the plant must make the outer aggregates collide"
            );
            assert!(
                s.declared_convergence(),
                "N={n}, peer {i}: the driver refused to SKIP a colliding outer range — {s:?}"
            );
            assert!(
                !s.repaired(),
                "N={n}, peer {i}: nothing may be offered — {s:?}"
            );
        }

        assert_eq!(
            distinct(&sessions),
            1,
            "N={n}: identical content must produce one verdict, not {n} draws"
        );
    }
}

/// The control, and the other row of the conjecture's table. A peer of a *different* content class
/// does repair — so the suite above is not passing against a driver that SKIPs everything, and
/// what buys a retry is a new content class, never another copy of one already present.
#[test]
fn only_a_new_content_class_buys_a_retry() {
    let plants = class_plants(WIDTH);
    let a = store(WIDTH, plants[0]);
    let classes = plants.len();

    let peers = fleet(WIDTH, &plants, classes, 4 * classes);
    for (i, peer) in peers.iter().enumerate() {
        let s = session(&a, peer);
        let class = 1 + i % (classes - 1);
        if class == 1 {
            assert!(
                !s.repaired(),
                "peer {i} (class 1) collides with A, so it cannot repair — {s:?}"
            );
        } else {
            assert!(
                s.repaired(),
                "peer {i} (class {class}) genuinely differs and must refine to enumeration — {s:?}"
            );
        }
    }

    assert!(
        peers.iter().any(|peer| session(&a, peer).repaired()),
        "the control is vacuous unless some class actually repairs"
    );
}

/// **The conjecture, stated as the invariance it is.** Hold the content-class count fixed, grow
/// the fleet: the number of distinct verdicts does not move. `N` is not the independent variable.
#[test]
fn fleet_size_does_not_change_the_verdict_count() {
    let plants = class_plants(WIDTH);
    let a = store(WIDTH, plants[0]);

    for classes in [2, 3, 5, plants.len()] {
        let counterpart_classes = classes - 1;
        let mut baseline = None;

        for n in FLEET_SIZES.iter().map(|n| n * counterpart_classes) {
            let peers = fleet(WIDTH, &plants, classes, n);
            let sessions: Vec<Session> = peers.iter().map(|b| session(&a, b)).collect();
            let seen = distinct(&sessions);

            assert!(
                seen <= counterpart_classes,
                "c={classes}, N={n}: {seen} distinct verdicts exceeds the {counterpart_classes} \
                 counterpart classes that can produce one"
            );
            match baseline {
                None => baseline = Some(seen),
                Some(first) => assert_eq!(
                    seen, first,
                    "c={classes}, N={n}: the verdict count moved with N, from {first} to {seen}"
                ),
            }
        }
    }
}

/// **Redundancy is not the only thing that fails to help: churn does not either.** `⊗` is a group
/// operation, so an element written to *both* peers moves both sums by the same delta and the
/// aggregates still agree. The outer range keeps SKIPping, refinement never starts, and the cut
/// point that would separate the two planted keys is never computed — whatever else the fleet
/// writes, and wherever it lands relative to the plant.
#[test]
fn a_collision_is_absorbing_under_shared_writes() {
    let plants = class_plants(WIDTH);
    let (low, high) = (plants[0].min(plants[1]), plants[0].max(plants[1]));

    let batches: [(&str, Vec<u64>); 3] = [
        ("below both plants", (600..700).collect()),
        ("between the plants", (low + 1..high).take(50).collect()),
        ("above both plants", (high + 1..=high + 50).collect()),
    ];

    for (label, shared) in batches {
        assert!(!shared.is_empty(), "{label}: the batch must not be vacuous");
        let a = store_with(WIDTH, plants[0], &shared);
        let b = store_with(WIDTH, plants[1], &shared);

        assert_ne!(a.keys, b.keys, "{label}: the stores must still differ");
        assert!(
            session(&a, &b).declared_convergence(),
            "{label}: a write applied to both peers cancels in the aggregate and must leave the \
             collision intact"
        );
    }
}

/// The control. A write landing on **one** side does break it — so the absorbing property above is
/// the shared delta cancelling, not the driver ignoring writes altogether.
#[test]
fn a_one_sided_write_does_break_the_collision() {
    let plants = class_plants(WIDTH);
    let a = store(WIDTH, plants[0]);
    let b = store_with(WIDTH, plants[1], &[601]);

    assert!(
        session(&a, &b).repaired(),
        "a write on one side alone must refine to enumeration"
    );
}
