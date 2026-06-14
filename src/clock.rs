// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Hybrid Logical Clock (HLC) used to timestamp values for conflict resolution.
//!
//! Conflict resolution in [`ReconcileStore`](crate::ReconcileStore) is last-write-wins (LWW).
//! Keying LWW on a raw physical wall-clock (`DateTime<Utc>`) is unsafe:
//!
//! * under clock skew, a node whose clock runs ahead always wins, silently losing
//!   causally-newer writes from other nodes;
//! * on *equal* timestamps a naive tie-break is not commutative, so two replicas can each
//!   keep their own value forever. Since the timestamp is part of the reconciliation hash,
//!   their fingerprints never match and the protocol re-exchanges the pair eternally
//!   (permanent divergence + livelock).
//!
//! A [`Timestamp`] fixes both. It is a 64-bit-ish hybrid timestamp (Kulkarni et al., 2014) that:
//!
//! * stays close to physical time, yet is **locally monotonic** and **respects causality**:
//!   on receiving a remote timestamp a node advances its own clock past it (the engine's
//!   internal clock observes every inbound timestamp), so a subsequent local write is ordered
//!   *after* everything it has seen — no lost update under bounded skew;
//! * carries a `node_id`, giving a **globally deterministic total order**
//!   `(wall_ms, counter, node_id)`. Every replica therefore picks the *same* survivor on a
//!   conflict, which makes the merge commutative, associative and idempotent — i.e. genuine
//!   Strong Eventual Consistency.
//!
//! LWW still discards one of two *genuinely concurrent* writes by design; recovering both
//! would require version vectors or a CRDT and is out of scope.

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Default maximum number of milliseconds by which a remote clock may lead physical time before
/// its `wall_ms` is clamped when updating the local clock state.
///
/// This is only the default the `HlcClock` adapter is built with; the threshold is a property of
/// the clock itself, overridable at construction (see `HlcClock::with_max_clock_drift_ms`) rather
/// than a knob on the store's [`Config`](crate::reconcile_store::Config). The rationale below
/// explains why this default value was chosen.
///
/// **Why 1 hour?**
/// NTP-disciplined clocks rarely deviate by more than a few hundred milliseconds in practice;
/// even aggressively skewed or misconfigured peers stay well under a minute ahead. One hour
/// (3 600 000 ms) is therefore orders of magnitude above any legitimate skew while still
/// being finite, giving huge headroom for leap-second smearing, suspended VMs resuming, and
/// other real-world anomalies. In the default unauthenticated mode the gossip socket accepts
/// packets from any sender, so a single malicious or buggy peer can inject arbitrary
/// `wall_ms` values; without a cap, one packet stamped near `u64::MAX` would pin every node's
/// clock to that value permanently, destroying LWW recency. (The clamp protects the local
/// clock state only: a stored value keeps its original stamp as LWW data, so downstream
/// consumers of stored stamps — e.g. the tombstone expiry arithmetic in `reconcile_store`,
/// where `wall_ms as i64` can turn negative — must defend against oversized stamps
/// themselves.)
///
/// **Clamp semantics (strict monotonicity is preserved):**
/// The clamp is applied only inside the [`Clock::observe`] implementation; it limits how far a
/// remote stamp may advance *the local clock state* (`last`). It does **not** retroactively alter any
/// timestamp that was already minted: if the local clock was legitimately advanced to some
/// `T` before encountering an out-of-bounds remote, the next `now()` will still return a
/// value `> T`. Put differently, the clamp prevents *future* poisoning; it does not wind the
/// clock back.
///
/// A clamped remote stamp is still valid data in the LWW comparison — the remote's own
/// `Timestamp` is returned to the caller unchanged and will win if it is numerically larger
/// than competing local values. The clamp only stops the *local clock* from chasing that
/// value into the far future.
pub const DEFAULT_MAX_CLOCK_DRIFT_MS: u64 = 3_600_000; // 1 hour

/// A Hybrid Logical Clock timestamp.
///
/// The fields are compared in declaration order, so the derived [`Ord`] is exactly the
/// total order `(wall_ms, counter, node_id)` used to resolve conflicts. See the
/// [module documentation](crate::clock) for the rationale.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct Timestamp {
    /// Physical component: milliseconds since the Unix epoch, as last observed by the clock.
    wall_ms: u64,
    /// Logical component: disambiguates events sharing the same `wall_ms`.
    counter: u32,
    /// Identity of the node that minted this timestamp; provides the deterministic tie-break.
    node_id: u64,
}

impl Timestamp {
    /// Build a `Timestamp` from its raw components.
    ///
    /// Mostly useful in tests and when reconstructing a timestamp from external storage;
    /// normal code obtains timestamps from the store's internal clock.
    pub fn new(wall_ms: u64, counter: u32, node_id: u64) -> Timestamp {
        Timestamp {
            wall_ms,
            counter,
            node_id,
        }
    }

    /// Physical component (milliseconds since the Unix epoch).
    pub fn wall_ms(&self) -> u64 {
        self.wall_ms
    }

    /// Logical counter component.
    pub fn counter(&self) -> u32 {
        self.counter
    }

    /// Identity of the node that minted this timestamp.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }
}

/// Read physical time as milliseconds since the Unix epoch.
fn phys_now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

/// Advance a `(wall_ms, counter)` pair by one logical tick without wrapping.
///
/// Normal case: increment the counter within the same millisecond.
/// Overflow case: when the counter is already at `u32::MAX`, bump `wall_ms` by 1 ms and
/// reset the counter to 0. This is the standard HLC fallback and preserves strict
/// monotonicity: the resulting `(wall_ms + 1, 0)` is always greater than `(wall_ms, u32::MAX)`.
///
/// `wall_ms.saturating_add(1)` ensures that even a wall value of `u64::MAX` cannot wrap
/// (it saturates at `u64::MAX`, which keeps the pair non-decreasing in the degenerate case).
fn advance(wall_ms: u64, counter: u32) -> (u64, u32) {
    match counter.checked_add(1) {
        Some(c) => (wall_ms, c),
        None => (wall_ms.saturating_add(1), 0),
    }
}

/// The domain's **clock port**: the seam through which the reconciliation engine reads time.
///
/// The Hybrid Logical Clock algorithm stays in the domain; an adapter behind this port performs
/// the single physical-time read (`HlcClock` is the default adapter, a test adapter can be a
/// deterministic stub). Pinning the timestamp to [`Timestamp`] — rather than a generic associated type —
/// keeps the port object-safe and avoids leaking a clock type parameter into the engine, store and
/// `Config` (`ARCHITECTURE.md` §3.4); the engine therefore holds the port as `Arc<dyn Clock>`.
pub trait Clock: Send + Sync + 'static {
    /// Mint a strictly-monotonic local timestamp for a write or an outgoing message.
    fn now(&self) -> Timestamp;
    /// Advance the clock past a timestamp received from a peer, so that a subsequent
    /// [`now`](Clock::now) is ordered after it (this is what prevents lost updates under skew).
    ///
    /// This "ordered-after" guarantee holds for remote stamps within a bounded lead over
    /// physical time; an implementation may clamp a remote stamp that leads physical time by
    /// an implausibly large, configurable margin (default [`DEFAULT_MAX_CLOCK_DRIFT_MS`]) so
    /// that a single poisoned stamp cannot pin the local clock into the far future. The
    /// total order on [`Timestamp`] and the strict-`>` merge are unaffected.
    fn observe(&self, remote: Timestamp);
    /// Advance the clock past a stamp that **this node itself authored** (e.g. restored from its
    /// own persisted state), so that the first post-restart [`now`](Clock::now) is strictly
    /// ordered after every pre-restart write.
    ///
    /// Unlike [`observe`](Clock::observe), implementations must **not** apply the far-future
    /// suspicion clamp to a self-authored stamp. The clamp guards against a remote peer injecting
    /// an arbitrarily large wall value; it must not fire on a stamp we wrote ourselves, because
    /// refusing to chase our own past output re-introduces own-write shadowing after a backward
    /// clock step (NTP correction, VM resume) that moved physical time behind the persisted max.
    ///
    /// The default implementation delegates to [`observe`](Clock::observe), which is safe for
    /// adapters that already have no clamp (e.g. the test `ManualClock`). `HlcClock` overrides
    /// this with a clamp-free advance to preserve the guarantee above.
    fn observe_trusted(&self, remote: Timestamp) {
        self.observe(remote);
    }
}

/// A per-node Hybrid Logical Clock — the default [`Clock`] adapter.
///
/// Generates locally-monotonic [`Timestamp`]s with [`now`](Clock::now) and advances
/// past timestamps received from peers with [`observe`](Clock::observe). The clock is
/// internally synchronized, so a single instance is shared (cloned) across all tasks of a
/// node. It owns the only physical-time read in the crate (`phys_now_ms`).
#[derive(Debug)]
pub(crate) struct HlcClock {
    node_id: u64,
    /// Maximum milliseconds a remote stamp may lead physical time before [`observe`](Clock::observe)
    /// clamps it when advancing the local clock state. Owned by the clock (not the store), defaulting
    /// to [`DEFAULT_MAX_CLOCK_DRIFT_MS`] and overridable via
    /// [`with_max_clock_drift_ms`](HlcClock::with_max_clock_drift_ms).
    max_clock_drift_ms: u64,
    /// Last timestamp produced or observed; the wall/counter pair is updated atomically
    /// under the mutex so that [`now`](HlcClock::now) stays strictly monotonic.
    last: Mutex<Timestamp>,
}

impl HlcClock {
    /// Create a clock for the node identified by `node_id`, using the default far-future clamp
    /// threshold ([`DEFAULT_MAX_CLOCK_DRIFT_MS`]). Override it with
    /// [`with_max_clock_drift_ms`](HlcClock::with_max_clock_drift_ms).
    pub fn new(node_id: u64) -> HlcClock {
        HlcClock {
            node_id,
            max_clock_drift_ms: DEFAULT_MAX_CLOCK_DRIFT_MS,
            last: Mutex::new(Timestamp {
                wall_ms: 0,
                counter: 0,
                node_id,
            }),
        }
    }

    /// Override how far (in milliseconds) a remote stamp may lead physical time before
    /// [`observe`](Clock::observe) clamps it (default [`DEFAULT_MAX_CLOCK_DRIFT_MS`]). The clamp
    /// threshold is a clock concern, configured here rather than through the store's
    /// [`Config`](crate::reconcile_store::Config).
    #[allow(dead_code)]
    pub fn with_max_clock_drift_ms(mut self, max_clock_drift_ms: u64) -> HlcClock {
        self.max_clock_drift_ms = max_clock_drift_ms;
        self
    }
}

impl HlcClock {
    /// Shared advance logic for [`Clock::observe`] and [`Clock::observe_trusted`].
    ///
    /// `effective_remote_wall` is the wall value to use for the remote stamp — callers that
    /// apply the far-future clamp pass the clamped value; the trusted path passes the raw value.
    /// `remote_counter` is the counter from the original `remote` stamp (unchanged by any clamp).
    ///
    /// Preserves strict monotonicity and counter semantics exactly as the original `observe`
    /// implementation did in its unclamped branches.
    fn advance_inner(
        &self,
        last: &mut Timestamp,
        pt: u64,
        effective_remote_wall: u64,
        remote_counter: u32,
    ) {
        let max_wall = pt.max(last.wall_ms).max(effective_remote_wall);

        // Pick the base counter from the dominant wall bucket, then advance one logical tick.
        // advance() handles u32::MAX → (wall+1, 0) so the result can never wrap.
        let base_counter = if max_wall == last.wall_ms && max_wall == effective_remote_wall {
            // Both last and the (clamped) remote share max_wall: take the larger counter.
            last.counter.max(remote_counter)
        } else if max_wall == last.wall_ms {
            last.counter
        } else if max_wall == effective_remote_wall {
            remote_counter
        } else {
            // Physical time leapt past both: fresh wall, counter starts at 0.
            // We return early here rather than running through advance() to preserve the
            // original semantics (counter = 0, not 1) for the physical-time-dominates case.
            *last = Timestamp {
                wall_ms: max_wall,
                counter: 0,
                node_id: self.node_id,
            };
            return;
        };

        let (new_wall, new_counter) = advance(max_wall, base_counter);
        *last = Timestamp {
            wall_ms: new_wall,
            counter: new_counter,
            node_id: self.node_id,
        };
    }
}

impl Clock for HlcClock {
    /// Mint a fresh timestamp for a local event (a write or an outgoing message).
    ///
    /// The returned timestamp is strictly greater than every timestamp previously produced
    /// or observed by this clock, ensuring local monotonicity.
    fn now(&self) -> Timestamp {
        let pt = phys_now_ms();
        let mut last = self.last.lock();
        let next = if pt > last.wall_ms {
            Timestamp {
                wall_ms: pt,
                counter: 0,
                node_id: self.node_id,
            }
        } else {
            // Physical time has not advanced past the last stored wall; bump the counter.
            // advance() handles the u32::MAX → (wall+1, 0) rollover so we cannot wrap.
            let (wall_ms, counter) = advance(last.wall_ms, last.counter);
            Timestamp {
                wall_ms,
                counter,
                node_id: self.node_id,
            }
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
    /// configured `max_clock_drift_ms` (default [`DEFAULT_MAX_CLOCK_DRIFT_MS`]), it is treated as
    /// though it arrived at `phys_now + max_clock_drift_ms`. A `warn!` is emitted so operators can
    /// detect misbehaving or compromised peers. The remote's own `Timestamp` is left untouched
    /// for LWW purposes; only the local clock state is protected.
    fn observe(&self, remote: Timestamp) {
        let pt = phys_now_ms();
        let mut last = self.last.lock();

        // Clamp remote.wall_ms so a buggy or malicious peer cannot pin the local clock
        // arbitrarily far into the future (see DEFAULT_MAX_CLOCK_DRIFT_MS for the full rationale).
        let cap = pt.saturating_add(self.max_clock_drift_ms);
        let effective_remote_wall = if remote.wall_ms > cap {
            warn!(
                remote_wall_ms = remote.wall_ms,
                remote_node_id = remote.node_id,
                phys_now_ms = pt,
                cap_ms = cap,
                max_clock_drift_ms = self.max_clock_drift_ms,
                "remote timestamp leads local clock by more than the configured max drift; \
                 clamping to cap to protect local clock state"
            );
            cap
        } else {
            remote.wall_ms
        };

        self.advance_inner(&mut last, pt, effective_remote_wall, remote.counter);
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
        self.advance_inner(&mut last, pt, remote.wall_ms, remote.counter);
    }
}

/// A value that carries a [`Timestamp`].
///
/// Lets the reconciliation engine read the timestamp of a stored value (to advance its
/// clock on receipt) without knowing the concrete value type.
pub trait Timestamped {
    /// The Hybrid Logical Clock timestamp attached to this value.
    fn timestamp(&self) -> Timestamp;
}

impl<V> Timestamped for (Timestamp, V) {
    fn timestamp(&self) -> Timestamp {
        self.0
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
        // Force the clock a little into the future (within DEFAULT_MAX_CLOCK_DRIFT_MS) so physical
        // time cannot advance past it for the duration of the test: every `now()` must then
        // bump the counter. We no longer use u64::MAX here because the far-future clamp
        // (see observe()) correctly rejects values beyond phys_now + DEFAULT_MAX_CLOCK_DRIFT_MS.
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
        // *after* it, not lost. (Far-future stamps beyond DEFAULT_MAX_CLOCK_DRIFT_MS are clamped;
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
    fn total_order_breaks_ties_on_node_id() {
        // Equal wall and counter: the node_id decides, deterministically and identically on
        // every replica.
        let a = Timestamp::new(100, 0, 1);
        let b = Timestamp::new(100, 0, 2);
        assert!(a < b);
        assert!(b > a);
        // And it is consistent with the field priority: wall dominates counter dominates id.
        assert!(Timestamp::new(100, 1, 1) > Timestamp::new(100, 0, 2));
        assert!(Timestamp::new(101, 0, 1) > Timestamp::new(100, 9, 9));
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
    /// The next `now()` must be within phys_now + DEFAULT_MAX_CLOCK_DRIFT_MS + small margin,
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
        let upper_bound = pt + DEFAULT_MAX_CLOCK_DRIFT_MS + 10;
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
        let upper_bound = pt + DEFAULT_MAX_CLOCK_DRIFT_MS + 10;
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

    /// Verify that `advance()` itself never wraps the counter: at u32::MAX it rolls wall forward.
    #[test]
    fn advance_never_wraps_counter() {
        let (w, c) = advance(1000, u32::MAX);
        assert_eq!(w, 1001, "wall should roll to 1001");
        assert_eq!(c, 0, "counter should reset to 0 after roll");

        // Non-overflow case: straightforward increment.
        let (w2, c2) = advance(1000, 0);
        assert_eq!(w2, 1000);
        assert_eq!(c2, 1);

        // Saturating wall: u64::MAX + 1 must not wrap.
        let (w3, c3) = advance(u64::MAX, u32::MAX);
        assert_eq!(w3, u64::MAX, "wall saturates at u64::MAX");
        assert_eq!(c3, 0);
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
