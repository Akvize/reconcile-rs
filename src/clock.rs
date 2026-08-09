// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The wall-clock **adapter** behind the [`Clock`] port.
//!
//! The Hybrid Logical Clock's value types ([`Hlc`], [`Timestamp`] and their
//! [`PhysicalTime`]/[`LogicalCounter`]/[`NodeId`] components), its port ([`Clock`]) and its
//! ordering arithmetic live in the infrastructure-free [`lww_register::clock`] module. What is
//! left here is the one thing that cannot: `HlcClock`, the adapter that reads physical time
//! through `chrono` and feeds it into that arithmetic. It owns the only wall-clock read in the
//! crate.
//!
//! Those types, the [`AdmittedTime`] seam and [`MAX_CLOCK_DRIFT`] are re-exported here so that
//! `reconcile::clock::*` keeps resolving in one place. Note where the two halves sit: the
//! arithmetic is [`Hlc::next_tick`] and [`Hlc::advance_past_remote`], methods on the *reading*,
//! which knows nothing of node identity — this adapter is what holds a [`NodeId`] and pairs it
//! with a reading to mint a [`Timestamp`].
//!
//! The second thing that cannot live in the domain is `BoundedInstant` (crate-private): turning a
//! *stored* HLC stamp into the `chrono` instant the tombstone-expiry wheel ages by. It sits here
//! because it is the only other place in the crate that needs both a physical-time read and
//! `chrono`, and because the bound it applies is the same [`MAX_CLOCK_DRIFT`] budget
//! `HlcClock::observe` uses.

use parking_lot::Mutex;
use tracing::warn;

pub use lww_register::clock::{
    AdmittedTime, Clock, ClockDrift, Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp,
    MAX_CLOCK_DRIFT,
};

use chrono::{DateTime, Utc};

/// Read physical time as an instant on the Unix-epoch millisecond scale.
pub(crate) fn phys_now() -> PhysicalTime {
    PhysicalTime::from_millis(Utc::now().timestamp_millis().max(0) as u64)
}

/// How a [`BoundedInstant`] was arrived at — the observable outcome of bounding a stored stamp.
///
/// Only [`Verbatim`](StampBound::Verbatim) is the ordinary case; the other two mean a stamp in the
/// map leads this node's physical time by more than the budget, which on the default
/// unauthenticated socket is a signal worth surfacing (see `README.md`, "Security model").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StampBound {
    /// The stamp led local physical time by no more than the budget, so its own physical
    /// component *is* the instant — byte-for-byte the behaviour that predates this bound.
    Verbatim,
    /// The stamp led local physical time by more than the budget; the cap (`local now + budget`)
    /// is used instead. The stored stamp itself is untouched.
    Capped,
    /// Not even the cap is a representable `DateTime<Utc>`, so local now is used. Unreachable with
    /// the default budget — it needs a [`ClockDrift`] large enough to push `now + budget` past
    /// year 262143 — but it is the residual path, and it is reported rather than swallowed.
    Unrepresentable,
}

/// A wall-clock instant derived from a **stored** HLC stamp, bounded so that a peer-controlled
/// stamp cannot drive it arbitrarily far into the future.
///
/// The far-future clamp in [`Clock::observe`] protects the *local clock state* only: a remote
/// stamp is kept exactly as received in the map, because it is LWW data and rewriting it would
/// change conflict resolution and break the wire contract. Anything that then does *arithmetic* on
/// a stored stamp therefore has to bound the value it derives — this type is that bound, and the
/// tombstone-expiry hook in [`replicated_map`](crate::replicated_map) is its consumer.
///
/// Constructing one is the only way to get the instant, so the two hazards of the old
/// `physical().millis() as i64` are both closed:
///
/// * a stamp in the far future but inside `chrono`'s range converted *exactly*, dating the
///   tombstone past any plausible expiry, so `TimeoutWheel::expired()` never yielded it — an
///   unbounded-retention hole reachable by any host that can send a datagram;
/// * a stamp above `i64::MAX` wrapped negative into a pre-1970 date. After the clamp that regime
///   is unrepresentable rather than handled: the admitted value is at most `now + budget`, and the
///   conversion is a total [`i64::try_from`], not a lossy cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundedInstant {
    instant: DateTime<Utc>,
    bound: StampBound,
}

impl BoundedInstant {
    /// Bound `stamp_physical` to `local now + budget` and convert it to a wall-clock instant.
    ///
    /// `budget` is the same kind of plausibility allowance [`Clock::observe`] applies (default
    /// [`MAX_CLOCK_DRIFT`]): a stamp may legitimately lead this node's clock by a bounded skew, and
    /// within that skew the stamp's own physical component is used unchanged, so honest replicas
    /// keep agreeing on when a tombstone ages out. Beyond it, the cap is used — see
    /// [`StampBound`] for what each outcome means.
    pub(crate) fn from_stored_stamp(
        stamp_physical: PhysicalTime,
        budget: ClockDrift,
    ) -> BoundedInstant {
        let admitted = AdmittedTime::clamped_to_drift(stamp_physical, phys_now(), budget);
        // `i64::try_from` rather than `as`: after the clamp the value is provably at most
        // `now + budget`, so the wrap-to-negative regime cannot arise — but a *total* conversion
        // is what makes that a fact of the code rather than of the reasoning around it.
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
            // Residual path: the cap saturates at `u64::MAX` for an absurd budget, and `chrono`
            // tops out around year 262143. Fall back to local now — the tombstone then ages from
            // this moment, which is the conservative direction — and say so.
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

/// A per-node Hybrid Logical Clock — the default [`Clock`] adapter.
///
/// Generates locally-monotonic [`Timestamp`]s with [`now`](Clock::now) and advances
/// past timestamps received from peers with [`observe`](Clock::observe). The clock is
/// internally synchronized, so a single instance is shared (cloned) across all tasks of a
/// node. It owns the only physical-time read in the crate (`phys_now`); the ordering rule it
/// applies to that reading is [`Hlc::advance_past_remote`]. Its [`NodeId`] is held in exactly one
/// place — the clock state is a bare [`Hlc`], and the identity is attached only when a reading is
/// minted into a [`Timestamp`].
#[derive(Debug)]
pub(crate) struct HlcClock {
    node_id: NodeId,
    /// How far a remote stamp may lead physical time before [`observe`](Clock::observe)
    /// clamps it when advancing the local clock state. Owned by the clock (not the store), defaulting
    /// to [`MAX_CLOCK_DRIFT`] and overridable via
    /// [`with_max_clock_drift`](HlcClock::with_max_clock_drift).
    max_clock_drift: ClockDrift,
    /// Last clock reading produced or observed; the physical/logical pair is updated atomically
    /// under the mutex so that [`now`](HlcClock::now) stays strictly monotonic. It is an [`Hlc`],
    /// not a [`Timestamp`]: the node identity is `self.node_id`, and duplicating it inside the
    /// state would be a second copy that could disagree with the first.
    last: Mutex<Hlc>,
}

impl HlcClock {
    /// Create a clock for the node identified by `node_id`, using the default far-future clamp
    /// threshold ([`MAX_CLOCK_DRIFT`]). Override it with
    /// [`with_max_clock_drift`](HlcClock::with_max_clock_drift).
    pub fn new(node_id: NodeId) -> HlcClock {
        HlcClock {
            node_id,
            max_clock_drift: MAX_CLOCK_DRIFT,
            last: Mutex::new(Hlc::START),
        }
    }

    /// Override how far a remote stamp may lead physical time before
    /// [`observe`](Clock::observe) clamps it (default [`MAX_CLOCK_DRIFT`]). The clamp
    /// threshold is a clock concern, configured here rather than through the store's
    /// [`Config`](crate::replicated_map::Config).
    #[allow(dead_code)]
    pub fn with_max_clock_drift(mut self, max_clock_drift: ClockDrift) -> HlcClock {
        self.max_clock_drift = max_clock_drift;
        self
    }
}

impl Clock for HlcClock {
    /// Mint a fresh timestamp for a local event (a write or an outgoing message).
    ///
    /// The returned timestamp is strictly greater than every timestamp previously produced
    /// or observed by this clock, ensuring local monotonicity.
    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let pt = phys_now();
        let mut last = self.last.lock();
        let next = if pt > last.physical() {
            Hlc::new(pt, LogicalCounter::ZERO)
        } else {
            // Physical time has not advanced past the last stored value; bump the logical
            // counter. next_tick() handles the u32::MAX → (physical + 1, 0) roll, so no wrap.
            last.next_tick()
        };
        *last = next;
        Timestamp::new(next, self.node_id)
    }

    /// Advance the clock to account for a timestamp received from a peer.
    ///
    /// After observing `remote`, a subsequent [`now`](Clock::now) is guaranteed to be
    /// greater than `remote`, so a local write following the receipt of a remote value is
    /// ordered after it. This is what prevents lost updates under clock skew.
    ///
    /// **Far-future clamp**: this is the untrusted path, so the remote physical-time reading is
    /// admitted
    /// through [`AdmittedTime::clamped_to_drift`] — if it exceeds physical now by more than this
    /// clock's configured `max_clock_drift` (default [`MAX_CLOCK_DRIFT`]), it is treated as though
    /// it arrived at `phys_now + max_clock_drift`. A `warn!` is emitted so operators can detect
    /// misbehaving or compromised peers. The remote's own `Timestamp` is left untouched for LWW
    /// purposes; only the local clock state is protected.
    fn observe(&self, remote: Timestamp) {
        let pt = phys_now();
        let mut last = self.last.lock();

        // Clamp the remote physical time so a buggy or malicious peer cannot pin the local clock
        // arbitrarily far into the future (see MAX_CLOCK_DRIFT for the full rationale). The
        // resulting `AdmittedTime` is the only thing `advance_past_remote` accepts.
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

    /// Advance the clock past a stamp this node itself authored (e.g. restored from persisted
    /// state), without applying the far-future clamp used for remote peer stamps.
    ///
    /// The clamp guards against a hostile or buggy peer injecting an arbitrarily large physical
    /// time; it must not fire on self-authored stamps. If the wall clock stepped backward by
    /// more than [`MAX_CLOCK_DRIFT`] across a restart (NTP step, VM resume), an honest
    /// persisted stamp would exceed `phys_now + MAX_CLOCK_DRIFT` and the clamped path would
    /// fail to advance the clock past it, re-introducing the own-write-shadowing bug. This is the
    /// one place entitled to [`AdmittedTime::trusted`], and it has to say so.
    fn observe_trusted(&self, remote: Timestamp) {
        let pt = phys_now();
        let mut last = self.last.lock();
        // No clamp: the stamp is ours, and we say so.
        last.advance_past_remote(
            pt,
            AdmittedTime::trusted(remote.physical()),
            remote.logical(),
        );
    }
}

/// Deterministic [`Clock`] adapter for tests: no physical-time read at all.
///
/// [`now`](Clock::now) bumps the logical counter; [`observe`](Clock::observe) jumps to a strictly
/// greater stamp. Stamps are therefore fully reproducible, which is what lets engine/HLC tests be
/// deterministic without real wall-clock time — the testability the [`Clock`] port exists to give.
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
        // Go through the domain's own tick so the test double exercises the real rule, including
        // the counter roll at u32::MAX — rebuilding the tick by hand here would re-implement it,
        // and would drop the roll.
        let next = last.next_tick();
        *last = next;
        Timestamp::new(next, self.node_id)
    }

    fn observe(&self, remote: Timestamp) {
        let mut last = self.last.lock();
        if remote.hlc() > *last {
            // Adopt the remote reading so the next `now` is ordered strictly after `remote`; our
            // own node_id is attached at mint time, not stored here.
            *last = remote.hlc();
        }
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
        // Force the clock a little into the future (within MAX_CLOCK_DRIFT) so physical
        // time cannot advance past it for the duration of the test: every `now()` must then
        // bump the counter. We no longer use u64::MAX here because the far-future clamp
        // (see observe()) correctly rejects values beyond phys_now + MAX_CLOCK_DRIFT.
        let near_future = phys_now().saturating_add(ClockDrift::from_millis(60_000)); // 60 s ahead
        clock.observe(Timestamp::new(
            Hlc::new(near_future, LogicalCounter::ZERO),
            NodeId::new(9),
        ));
        let a = clock.now();
        let b = clock.now();
        assert_eq!(a.physical(), b.physical());
        // Exactly one logical tick apart — expressed through the domain's own rule rather than
        // rebuilding it from the raw counter.
        assert_eq!(b, Timestamp::new(a.hlc().next_tick(), a.node_id()));
    }

    #[test]
    fn observe_advances_past_a_future_timestamp() {
        // Legitimate skew: a peer with a clock running a few seconds ahead. After observing its
        // timestamp, our next local write must be ordered *after* it, not lost. (Far-future
        // stamps beyond MAX_CLOCK_DRIFT are clamped; see `observe_far_future_is_clamped`.)
        let clock = HlcClock::new(NodeId::new(1));
        // 5 s ahead: well within cap.
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
        // The test adapter reads no wall clock, so the stamp sequence is fully reproducible.
        let clock = ManualClock::new(NodeId::new(7));
        assert_eq!(clock.now(), ts(0, 1, 7));
        assert_eq!(clock.now(), ts(0, 2, 7));
        // Observing a future remote stamp jumps the clock; the next mint is ordered after it.
        let remote = ts(50, 4, 9);
        clock.observe(remote);
        let local = clock.now();
        assert_eq!(local, ts(50, 5, 7));
        assert!(local > remote);
    }

    /// Observing a stamp near u64::MAX must not pin the local clock anywhere near u64::MAX.
    /// The next `now()` must be within phys_now + MAX_CLOCK_DRIFT + small margin,
    /// and strict monotonicity relative to any previously minted stamp must hold.
    #[test]
    fn observe_far_future_is_clamped() {
        let clock = HlcClock::new(NodeId::new(1));

        // Mint one local stamp first so we have a baseline for the monotonicity check.
        let before_clamp = clock.now();

        // Adversarial stamp: physical time near u64::MAX.
        let adversarial = ts(u64::MAX - 1, 0, 99);
        clock.observe(adversarial);

        // The next mint must be strictly after `before_clamp` (monotonicity preserved) …
        let after_clamp = clock.now();
        assert!(
            after_clamp > before_clamp,
            "monotonicity violated: {after_clamp:?} !> {before_clamp:?}"
        );

        // … but must NOT be anywhere near u64::MAX.
        // Allow a generous margin above the cap: the result may be at cap + 1 due to the logical
        // tick, but must never approach the adversarial physical time.
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
        // Feed three different stamps all well beyond the cap.
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

    /// When the counter saturates at u32::MAX while physical time is pinned, the next `now()`
    /// must roll `physical` forward by 1 ms and reset the counter to 0, producing a strictly
    /// greater timestamp with no wrapping.
    #[test]
    fn counter_overflow_rolls_physical_forward() {
        let clock = HlcClock::new(NodeId::new(3));

        // Pin the local clock to a physical value and max counter by directly observing a stamp.
        // We set physical time to phys_now + 1 ms so real time will not advance past it during the
        // test (giving us deterministic counter behavior), but stay within the drift cap.
        let pinned_physical = phys_now().next_ms();
        let max_counter_stamp = Timestamp::new(
            Hlc::new(pinned_physical, LogicalCounter::new(u32::MAX)),
            NodeId::new(99),
        );
        clock.observe(max_counter_stamp);

        // observe() must have handled the overflow: the stored state is (pinned_physical+1, 0).
        // now() must produce a stamp strictly greater than max_counter_stamp.
        let rolled = clock.now();

        assert!(
            rolled > max_counter_stamp,
            "timestamp not strictly greater after counter roll: {rolled:?} vs {max_counter_stamp:?}"
        );

        // The physical component must have advanced past pinned_physical.
        assert!(
            rolled.physical() > pinned_physical,
            "physical time did not roll forward: {rolled:?}"
        );
    }

    /// `observe_trusted` of a stamp far beyond `phys_now + MAX_CLOCK_DRIFT` must advance the
    /// clock all the way past that stamp (so the next `now()` is strictly greater than it), while
    /// plain `observe` of the same stamp stays clamped and the next `now()` stays well below it.
    ///
    /// This pins the trusted/untrusted distinction: the trusted path is needed for persisted
    /// stamps when the wall clock stepped backward by more than MAX_CLOCK_DRIFT (NTP step,
    /// VM resume). Without it, the clamped path would leave the clock below the persisted max,
    /// and a fresh write would shadow an older persisted value — the own-write-shadowing bug.
    #[test]
    fn observe_trusted_bypasses_far_future_clamp() {
        // A stamp far beyond the cap — the exact scenario of a wall-clock backward step that
        // makes an honest persisted stamp land outside phys_now + MAX_CLOCK_DRIFT.
        let far_future = Timestamp::new(
            Hlc::new(
                phys_now()
                    .saturating_add(MAX_CLOCK_DRIFT)
                    .saturating_add(ClockDrift::from_millis(5_000_000)),
                LogicalCounter::new(3),
            ),
            NodeId::new(7),
        );

        // ---- trusted path: clock must chase the stamp ----
        let trusted_clock = HlcClock::new(NodeId::new(1));
        trusted_clock.observe_trusted(far_future);
        let after_trusted = trusted_clock.now();
        assert!(
            after_trusted > far_future,
            "observe_trusted did not advance the clock past the far-future stamp: \
             next now() {after_trusted:?} is not > {far_future:?}"
        );

        // ---- clamped path: clock must NOT chase the stamp ----
        let clamped_clock = HlcClock::new(NodeId::new(2));
        clamped_clock.observe(far_future);
        let after_clamped = clamped_clock.now();
        // Re-read physical time for the cap bound: `observe`/`now` recompute the cap against a
        // fresh phys_now, so basing the bound on a stale reading from the top of the test would
        // flake if the wall clock advanced more than a few ms during execution. `cap_upper`
        // re-reads now and adds slack for the +1 the logical tick may contribute.
        let cap_upper = phys_now()
            .saturating_add(MAX_CLOCK_DRIFT)
            .saturating_add(ClockDrift::from_millis(10));
        assert!(
            after_clamped.physical() <= cap_upper,
            "observe (clamped) let physical time escape the cap: {:?} > {:?}",
            after_clamped.physical(),
            cap_upper
        );
        // Confirm the clamped result is below the far-future stamp (pins the distinction).
        assert!(
            after_clamped < far_future,
            "clamped observe produced a stamp >= the far-future value: \
             {after_clamped:?} should be < {far_future:?}"
        );
    }

    /// The four regimes of `BoundedInstant::from_stored_stamp`, at the level of the helper itself.
    /// The same four are exercised end-to-end through the tombstone hook in `replicated_map`.
    #[test]
    fn bounded_instant_covers_every_stamp_regime() {
        let budget = MAX_CLOCK_DRIFT;

        // (1) Normal stamp — a second ago. Used verbatim, so replicas still agree on the instant.
        let normal = phys_now().millis() - 1_000;
        let b = BoundedInstant::from_stored_stamp(PhysicalTime::from_millis(normal), budget);
        assert_eq!(b.bound(), StampBound::Verbatim);
        assert_eq!(b.instant().timestamp_millis(), normal as i64);

        // (2) Far future but inside chrono's range: the regime where the old `as i64` cast was
        // *exact*, so fixing the cast alone would have left the tombstone dated centuries out.
        let far = phys_now().millis() + 10_000 * 365 * 24 * 3_600_000; // ~10 000 years ahead
        let b = BoundedInstant::from_stored_stamp(PhysicalTime::from_millis(far), budget);
        assert_eq!(b.bound(), StampBound::Capped);
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

        // (3) Above i64::MAX: the regime the old cast wrapped negative into 1970. It must come
        // back as the same cap as (2), never as a pre-epoch date.
        let b = BoundedInstant::from_stored_stamp(PhysicalTime::from_millis(u64::MAX), budget);
        assert_eq!(b.bound(), StampBound::Capped);
        assert!(
            b.instant().timestamp_millis() > 0,
            "wrapped to a pre-epoch instant: {}",
            b.instant()
        );
        assert!(b.instant().timestamp_millis() <= cap_upper);

        // (4) Residual path: only reachable with a budget so large that `now + budget` saturates
        // past chrono's year-262143 ceiling. Falls back to local now, and says so.
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

    /// The clamp threshold is a clock-level knob: a clock built with a tighter `max_clock_drift`
    /// clamps a remote stamp that the default 1-hour bound would have accepted.
    #[test]
    fn custom_max_clock_drift_is_respected() {
        let drift = ClockDrift::from_millis(1_000); // 1 s cap, far tighter than the 1-hour default
        let clock = HlcClock::new(NodeId::new(1)).with_max_clock_drift(drift);

        // A stamp 60 s ahead is well within the default cap but well beyond this clock's 1 s cap,
        // so observing it must clamp the local clock rather than chase the remote physical time.
        let remote_physical = phys_now().saturating_add(ClockDrift::from_millis(60_000));
        clock.observe(Timestamp::new(
            Hlc::new(remote_physical, LogicalCounter::ZERO),
            NodeId::new(99),
        ));

        let minted = clock.now();
        // Small margin for the logical tick and elapsed time.
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
}
