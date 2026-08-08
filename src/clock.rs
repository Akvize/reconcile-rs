// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The wall-clock **adapter** behind the [`Clock`] port.
//!
//! The Hybrid Logical Clock's value type ([`Timestamp`]), its port ([`Clock`]) and its ordering
//! arithmetic live in the infrastructure-free [`lww_register::clock`] module. What is left here is
//! the one thing that cannot: `HlcClock`, the adapter that reads physical time through `chrono`
//! and feeds it into that arithmetic. It owns the only wall-clock read in the crate.
//!
//! [`Timestamp`], [`Clock`] and [`MAX_CLOCK_DRIFT_MS`] are re-exported here so that
//! `reconcile::clock::*` keeps resolving exactly as before the workspace split.

use parking_lot::Mutex;
use tracing::warn;

pub use lww_register::clock::{advance, advance_past, Clock, Timestamp, MAX_CLOCK_DRIFT_MS};

use chrono::Utc;

/// Read physical time as milliseconds since the Unix epoch.
fn phys_now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// A per-node Hybrid Logical Clock — the default [`Clock`] adapter.
///
/// Generates locally-monotonic [`Timestamp`]s with [`now`](Clock::now) and advances
/// past timestamps received from peers with [`observe`](Clock::observe). The clock is
/// internally synchronized, so a single instance is shared (cloned) across all tasks of a
/// node. It owns the only physical-time read in the crate (`phys_now_ms`); the ordering rule it
/// applies to that reading is [`lww_register::clock::advance_past`].
#[derive(Debug)]
pub(crate) struct HlcClock {
    node_id: u64,
    /// Maximum milliseconds a remote stamp may lead physical time before [`observe`](Clock::observe)
    /// clamps it when advancing the local clock state. Owned by the clock (not the store), defaulting
    /// to [`MAX_CLOCK_DRIFT_MS`] and overridable via
    /// [`with_max_clock_drift_ms`](HlcClock::with_max_clock_drift_ms).
    max_clock_drift_ms: u64,
    /// Last timestamp produced or observed; the wall/counter pair is updated atomically
    /// under the mutex so that [`now`](HlcClock::now) stays strictly monotonic.
    last: Mutex<Timestamp>,
}

impl HlcClock {
    /// Create a clock for the node identified by `node_id`, using the default far-future clamp
    /// threshold ([`MAX_CLOCK_DRIFT_MS`]). Override it with
    /// [`with_max_clock_drift_ms`](HlcClock::with_max_clock_drift_ms).
    pub fn new(node_id: u64) -> HlcClock {
        HlcClock {
            node_id,
            max_clock_drift_ms: MAX_CLOCK_DRIFT_MS,
            last: Mutex::new(Timestamp::new(0, 0, node_id)),
        }
    }

    /// Override how far (in milliseconds) a remote stamp may lead physical time before
    /// [`observe`](Clock::observe) clamps it (default [`MAX_CLOCK_DRIFT_MS`]). The clamp
    /// threshold is a clock concern, configured here rather than through the store's
    /// [`Config`](crate::replicated_map::Config).
    #[allow(dead_code)]
    pub fn with_max_clock_drift_ms(mut self, max_clock_drift_ms: u64) -> HlcClock {
        self.max_clock_drift_ms = max_clock_drift_ms;
        self
    }
}

impl Clock for HlcClock {
    /// Mint a fresh timestamp for a local event (a write or an outgoing message).
    ///
    /// The returned timestamp is strictly greater than every timestamp previously produced
    /// or observed by this clock, ensuring local monotonicity.
    fn node_id(&self) -> u64 {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let pt = phys_now_ms();
        let mut last = self.last.lock();
        let next = if pt > last.wall_ms() {
            Timestamp::new(pt, 0, self.node_id)
        } else {
            // Physical time has not advanced past the last stored wall; bump the counter.
            // advance() handles the u32::MAX → (wall+1, 0) rollover so we cannot wrap.
            let (wall_ms, counter) = advance(last.wall_ms(), last.counter());
            Timestamp::new(wall_ms, counter, self.node_id)
        };
        *last = next;
        next
    }

    /// Advance the clock to account for a timestamp received from a peer.
    ///
    /// After observing `remote`, a subsequent [`now`](Clock::now) is guaranteed to be
    /// greater than `remote`, so a local write following the receipt of a remote value is
    /// ordered after it. This is what prevents lost updates under clock skew.
    ///
    /// **Far-future clamp**: if `remote.wall_ms` exceeds physical now by more than this clock's
    /// configured `max_clock_drift_ms` (default [`MAX_CLOCK_DRIFT_MS`]), it is treated as
    /// though it arrived at `phys_now + max_clock_drift_ms`. A `warn!` is emitted so operators can
    /// detect misbehaving or compromised peers. The remote's own `Timestamp` is left untouched
    /// for LWW purposes; only the local clock state is protected.
    fn observe(&self, remote: Timestamp) {
        let pt = phys_now_ms();
        let mut last = self.last.lock();

        // Clamp remote.wall_ms so a buggy or malicious peer cannot pin the local clock
        // arbitrarily far into the future (see MAX_CLOCK_DRIFT_MS for the full rationale).
        let cap = pt.saturating_add(self.max_clock_drift_ms);
        let effective_remote_wall = if remote.wall_ms() > cap {
            warn!(
                remote_wall_ms = remote.wall_ms(),
                remote_node_id = remote.node_id(),
                phys_now_ms = pt,
                cap_ms = cap,
                max_clock_drift_ms = self.max_clock_drift_ms,
                "remote timestamp leads local clock by more than the configured max drift; \
                 clamping to cap to protect local clock state"
            );
            cap
        } else {
            remote.wall_ms()
        };

        advance_past(
            &mut last,
            pt,
            effective_remote_wall,
            remote.counter(),
            self.node_id,
        );
    }

    /// Advance the clock past a stamp this node itself authored (e.g. restored from persisted
    /// state), without applying the far-future clamp used for remote peer stamps.
    ///
    /// The clamp guards against a hostile or buggy peer injecting an arbitrarily large wall
    /// value; it must not fire on self-authored stamps. If the wall clock stepped backward by
    /// more than [`MAX_CLOCK_DRIFT_MS`] across a restart (NTP step, VM resume), an honest
    /// persisted stamp would exceed `phys_now + MAX_CLOCK_DRIFT_MS` and the clamped path would
    /// fail to advance the clock past it, re-introducing the own-write-shadowing bug.
    fn observe_trusted(&self, remote: Timestamp) {
        let pt = phys_now_ms();
        let mut last = self.last.lock();
        // No clamp: pass the raw wall value directly.
        advance_past(
            &mut last,
            pt,
            remote.wall_ms(),
            remote.counter(),
            self.node_id,
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
    node_id: u64,
    last: Mutex<Timestamp>,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new(node_id: u64) -> ManualClock {
        ManualClock {
            node_id,
            last: Mutex::new(Timestamp::new(0, 0, node_id)),
        }
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn node_id(&self) -> u64 {
        self.node_id
    }

    fn now(&self) -> Timestamp {
        let mut last = self.last.lock();
        let next = Timestamp::new(last.wall_ms(), last.counter() + 1, self.node_id);
        *last = next;
        next
    }

    fn observe(&self, remote: Timestamp) {
        let mut last = self.last.lock();
        if remote > *last {
            // Adopt the remote wall/counter (under our own node_id) so the next `now` is ordered
            // strictly after `remote`.
            *last = Timestamp::new(remote.wall_ms(), remote.counter(), self.node_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_strictly_monotonic() {
        let clock = HlcClock::new(1);
        let mut prev = clock.now();
        for _ in 0..10_000 {
            let next = clock.now();
            assert!(next > prev, "{next:?} !> {prev:?}");
            prev = next;
        }
    }

    #[test]
    fn counter_increments_when_wall_does_not_advance() {
        let clock = HlcClock::new(1);
        // Force the clock a little into the future (within MAX_CLOCK_DRIFT_MS) so physical
        // time cannot advance past it for the duration of the test: every `now()` must then
        // bump the counter. We no longer use u64::MAX here because the far-future clamp
        // (see observe()) correctly rejects values beyond phys_now + MAX_CLOCK_DRIFT_MS.
        let near_future = phys_now_ms() + 60_000; // 60 s ahead — well within the 1-hour cap
        clock.observe(Timestamp::new(near_future, 0, 9));
        let a = clock.now();
        let b = clock.now();
        assert_eq!(a.wall_ms(), b.wall_ms());
        assert_eq!(b.counter(), a.counter() + 1);
    }

    #[test]
    fn observe_advances_past_a_future_timestamp() {
        // Reproduces defect (a) for *legitimate* skew: a peer with a clock running a few
        // seconds ahead. After observing its timestamp, our next local write must be ordered
        // *after* it, not lost. (Far-future stamps beyond MAX_CLOCK_DRIFT_MS are clamped;
        // see `observe_far_future_is_clamped` for that case.)
        let clock = HlcClock::new(1);
        let future = Timestamp::new(phys_now_ms() + 5_000, 5, 2); // 5 s ahead: well within cap
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
        let clock = ManualClock::new(7);
        assert_eq!(clock.now(), Timestamp::new(0, 1, 7));
        assert_eq!(clock.now(), Timestamp::new(0, 2, 7));
        // Observing a future remote stamp jumps the clock; the next mint is ordered after it.
        let remote = Timestamp::new(50, 4, 9);
        clock.observe(remote);
        let local = clock.now();
        assert_eq!(local, Timestamp::new(50, 5, 7));
        assert!(local > remote);
    }

    // ----- New tests for the two bug fixes -----

    /// Observing a stamp near u64::MAX must not pin the local clock anywhere near u64::MAX.
    /// The next `now()` must be within phys_now + MAX_CLOCK_DRIFT_MS + small margin,
    /// and strict monotonicity relative to any previously minted stamp must hold.
    #[test]
    fn observe_far_future_is_clamped() {
        let clock = HlcClock::new(1);

        // Mint one local stamp first so we have a baseline for the monotonicity check.
        let before_clamp = clock.now();

        // Adversarial stamp: wall_ms near u64::MAX.
        let adversarial = Timestamp::new(u64::MAX - 1, 0, 99);
        clock.observe(adversarial);

        // The next mint must be strictly after `before_clamp` (monotonicity preserved) …
        let after_clamp = clock.now();
        assert!(
            after_clamp > before_clamp,
            "monotonicity violated: {after_clamp:?} !> {before_clamp:?}"
        );

        // … but must NOT be anywhere near u64::MAX.
        let pt = phys_now_ms();
        // Allow a generous margin above the cap: the result may be at cap + 1 due to advance(),
        // but must never approach adversarial.wall_ms.
        let upper_bound = pt + MAX_CLOCK_DRIFT_MS + 10;
        assert!(
            after_clamp.wall_ms() <= upper_bound,
            "clock was not clamped: wall_ms {} >> cap {}",
            after_clamp.wall_ms(),
            upper_bound
        );
    }

    /// Repeated observes of increasing far-future stamps must not ratchet past the cap.
    #[test]
    fn repeated_far_future_observes_do_not_escape_cap() {
        let clock = HlcClock::new(2);
        // Feed three different stamps all well beyond the cap.
        for delta in [u64::MAX / 2, u64::MAX - 500, u64::MAX - 1] {
            clock.observe(Timestamp::new(delta, 0, 99));
        }
        let minted = clock.now();
        let pt = phys_now_ms();
        let upper_bound = pt + MAX_CLOCK_DRIFT_MS + 10;
        assert!(
            minted.wall_ms() <= upper_bound,
            "wall_ms {} escaped the cap {}",
            minted.wall_ms(),
            upper_bound
        );
    }

    /// When the counter saturates at u32::MAX while the wall is pinned, the next `now()`
    /// must roll wall_ms forward by 1 ms and reset counter to 0, producing a strictly
    /// greater timestamp with no wrapping.
    #[test]
    fn counter_overflow_rolls_wall_forward() {
        let clock = HlcClock::new(3);

        // Pin the local clock to a wall value and max counter by directly observing a stamp.
        // We set wall_ms to phys_now + 1 so physical time will not advance past it during the
        // test (giving us deterministic counter behavior), but stay within the drift cap.
        let pinned_wall = phys_now_ms() + 1;
        let max_counter_stamp = Timestamp::new(pinned_wall, u32::MAX, 99);
        clock.observe(max_counter_stamp);

        // observe() must have handled the overflow: the stored state is (pinned_wall+1, 0).
        // now() must produce a stamp strictly greater than max_counter_stamp.
        let rolled = clock.now();

        assert!(
            rolled > max_counter_stamp,
            "timestamp not strictly greater after counter roll: {rolled:?} vs {max_counter_stamp:?}"
        );

        // wall_ms must have advanced past pinned_wall.
        assert!(
            rolled.wall_ms() > pinned_wall,
            "wall_ms did not roll forward: {rolled:?}"
        );
    }

    /// `observe_trusted` of a stamp far beyond `phys_now + MAX_CLOCK_DRIFT_MS` must advance the
    /// clock all the way past that stamp (so the next `now()` is strictly greater than it), while
    /// plain `observe` of the same stamp stays clamped and the next `now()` stays well below it.
    ///
    /// This pins the trusted/untrusted distinction: the trusted path is needed for persisted
    /// stamps when the wall clock stepped backward by more than MAX_CLOCK_DRIFT_MS (NTP step,
    /// VM resume). Without it, the clamped path would leave the clock below the persisted max,
    /// and a fresh write would shadow an older persisted value — the own-write-shadowing bug.
    #[test]
    fn observe_trusted_bypasses_far_future_clamp() {
        let pt = phys_now_ms();
        // A stamp far beyond the cap — the exact scenario of a wall-clock backward step that
        // makes an honest persisted stamp land outside phys_now + MAX_CLOCK_DRIFT_MS.
        let far_future = Timestamp::new(pt + MAX_CLOCK_DRIFT_MS + 5_000_000, 3, 7);

        // ---- trusted path: clock must chase the stamp ----
        let trusted_clock = HlcClock::new(1);
        trusted_clock.observe_trusted(far_future);
        let after_trusted = trusted_clock.now();
        assert!(
            after_trusted > far_future,
            "observe_trusted did not advance the clock past the far-future stamp: \
             next now() {after_trusted:?} is not > {far_future:?}"
        );

        // ---- clamped path: clock must NOT chase the stamp ----
        let clamped_clock = HlcClock::new(2);
        clamped_clock.observe(far_future);
        let after_clamped = clamped_clock.now();
        // Re-read physical time for the cap bound: `observe`/`now` recompute the cap against a
        // fresh phys_now, so basing the bound on the stale `pt` from the top of the test would
        // flake if the wall clock advanced more than a few ms during execution. `cap_upper`
        // re-reads now and adds slack for the +1 that `advance()` may contribute.
        let cap_upper = phys_now_ms() + MAX_CLOCK_DRIFT_MS + 10;
        assert!(
            after_clamped.wall_ms() <= cap_upper,
            "observe (clamped) let wall_ms escape the cap: {} > {}",
            after_clamped.wall_ms(),
            cap_upper
        );
        // Confirm the clamped result is below the far-future stamp (pins the distinction).
        assert!(
            after_clamped < far_future,
            "clamped observe produced a stamp >= the far-future value: \
             {after_clamped:?} should be < {far_future:?}"
        );
    }

    /// The clamp threshold is a clock-level knob: a clock built with a tighter `max_clock_drift_ms`
    /// clamps a remote stamp that the default 1-hour bound would have accepted.
    #[test]
    fn custom_max_clock_drift_is_respected() {
        let drift = 1_000; // 1 s cap, far tighter than the 1-hour default
        let clock = HlcClock::new(1).with_max_clock_drift_ms(drift);

        // A stamp 60 s ahead is well within the default cap but well beyond this clock's 1 s cap,
        // so observing it must clamp the local clock rather than chase the remote wall.
        let pt_before = phys_now_ms();
        clock.observe(Timestamp::new(pt_before + 60_000, 0, 99));

        let minted = clock.now();
        let pt = phys_now_ms();
        let upper_bound = pt + drift + 10; // small margin for advance()/elapsed time
        assert!(
            minted.wall_ms() <= upper_bound,
            "custom drift cap not enforced: wall_ms {} > cap {}",
            minted.wall_ms(),
            upper_bound
        );
    }
}
