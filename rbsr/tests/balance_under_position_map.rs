// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Which divergences the **count** half of the aggregate can see, and what actually decides it.
//!
//! The count is exact under `f_p = id`, so a range whose two peers hold *different* cardinalities
//! is never SKIPped — no assumption on the lift, no probability. A silent loss therefore requires a
//! **balanced** range, and whether a divergence is *ever* unbalanced is not a property of RBSR: it
//! is a property of the **position map** `π`, the projection that gives the store its order.
//!
//! The obvious reading is that `π`-injectivity decides it — a key-ordered map ties the two versions
//! of a conflicting record to one position, a versioned order separates them. **That reading is
//! wrong, and arm 2 below is the counter-example.** Separating two records needs a cut point
//! strictly between them, and cut points come from `Select`, i.e. from positions some peer actually
//! holds. Two *distinct but adjacent* positions are exactly as unseparable as one shared position.
//!
//! What decides it is whether the update **relocates** the record — whether the *leading* component
//! of the order is the one that changed:
//!
//! | arm | `π` | conflicting records | separable? | count sees it |
//! |---|---|---|---|---|
//! | 1 | `(key)` — this workspace | one shared position | no, ties | **never** |
//! | 2 | `(key, version)` | adjacent, nothing sorts between | **no**, adjacency | **never** |
//! | 3 | `(timestamp, key)` — Negentropy's order | relocated across the order | yes | yes |
//!
//! Arms 1 and 2 reach the same observable outcome for different structural reasons, which is why
//! injectivity is the wrong invariant to state. Arm 3 is the falsifiable half.
//!
//! Both stores are driven by `rbsr`'s own unmodified driver, at the shipped [`rsos::lift`]'s full
//! width — this is a statement about counts, not about collisions. Ranges are captured where the
//! driver asks for them, inside [`RsosView::aggregate`], because [`rbsr::RangeAggregate`] exposes
//! neither its bounds nor its aggregate to an out-of-crate test
//! (<https://github.com/Akvize/reconcile-rs/issues/289>).

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::ops::{Bound, RangeBounds};

use rbsr::{initial_ranges, protocol_round, RsosView};
use rsos::{lift, Aggregate, Fingerprint};

/// Past this many rounds the drive is not converging; the cap turns a hang into a failure.
const MAX_ROUNDS: usize = 64;

/// Elements each peer holds.
const RECORDS: u64 = 500;

/// The one record the two peers disagree about.
const CONFLICT: u64 = 250;

/// Arm 3's updated timestamp: past every honest one, so the record moves to the far end of the
/// order rather than staying beside its old self.
const LATE: u64 = 700;

// ---------------------------------------------------------------------------------------------
// A sorted store that records what the driver asked it
// ---------------------------------------------------------------------------------------------

/// A set of positions with a per-position summary, plus a log of every range the driver aggregated.
///
/// `K` is the **position** type — what the store is ordered by, which is the axis under test. The
/// summary is the shipped lift over `(key, value)`, so two peers holding one record with different
/// values summarize it differently whatever position it occupies.
struct Store<K> {
    positions: Vec<K>,
    summaries: Vec<Fingerprint>,
    asked: RefCell<Vec<(Bound<K>, Bound<K>)>>,
}

impl<K: Clone + Ord> Store<K> {
    /// Build from `(position, summary)` pairs. Positions must be unique — a duplicate would make
    /// the store a multiset, which the count argument does not cover.
    fn new(mut entries: Vec<(K, Fingerprint)>) -> Store<K> {
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let before = entries.len();
        entries.dedup_by(|left, right| left.0 == right.0);
        assert_eq!(before, entries.len(), "positions must be unique");

        let (positions, summaries) = entries.into_iter().unzip();
        Store {
            positions,
            summaries,
            asked: RefCell::new(Vec::new()),
        }
    }

    /// Half-open index span of `range`, empty when the bounds invert.
    fn span<R: RangeBounds<K>>(&self, range: &R) -> (usize, usize) {
        let start = match range.start_bound() {
            Bound::Unbounded => 0,
            Bound::Included(key) => self.positions.partition_point(|x| x < key),
            Bound::Excluded(key) => self.positions.partition_point(|x| x <= key),
        };
        let end = match range.end_bound() {
            Bound::Unbounded => self.positions.len(),
            Bound::Included(key) => self.positions.partition_point(|x| x <= key),
            Bound::Excluded(key) => self.positions.partition_point(|x| x < key),
        };
        (start, end.max(start))
    }

    /// `|X ∩ range|`, **without** recording — the harness reading the store, not the driver asking.
    fn count<R: RangeBounds<K>>(&self, range: &R) -> usize {
        let (start, end) = self.span(range);
        end - start
    }

    /// Every range this peer was asked to aggregate, in the order asked.
    fn asked(&self) -> Vec<(Bound<K>, Bound<K>)> {
        self.asked.borrow().clone()
    }

    /// [`RsosView::aggregate`]'s body, plus the recording. Inherent so the trait impls below are
    /// one line each.
    fn record_and_aggregate<R: RangeBounds<K>>(&self, range: R) -> Aggregate {
        self.asked
            .borrow_mut()
            .push((range.start_bound().cloned(), range.end_bound().cloned()));

        let (start, end) = self.span(&range);
        let summary = self.summaries[start..end]
            .iter()
            .fold(Fingerprint::ZERO, |acc, &one| acc + one);
        Aggregate::new(end - start, summary)
    }
}

/// The four Def. 3.9 queries [`RsosView`] carries, once per concrete position type.
///
/// Written out rather than `impl<K> RsosView<K> for Store<K>`, which is E0119 against the blanket
/// impl covering every [`rsos::Rsos`] — the trait's own docs call this out. Each body forwards to
/// the generic inherent method above, so only the header is duplicated.
macro_rules! rsos_view_for {
    ($position:ty) => {
        impl RsosView<$position> for Store<$position> {
            fn size(&self) -> usize {
                self.positions.len()
            }

            fn aggregate<R: RangeBounds<$position>>(&self, range: R) -> Aggregate {
                self.record_and_aggregate(range)
            }

            fn rank(&self, z: &$position) -> usize {
                self.positions.partition_point(|x| x < z)
            }

            fn select(&self, r: usize) -> &$position {
                &self.positions[r]
            }
        }
    };
}

rsos_view_for!(u64);
rsos_view_for!((u64, u64));

// ---------------------------------------------------------------------------------------------
// Driving the real protocol
// ---------------------------------------------------------------------------------------------

/// What one full drive observed.
struct Drive {
    rounds: usize,
    enumerated: usize,
    /// Ranges either peer was asked about on which the two hold **different** cardinalities — the
    /// comparisons an exact count resolves for free.
    unbalanced: usize,
    /// Ranges either peer was asked about, in total.
    asked: usize,
}

/// Reconcile `a` against `b` to a fixed point, alternating which peer answers, then classify every
/// range either was asked about.
fn drive<K: Clone + Ord>(a: &Store<K>, b: &Store<K>) -> Drive
where
    Store<K>: RsosView<K>,
{
    let mut active = initial_ranges(a);
    let mut responder: &Store<K> = b;
    let mut advertiser: &Store<K> = a;
    let mut rounds = 0;
    let mut enumerated = 0;

    while !active.is_empty() && rounds < MAX_ROUNDS {
        let mut children = Vec::new();
        let mut enumerations = Vec::new();
        let outcome = protocol_round(responder, active, &mut children, &mut enumerations);

        enumerated += outcome.enumerated();
        active = children;
        rounds += 1;
        std::mem::swap(&mut responder, &mut advertiser);
    }
    assert!(rounds < MAX_ROUNDS, "the drive did not reach a fixed point");

    let asked: Vec<(Bound<K>, Bound<K>)> = a.asked().into_iter().chain(b.asked()).collect();
    let unbalanced = asked
        .iter()
        .filter(|range| a.count(*range) != b.count(*range))
        .count();

    Drive {
        rounds,
        enumerated,
        unbalanced,
        asked: asked.len(),
    }
}

/// Every arm plants the same divergence — one record, two values — so the only variable is `π`.
fn report(arm: &str, drive: &Drive) {
    println!(
        "{arm:<18} {} rounds, {} ranges asked, {} unbalanced, {} enumerated",
        drive.rounds, drive.asked, drive.unbalanced, drive.enumerated
    );
}

// ---------------------------------------------------------------------------------------------
// Three position maps over one divergence
// ---------------------------------------------------------------------------------------------

/// **Arm 1** — `π(k, v) = k`. Both peers hold every key; they disagree on the value at
/// [`CONFLICT`].
fn key_ordered(value_at_conflict: &str) -> Store<u64> {
    let entries = (0..RECORDS)
        .map(|key| (key, summary(key, value_at_conflict)))
        .collect();
    Store::new(entries)
}

/// **Arm 2** — `π(k, v, t) = (k, t)`. The version rides *behind* the key, so the two conflicting
/// records sort next to each other with nothing in between.
fn key_then_version(version_at_conflict: u64, value_at_conflict: &str) -> Store<(u64, u64)> {
    let entries = (0..RECORDS)
        .map(|key| {
            let version = if key == CONFLICT {
                version_at_conflict
            } else {
                0
            };
            ((key, version), summary(key, value_at_conflict))
        })
        .collect();
    Store::new(entries)
}

/// **Arm 3** — `π(k, v, t) = (t, k)`, Negentropy's `(timestamp, id)`. The timestamp *leads*, so
/// rewriting a record moves it across the order instead of leaving it beside its old self.
fn time_ordered(time_at_conflict: u64, value_at_conflict: &str) -> Store<(u64, u64)> {
    let entries = (0..RECORDS)
        .map(|key| {
            let time = if key == CONFLICT {
                time_at_conflict
            } else {
                key
            };
            ((time, key), summary(key, value_at_conflict))
        })
        .collect();
    Store::new(entries)
}

/// The shipped lift over `(key, value)`: shared everywhere but the conflicting record.
fn summary(key: u64, value_at_conflict: &str) -> Fingerprint {
    let value = if key == CONFLICT {
        value_at_conflict
    } else {
        "shared"
    };
    lift(&key, value)
}

// ---------------------------------------------------------------------------------------------
// The experiment
// ---------------------------------------------------------------------------------------------

/// **Arm 1 — ties.** Identical position sets make the two peers agree on the cardinality of *every*
/// range, so the exact count contributes nothing at any depth. This is the divergence an LWW
/// register produces continuously, and it is the case the count cannot cover.
#[test]
fn a_key_ordered_conflict_is_balanced_because_the_positions_tie() {
    let a = key_ordered("written-by-a");
    let b = key_ordered("written-by-b");

    assert_ne!(
        a.aggregate(..),
        b.aggregate(..),
        "the two peers must genuinely diverge, or the drive is vacuous"
    );
    assert_eq!(
        a.positions, b.positions,
        "arm 1's mechanism is the tie: one position carries both values"
    );

    let drive = drive(&a, &b);
    report("key-ordered", &drive);

    assert_eq!(
        drive.unbalanced, 0,
        "tied positions agree on the count of every range whatsoever"
    );
    assert!(
        drive.enumerated > 0,
        "the drive must still localize the difference — balance costs the count, not correctness"
    );
}

/// **Arm 2 — adjacency, and the refutation.** Distinct positions are not enough. `(key, version)`
/// sorts the two conflicting records next to each other, no peer holds anything between them, so no
/// `Select` can ever produce a cut point that separates them and every range holding one holds
/// both.
///
/// Same observable outcome as arm 1, different mechanism — which is why "make `π` injective" is not
/// the rule.
#[test]
fn a_key_then_version_conflict_is_balanced_because_the_positions_are_adjacent() {
    let a = key_then_version(1, "written-by-a");
    let b = key_then_version(2, "written-by-b");

    assert_ne!(a.aggregate(..), b.aggregate(..));
    assert_ne!(
        a.positions, b.positions,
        "arm 2's positions must be distinct — that is the whole point of the arm"
    );
    assert!(
        !a.positions.contains(&(CONFLICT, 2)) && !b.positions.contains(&(CONFLICT, 1)),
        "neither peer may hold a position between the two, or they become separable"
    );

    let drive = drive(&a, &b);
    report("key-then-version", &drive);

    assert_eq!(
        drive.unbalanced, 0,
        "adjacent positions are as unseparable as tied ones: injectivity is not the invariant"
    );
    assert!(drive.enumerated > 0);
}

/// **Arm 3 — relocation.** With the timestamp leading, rewriting a record moves it past every
/// record whose timestamp lies between the two, and those records are the cut points that separate
/// them. Ranges on either side then hold different cardinalities and the exact count resolves them
/// outright.
#[test]
fn a_time_ordered_conflict_becomes_unbalanced_once_the_record_relocates() {
    let a = time_ordered(CONFLICT, "written-by-a");
    let b = time_ordered(LATE, "written-by-b");

    assert_ne!(a.aggregate(..), b.aggregate(..));
    assert_eq!(
        a.size(),
        b.size(),
        "store totals stay equal, so the outer range is still balanced — the count only starts \
         helping below the depth that separates the two"
    );

    let drive = drive(&a, &b);
    report("time-ordered", &drive);

    assert!(
        drive.unbalanced > 0,
        "a relocated record must reach a range on which the cardinalities differ"
    );
    assert!(drive.enumerated > 0);
}

/// The control: with no divergence at all every arm must resolve in one round, nothing enumerated
/// and nothing unbalanced. A failure here means the harness is wrong, not the claim.
#[test]
fn identical_peers_converge_immediately_under_every_position_map() {
    let keyed = drive(&key_ordered("shared"), &key_ordered("shared"));
    assert_eq!(
        (keyed.rounds, keyed.enumerated, keyed.unbalanced),
        (1, 0, 0)
    );

    let versioned = drive(
        &key_then_version(1, "shared"),
        &key_then_version(1, "shared"),
    );
    let stats = (versioned.rounds, versioned.enumerated, versioned.unbalanced);
    assert_eq!(stats, (1, 0, 0));

    let timed = drive(
        &time_ordered(CONFLICT, "shared"),
        &time_ordered(CONFLICT, "shared"),
    );
    assert_eq!(
        (timed.rounds, timed.enumerated, timed.unbalanced),
        (1, 0, 0)
    );
}
