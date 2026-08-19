// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::*;

/// Terse constructor for the tests below.
fn ts(physical: u64, logical: u32, node_id: u64) -> Timestamp {
    Timestamp::new(
        Hlc::new(
            PhysicalTime::from_millis(physical),
            LogicalCounter::new(logical),
        ),
        NodeId::new(node_id),
    )
}

#[test]
fn now_is_strictly_monotonic() {
    let clock = HlcClock::new(NodeId::new(1));
    let mut prev = clock.now();
    for _ in 0..10_000 {
        let next = clock.now();
        assert!(next > prev, "{next:?} !> {prev:?}");
        prev = next;
    }
}

#[test]
fn logical_increments_when_physical_does_not_advance() {
    let clock = HlcClock::new(NodeId::new(1));
    // Just into the future, within the clamp, so every `now()` bumps the counter.
    let near_future = phys_now().saturating_add(ClockDrift::from_millis(60_000)); // 60 s ahead
    clock.observe(Timestamp::new(
        Hlc::new(near_future, LogicalCounter::ZERO),
        NodeId::new(9),
    ));
    let a = clock.now();
    let b = clock.now();
    assert_eq!(a.physical(), b.physical());
    assert_eq!(b, Timestamp::new(a.hlc().next_tick(), a.node_id()));
}

#[test]
fn observe_advances_past_a_future_timestamp() {
    // Legitimate skew: our next local write must be ordered after the peer's stamp.
    let clock = HlcClock::new(NodeId::new(1));
    let future = Timestamp::new(
        Hlc::new(
            phys_now().saturating_add(ClockDrift::from_millis(5_000)),
            LogicalCounter::new(5),
        ),
        NodeId::new(2),
    );
    clock.observe(future);
    let local = clock.now();
    assert!(
        local > future,
        "local write {local:?} was not ordered after observed future timestamp {future:?}"
    );
}

#[test]
fn manual_clock_is_deterministic() {
    let clock = ManualClock::new(NodeId::new(7));
    assert_eq!(clock.now(), ts(0, 1, 7));
    assert_eq!(clock.now(), ts(0, 2, 7));
    let remote = ts(50, 4, 9);
    clock.observe(remote);
    let local = clock.now();
    assert_eq!(local, ts(50, 5, 7));
    assert!(local > remote);
}

/// A stamp near `u64::MAX` must neither pin the clock near it nor break monotonicity.
#[test]
fn observe_far_future_is_clamped() {
    let clock = HlcClock::new(NodeId::new(1));

    let before_clamp = clock.now();

    let adversarial = ts(u64::MAX - 1, 0, 99);
    clock.observe(adversarial);

    let after_clamp = clock.now();
    assert!(
        after_clamp > before_clamp,
        "monotonicity violated: {after_clamp:?} !> {before_clamp:?}"
    );

    // Margin for the logical tick, but nowhere near the adversarial physical time.
    let upper_bound = phys_now()
        .saturating_add(MAX_CLOCK_DRIFT)
        .saturating_add(ClockDrift::from_millis(10));
    assert!(
        after_clamp.physical() <= upper_bound,
        "clock was not clamped: physical {:?} >> cap {:?}",
        after_clamp.physical(),
        upper_bound
    );
}

/// Repeated observes of increasing far-future stamps must not ratchet past the cap.
#[test]
fn repeated_far_future_observes_do_not_escape_cap() {
    let clock = HlcClock::new(NodeId::new(2));
    for delta in [u64::MAX / 2, u64::MAX - 500, u64::MAX - 1] {
        clock.observe(ts(delta, 0, 99));
    }
    let minted = clock.now();
    let upper_bound = phys_now()
        .saturating_add(MAX_CLOCK_DRIFT)
        .saturating_add(ClockDrift::from_millis(10));
    assert!(
        minted.physical() <= upper_bound,
        "physical {:?} escaped the cap {:?}",
        minted.physical(),
        upper_bound
    );
}

/// A saturated counter under pinned physical time rolls `physical` forward without wrapping.
#[test]
fn counter_overflow_rolls_physical_forward() {
    let clock = HlcClock::new(NodeId::new(3));

    // Pin physical time just ahead of real time, inside the drift cap, with a max counter.
    let pinned_physical = phys_now().next_ms();
    let max_counter_stamp = Timestamp::new(
        Hlc::new(pinned_physical, LogicalCounter::new(u32::MAX)),
        NodeId::new(99),
    );
    clock.observe(max_counter_stamp);

    let rolled = clock.now();

    assert!(
        rolled > max_counter_stamp,
        "timestamp not strictly greater after counter roll: {rolled:?} vs {max_counter_stamp:?}"
    );

    assert!(
        rolled.physical() > pinned_physical,
        "physical time did not roll forward: {rolled:?}"
    );
}

/// The trusted path must chase a far-future stamp, the untrusted one must stay clamped —
/// the distinction persisted stamps depend on after a backward clock step.
#[test]
fn observe_trusted_bypasses_far_future_clamp() {
    let far_future = Timestamp::new(
        Hlc::new(
            phys_now()
                .saturating_add(MAX_CLOCK_DRIFT)
                .saturating_add(ClockDrift::from_millis(5_000_000)),
            LogicalCounter::new(3),
        ),
        NodeId::new(7),
    );

    // Trusted path: the clock chases the stamp.
    let trusted_clock = HlcClock::new(NodeId::new(1));
    trusted_clock.observe_trusted(far_future);
    let after_trusted = trusted_clock.now();
    assert!(
        after_trusted > far_future,
        "observe_trusted did not advance the clock past the far-future stamp: \
         next now() {after_trusted:?} is not > {far_future:?}"
    );

    // Clamped path: it must not.
    let clamped_clock = HlcClock::new(NodeId::new(2));
    clamped_clock.observe(far_future);
    let after_clamped = clamped_clock.now();
    // Re-read physical time: a stale reading would flake as the wall clock advances.
    let cap_upper = phys_now()
        .saturating_add(MAX_CLOCK_DRIFT)
        .saturating_add(ClockDrift::from_millis(10));
    assert!(
        after_clamped.physical() <= cap_upper,
        "observe (clamped) let physical time escape the cap: {:?} > {:?}",
        after_clamped.physical(),
        cap_upper
    );
    assert!(
        after_clamped < far_future,
        "clamped observe produced a stamp >= the far-future value: \
         {after_clamped:?} should be < {far_future:?}"
    );
}

/// The four regimes of `BoundedInstant::from_stored_stamp`.
#[test]
fn bounded_instant_covers_every_stamp_regime() {
    let budget = MAX_CLOCK_DRIFT;

    // (1) Normal: used verbatim.
    let normal = phys_now().millis() - 1_000;
    let b = BoundedInstant::from_stored_stamp(PhysicalTime::from_millis(normal), budget);
    assert_eq!(b.bound(), StampBound::Verbatim);
    assert_eq!(b.instant().timestamp_millis(), normal as i64);

    // (2) Far future, inside chrono's range: must be capped, not converted exactly.
    let far = phys_now().millis() + 10_000 * 365 * 24 * 3_600_000; // ~10 000 years ahead
    let b = BoundedInstant::from_stored_stamp(PhysicalTime::from_millis(far), budget);
    assert_eq!(b.bound(), StampBound::Capped);
    // Sampled *after* the construction it bounds, and never reused across constructions: the
    // cap is `now + budget` as of the call, so a sample taken before a later call races the
    // wall clock against it. Reusing one sample for regime (3) below is what made this test
    // flake on a loaded runner.
    let cap_upper = phys_now().saturating_add(budget).millis() as i64;
    assert!(
        b.instant().timestamp_millis() <= cap_upper,
        "instant {} escaped the cap {cap_upper}",
        b.instant()
    );
    assert!(
        b.instant().timestamp_millis() >= phys_now().millis() as i64,
        "a capped instant must still be in the future, not in the past"
    );

    // (3) Above `i64::MAX`: the same cap as (2), never a pre-epoch date.
    let b = BoundedInstant::from_stored_stamp(PhysicalTime::from_millis(u64::MAX), budget);
    assert_eq!(b.bound(), StampBound::Capped);
    let cap_upper = phys_now().saturating_add(budget).millis() as i64;
    assert!(
        b.instant().timestamp_millis() > 0,
        "wrapped to a pre-epoch instant: {}",
        b.instant()
    );
    assert!(
        b.instant().timestamp_millis() <= cap_upper,
        "instant {} escaped the cap {cap_upper}",
        b.instant()
    );

    // (4) Residual: `now + budget` past chrono's ceiling falls back to local now.
    let b = BoundedInstant::from_stored_stamp(
        PhysicalTime::from_millis(u64::MAX),
        ClockDrift::from_millis(u64::MAX),
    );
    assert_eq!(b.bound(), StampBound::Unrepresentable);
    let skew = (b.instant().timestamp_millis() - phys_now().millis() as i64).abs();
    assert!(
        skew < 60_000,
        "fallback instant is not ~now: {}",
        b.instant()
    );
}

/// A tighter `max_clock_drift` clamps a stamp the default bound would accept.
#[test]
fn custom_max_clock_drift_is_respected() {
    let drift = ClockDrift::from_millis(1_000); // 1 s cap, far tighter than the 1-hour default
    let clock = HlcClock::new(NodeId::new(1)).with_max_clock_drift(drift);

    let remote_physical = phys_now().saturating_add(ClockDrift::from_millis(60_000));
    clock.observe(Timestamp::new(
        Hlc::new(remote_physical, LogicalCounter::ZERO),
        NodeId::new(99),
    ));

    let minted = clock.now();
    let upper_bound = phys_now()
        .saturating_add(drift)
        .saturating_add(ClockDrift::from_millis(10));
    assert!(
        minted.physical() <= upper_bound,
        "custom drift cap not enforced: physical {:?} > cap {:?}",
        minted.physical(),
        upper_bound
    );
}

/// The default adapter must itself pass the gate any substitute [`Clock`] has to.
#[test]
fn hlc_clock_is_conformant() {
    assert_conformance(&HlcClock::new(NodeId::new(1)));
}

/// `ManualClock` skips the wall clock entirely, but the contract it must uphold for tests to
/// mean anything is the same one production adapters uphold.
#[test]
fn manual_clock_is_conformant() {
    assert_conformance(&ManualClock::new(NodeId::new(1)));
}
