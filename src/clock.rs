// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The wall-clock **adapter** behind the [`Clock`] port, whose types and arithmetic live in
//! [`lww_register::clock`] and are re-exported here.
//!
//! `HlcClock` owns the only wall-clock read in the crate and the node's [`NodeId`], which it
//! attaches when minting a reading into a [`Timestamp`]. `BoundedInstant` is the other thing that
//! needs both a physical-time read and `chrono`: the tombstone-expiry instant derived from a
//! *stored* stamp, bounded by the same [`MAX_CLOCK_DRIFT`] budget.
//!
//! `HlcClock` is the default [`Clock`] adapter, not the only one that can be plugged in:
//! [`ReplicatedMap::new_with_clock`](crate::ReplicatedMap::new_with_clock) accepts any `Arc<dyn
//! Clock>` (`#288`). [`assert_conformance`] is what an implementor runs before trusting a
//! substitute clock — see its docs, linked from
//! [`ReplicatedMap::new_with_clock`](crate::ReplicatedMap::new_with_clock), for exactly what a
//! non-conformant one silently breaks.

use parking_lot::Mutex;
use tracing::warn;

pub use lww_register::clock::{
    assert_conformance, AdmittedTime, Clock, ClockDrift, Hlc, LogicalCounter, NodeId, PhysicalTime,
    Timestamp, MAX_CLOCK_DRIFT,
};

use chrono::{DateTime, Utc};

/// Read physical time as an instant on the Unix-epoch millisecond scale.
pub(crate) fn phys_now() -> PhysicalTime {
    PhysicalTime::from_millis(Utc::now().timestamp_millis().max(0) as u64)
}

/// How a [`BoundedInstant`] was arrived at. Anything but
/// [`Verbatim`](StampBound::Verbatim) means a stored stamp leads local time by more than the
/// budget — worth surfacing on the default unauthenticated socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StampBound {
    /// Within budget: the stamp's own physical component is the instant.
    Verbatim,
    /// The stamp led local physical time by more than the budget; the cap (`local now + budget`)
    /// is used instead. The stored stamp itself is untouched.
    Capped,
    /// Not even the cap is a representable `DateTime<Utc>`, so local now is used. Needs a
    /// [`ClockDrift`] pushing `now + budget` past year 262143.
    Unrepresentable,
}

/// A wall-clock instant derived from a **stored** HLC stamp, bounded so a peer-controlled stamp
/// cannot drive it arbitrarily far into the future.
///
/// [`Clock::observe`]'s clamp guards the local clock state only — a stored stamp keeps its
/// received value as LWW data — so anything deriving an instant from one must bound it itself
/// (`ARCHITECTURE.md` §5 invariant 6). Constructing this is the only way to get the instant, so
/// neither an exact far-future conversion (unbounded tombstone retention) nor a wrap-negative
/// cast is reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundedInstant {
    instant: DateTime<Utc>,
    bound: StampBound,
}

impl BoundedInstant {
    /// Bound `stamp_physical` to `local now + budget` and convert it to a wall-clock instant.
    ///
    /// Within budget the stamp is used unchanged, so honest replicas agree on when a tombstone
    /// ages out; beyond it, the cap. Outcomes: [`StampBound`].
    pub(crate) fn from_stored_stamp(
        stamp_physical: PhysicalTime,
        budget: ClockDrift,
    ) -> BoundedInstant {
        let admitted = AdmittedTime::clamped_to_drift(stamp_physical, phys_now(), budget);
        // `i64::try_from`, not `as`: the wrap-to-negative regime must be unrepresentable.
        match i64::try_from(admitted.physical().millis())
            .ok()
            .and_then(DateTime::from_timestamp_millis)
        {
            Some(instant) => BoundedInstant {
                instant,
                bound: if admitted.was_clamped() {
                    StampBound::Capped
                } else {
                    StampBound::Verbatim
                },
            },
            // Residual: `chrono` tops out near year 262143. Age from now, the safe direction.
            None => BoundedInstant {
                instant: Utc::now(),
                bound: StampBound::Unrepresentable,
            },
        }
    }

    /// The bounded wall-clock instant.
    pub(crate) fn instant(self) -> DateTime<Utc> {
        self.instant
    }

    /// How that instant was arrived at.
    pub(crate) fn bound(self) -> StampBound {
        self.bound
    }
}

/// A per-node Hybrid Logical Clock: the default [`Clock`] adapter, internally synchronized and
/// cloned across a node's tasks.
///
/// Owns the crate's only physical-time read. Its state is a bare [`Hlc`]; the [`NodeId`] is
/// attached only at mint time, so it is held in one place.
#[derive(Debug)]
pub(crate) struct HlcClock {
    node_id: NodeId,
    /// How far a remote stamp may lead physical time before [`observe`](Clock::observe) clamps
    /// it. A clock concern, not a store one.
    max_clock_drift: ClockDrift,
    /// Last reading produced or observed, updated atomically so [`now`](HlcClock::now) stays
    /// strictly monotonic. An [`Hlc`], not a [`Timestamp`]: the identity lives in `node_id`.
    last: Mutex<Hlc>,
}

impl HlcClock {
    /// A clock for `node_id`, clamping at [`MAX_CLOCK_DRIFT`].
    pub fn new(node_id: NodeId) -> HlcClock {
        HlcClock {
            node_id,
            max_clock_drift: MAX_CLOCK_DRIFT,
            last: Mutex::new(Hlc::START),
        }
    }

    /// Override the clamp threshold (default [`MAX_CLOCK_DRIFT`]).
    #[allow(dead_code)]
    pub fn with_max_clock_drift(mut self, max_clock_drift: ClockDrift) -> HlcClock {
        self.max_clock_drift = max_clock_drift;
        self
    }
}

impl Clock for HlcClock {
    /// Mint a timestamp strictly greater than every timestamp this clock has produced or
    /// observed.
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let pt = phys_now();
        let mut last = self.last.lock();
        let next = if pt > last.physical() {
            Hlc::new(pt, LogicalCounter::ZERO)
        } else {
            last.next_tick()
        };
        *last = next;
        Timestamp::new(next, self.node_id)
    }

    /// Advance past a peer's timestamp, so a subsequent [`now`](Clock::now) outranks it.
    ///
    /// The untrusted path: the remote reading goes through [`AdmittedTime::clamped_to_drift`] and
    /// a clamp is `warn!`ed. The remote's own `Timestamp` is untouched.
    fn observe(&self, remote: Timestamp) {
        let pt = phys_now();
        let mut last = self.last.lock();

        let admitted = AdmittedTime::clamped_to_drift(remote.physical(), pt, self.max_clock_drift);
        if admitted.was_clamped() {
            warn!(
                remote_physical_ms = remote.physical().millis(),
                remote_node_id = remote.node_id().get(),
                phys_now_ms = pt.millis(),
                cap_ms = admitted.physical().millis(),
                max_clock_drift_ms = self.max_clock_drift.millis(),
                "remote timestamp leads local clock by more than the configured max drift; \
                 clamping to cap to protect local clock state"
            );
        }

        last.advance_past_remote(pt, admitted, remote.logical());
    }

    /// Advance past a stamp this node authored, without the clamp.
    ///
    /// A backward wall-clock step across a restart would otherwise leave the clock below an
    /// honest persisted stamp, shadowing our own writes. The one place entitled to
    /// [`AdmittedTime::trusted`].
    fn observe_trusted(&self, remote: Timestamp) {
        let pt = phys_now();
        let mut last = self.last.lock();
        last.advance_past_remote(
            pt,
            AdmittedTime::trusted(remote.physical()),
            remote.logical(),
        );
    }
}

/// Deterministic [`Clock`] adapter for tests: no physical-time read, so stamps are reproducible.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock {
    node_id: NodeId,
    last: Mutex<Hlc>,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new(node_id: NodeId) -> ManualClock {
        ManualClock {
            node_id,
            last: Mutex::new(Hlc::START),
        }
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let mut last = self.last.lock();
        // Go through the domain's own tick rather than rebuilding it, roll included.
        let next = last.next_tick();
        *last = next;
        Timestamp::new(next, self.node_id)
    }

    fn observe(&self, remote: Timestamp) {
        let mut last = self.last.lock();
        if remote.hlc() > *last {
            *last = remote.hlc();
        }
    }

    /// No physical-time read to clamp against, so this is just [`observe`](Clock::observe) —
    /// sound here only because `ManualClock` never clamps in the first place.
    fn observe_trusted(&self, remote: Timestamp) {
        self.observe(remote);
    }
}

#[cfg(test)]
mod tests {
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
}
