// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::*;

/// Terse constructors for the tests below.
fn hlc(physical: u64, logical: u32) -> Hlc {
    Hlc::new(
        PhysicalTime::from_millis(physical),
        LogicalCounter::new(logical),
    )
}

fn ts(physical: u64, logical: u32, node_id: u64) -> Timestamp {
    Timestamp::new(hlc(physical, logical), NodeId::new(node_id))
}

/// Physical time dominates the logical counter; no identity takes part.
#[test]
fn hlc_order_is_physical_then_logical() {
    assert!(hlc(101, 0) > hlc(100, u32::MAX));
    assert!(hlc(100, 1) > hlc(100, 0));
    assert_eq!(hlc(100, 0), hlc(100, 0));

    let mut sorted = vec![hlc(100, 1), hlc(99, 9), hlc(100, 0)];
    sorted.sort();
    assert_eq!(sorted, vec![hlc(99, 9), hlc(100, 0), hlc(100, 1)]);
}

/// The derived `Ord` *is* the conflict-resolution policy: pin it component by component.
#[test]
fn total_order_is_physical_then_logical_then_node_id() {
    assert!(ts(101, 0, 0) > ts(100, u32::MAX, u64::MAX));
    assert!(ts(100, 1, 0) > ts(100, 0, u64::MAX));
    assert!(ts(100, 0, 2) > ts(100, 0, 1));
    assert!(ts(100, 0, 1) < ts(100, 0, 2));
    assert_eq!(ts(100, 0, 1), ts(100, 0, 1));
    assert!(ts(100, 0, 1) <= ts(100, 0, 1));
    let mut sorted = vec![ts(100, 0, 2), ts(99, 9, 9), ts(100, 1, 0), ts(100, 0, 1)];
    sorted.sort();
    assert_eq!(
        sorted,
        vec![ts(99, 9, 9), ts(100, 0, 1), ts(100, 0, 2), ts(100, 1, 0)]
    );
}

/// Every plain-newtype accessor round-trips the value its constructor was given -- no hidden
/// default a caller could observe instead of what they actually stored. Multiple, non-adjacent
/// values (not just `0`/`1`) so a mutant that swaps the accessor for a fixed constant of either
/// kind still shows up as wrong for at least one of them.
#[test]
fn newtype_accessors_round_trip_their_constructor_argument() {
    for v in [0, 1, 7, u32::MAX] {
        assert_eq!(LogicalCounter::new(v).get(), v);
    }
    for v in [0, 1, 7, u64::MAX] {
        assert_eq!(NodeId::new(v).get(), v);
    }

    // `Timestamp::node_id()` specifically: `hlc()`/`physical()`/`logical()` are already exercised
    // via comparisons above, but nothing else calls `node_id()` and checks what it returns.
    let stamp = ts(50, 4, 9);
    assert_eq!(stamp.node_id(), NodeId::new(9));
    assert_ne!(stamp.node_id(), NodeId::new(0));
}

/// `next_tick()` never wraps: at `u32::MAX` it rolls physical forward.
#[test]
fn next_tick_never_wraps_counter() {
    assert_eq!(
        hlc(1000, u32::MAX).next_tick(),
        hlc(1001, 0),
        "physical should roll, counter reset"
    );

    assert_eq!(hlc(1000, 0).next_tick(), hlc(1000, 1));

    assert_eq!(
        hlc(u64::MAX, u32::MAX).next_tick(),
        hlc(u64::MAX, 0),
        "physical time saturates at u64::MAX"
    );
}

/// The ordering core advances strictly past both the previous state and the remote stamp.
#[test]
fn advance_past_remote_is_strictly_monotonic() {
    let mut last = hlc(10, 3);

    let remote = ts(50, 4, 9);
    last.advance_past_remote(
        PhysicalTime::EPOCH,
        AdmittedTime::trusted(remote.physical()),
        remote.logical(),
    );
    assert!(last > remote.hlc(), "{last:?} !> {:?}", remote.hlc());

    let before = last;
    last.advance_past_remote(
        PhysicalTime::from_millis(100),
        AdmittedTime::trusted(PhysicalTime::EPOCH),
        LogicalCounter::ZERO,
    );
    assert_eq!(last, hlc(100, 0));
    assert!(last > before);

    let before = last;
    last.advance_past_remote(
        PhysicalTime::EPOCH,
        AdmittedTime::trusted(PhysicalTime::EPOCH),
        LogicalCounter::ZERO,
    );
    assert_eq!(last, hlc(100, 1));
    assert!(last > before);
}

/// When `self` is strictly ahead of the remote in physical time, the remote's logical counter
/// belongs to an older instant and must not be mixed in — even if it happens to be numerically
/// larger than `self`'s own counter.
#[test]
fn advance_past_remote_ignores_a_trailing_remotes_logical_counter() {
    let mut last = hlc(100, 1);
    last.advance_past_remote(
        PhysicalTime::EPOCH,
        AdmittedTime::trusted(PhysicalTime::from_millis(50)),
        LogicalCounter::new(9_999),
    );
    assert_eq!(last, hlc(100, 2));
}

/// When `self` and the remote land on the exact same physical instant (a genuine tie), the
/// result must take the *larger* of the two logical counters, not just `self`'s own.
#[test]
fn advance_past_remote_mixes_logical_counters_on_a_genuine_physical_tie() {
    let mut last = hlc(50, 2);
    last.advance_past_remote(
        PhysicalTime::EPOCH,
        AdmittedTime::trusted(PhysicalTime::from_millis(50)),
        LogicalCounter::new(9),
    );
    assert_eq!(last, hlc(50, 10));
}

/// Out-of-budget readings come back capped and flagged, in-budget ones untouched.
#[test]
fn clamped_to_drift_caps_a_far_future_reading() {
    let now = PhysicalTime::from_millis(1_000);
    let budget = ClockDrift::from_millis(500);

    let admitted = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(u64::MAX), now, budget);
    assert_eq!(admitted.physical(), PhysicalTime::from_millis(1_500));
    assert!(admitted.was_clamped());

    let at_cap = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(1_500), now, budget);
    assert_eq!(at_cap.physical(), PhysicalTime::from_millis(1_500));
    assert!(!at_cap.was_clamped());

    let below = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(1_200), now, budget);
    assert_eq!(below.physical(), PhysicalTime::from_millis(1_200));
    assert!(!below.was_clamped());

    // The cap must saturate, not wrap below `now`.
    let huge = AdmittedTime::clamped_to_drift(
        PhysicalTime::from_millis(u64::MAX),
        now,
        ClockDrift::from_millis(u64::MAX),
    );
    assert_eq!(huge.physical(), PhysicalTime::from_millis(u64::MAX));
    assert!(!huge.was_clamped());

    assert_eq!(
        AdmittedTime::trusted(PhysicalTime::from_millis(u64::MAX)).physical(),
        PhysicalTime::from_millis(u64::MAX)
    );
}

/// A clamped reading must not drag the local clock past the cap; an unclamped one must.
#[test]
fn a_clamped_remote_cannot_poison_the_local_state() {
    let phys_now = PhysicalTime::from_millis(1_000);
    let budget = ClockDrift::from_millis(500);
    let hostile = ts(u64::MAX, 0, 99);

    let mut clamped = hlc(10, 0);
    clamped.advance_past_remote(
        phys_now,
        AdmittedTime::clamped_to_drift(hostile.physical(), phys_now, budget),
        hostile.logical(),
    );
    assert!(clamped.physical() <= phys_now.saturating_add(budget).next_ms());
    assert!(clamped < hostile.hlc());

    let mut trusted = hlc(10, 0);
    trusted.advance_past_remote(
        phys_now,
        AdmittedTime::trusted(hostile.physical()),
        hostile.logical(),
    );
    assert!(trusted > hostile.hlc());
}

/// A correct, minimal [`Clock`], used only to prove [`assert_conformance`] accepts a sound
/// implementation rather than rejecting everything indiscriminately.
struct ConformantClock {
    node_id: NodeId,
    last: std::sync::Mutex<Hlc>,
}

impl Clock for ConformantClock {
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let mut last = self.last.lock().unwrap();
        *last = last.next_tick();
        Timestamp::new(*last, self.node_id)
    }

    fn observe(&self, remote: Timestamp) {
        let mut last = self.last.lock().unwrap();
        last.advance_past_remote(
            PhysicalTime::EPOCH,
            AdmittedTime::trusted(remote.physical()),
            remote.logical(),
        );
    }

    fn observe_trusted(&self, remote: Timestamp) {
        self.observe(remote);
    }
}

#[test]
fn assert_conformance_accepts_a_correct_clock() {
    assert_conformance(&ConformantClock {
        node_id: NodeId::new(1),
        last: std::sync::Mutex::new(Hlc::START),
    });
}

/// The textbook footgun [`assert_conformance`]'s docs describe: read the wall clock, stamp
/// `logical = 0`. Compiles and type-checks; silently breaks `Entry::merge`'s strict `>` the
/// moment two calls land in the same physical millisecond.
struct NaiveNonMonotonicClock {
    node_id: NodeId,
}

impl Clock for NaiveNonMonotonicClock {
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        // A real wall clock can — and eventually will — return this same reading twice in a
        // row; a fixed value only makes the bug deterministic to demonstrate.
        Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(1_000), LogicalCounter::ZERO),
            self.node_id,
        )
    }

    fn observe(&self, _remote: Timestamp) {}
    fn observe_trusted(&self, _remote: Timestamp) {}
}

#[test]
#[should_panic(expected = "strictly monotonic")]
fn assert_conformance_rejects_a_non_monotonic_clock() {
    assert_conformance(&NaiveNonMonotonicClock {
        node_id: NodeId::new(1),
    });
}

/// The bug shape this test targets, independent of the open/close decision above: an
/// `observe_trusted` that clamps like `observe`. Reintroduces own-write shadowing after a
/// backward clock step, and used to type-check silently under the old default body.
struct ClampingTrustedClock {
    node_id: NodeId,
    last: std::sync::Mutex<Hlc>,
}

impl Clock for ClampingTrustedClock {
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let mut last = self.last.lock().unwrap();
        *last = last.next_tick();
        Timestamp::new(*last, self.node_id)
    }

    fn observe(&self, remote: Timestamp) {
        let mut last = self.last.lock().unwrap();
        let admitted =
            AdmittedTime::clamped_to_drift(remote.physical(), PhysicalTime::EPOCH, MAX_CLOCK_DRIFT);
        last.advance_past_remote(PhysicalTime::EPOCH, admitted, remote.logical());
    }

    fn observe_trusted(&self, remote: Timestamp) {
        self.observe(remote); // the bug: clamps a stamp this node itself authored
    }
}

#[test]
#[should_panic(expected = "observe_trusted")]
fn assert_conformance_rejects_observe_trusted_that_clamps() {
    assert_conformance(&ClampingTrustedClock {
        node_id: NodeId::new(1),
        last: std::sync::Mutex::new(Hlc::START),
    });
}
