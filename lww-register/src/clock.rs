// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Hybrid Logical Clock timestamps, ordering half: `ARCHITECTURE.md` §4.
//!
//! Kulkarni et al. 2014's HLC *is* the pair `(physical, logical)` — [`Hlc`], and all the
//! arithmetic touches. [`NodeId`] is not a clock component but the tie-break that makes the LWW
//! comparison a total order, attached where a reading is minted into a [`Timestamp`] (Preguiça,
//! Baquero & Shapiro, arXiv:1805.06358, on the LWW-Register). Reads no wall clock: the single
//! physical-time read lives in the `HlcClock` adapter behind the [`Clock`] port.
//!
//! [`AdmittedTime`] is the load-bearing type: [`Hlc::advance_past_remote`] accepts nothing else,
//! so the far-future clamp of [`MAX_CLOCK_DRIFT`] cannot be skipped by accident. It guards the
//! *local clock state* only — a stored remote stamp keeps its original value as LWW data, and the
//! one consumer deriving an instant from a stored stamp re-admits it (`reconcile::clock`'s
//! `BoundedInstant`, `ARCHITECTURE.md` §5 invariant 6).

use serde::{Deserialize, Serialize};

/// A **duration** in milliseconds: how far a clock reading may lead another before it is suspect.
///
/// Not [`PhysicalTime`], which is an **instant**: a budget is never comparable to an instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClockDrift(u64);

impl ClockDrift {
    /// A drift budget of `ms` milliseconds.
    pub const fn from_millis(ms: u64) -> ClockDrift {
        ClockDrift(ms)
    }

    /// The budget, in milliseconds.
    pub const fn millis(self) -> u64 {
        self.0
    }
}

/// Default maximum a remote clock may lead physical time before its [`PhysicalTime`] is clamped.
///
/// One hour: orders of magnitude above any NTP-plausible skew, still finite. Without a cap, one
/// unauthenticated packet stamped near `u64::MAX` pins every node's clock there permanently.
/// Overridable per clock (`HlcClock::with_max_clock_drift`).
///
/// The clamp limits how far a remote stamp advances the *local clock state*; it never rewrites an
/// already-minted stamp, and the remote's own [`Timestamp`] still competes in LWW unchanged.
pub const MAX_CLOCK_DRIFT: ClockDrift = ClockDrift::from_millis(3_600_000); // 1 hour

/// The **physical time** of a [`Timestamp`]: an instant, in milliseconds since the Unix epoch.
///
/// Arithmetic on it is narrow and saturating, so no call site reasons about wrapping.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct PhysicalTime(u64);

impl PhysicalTime {
    /// The Unix epoch itself — the instant a cold clock starts from.
    pub const EPOCH: PhysicalTime = PhysicalTime(0);

    /// An instant `ms` milliseconds after the Unix epoch.
    pub const fn from_millis(ms: u64) -> PhysicalTime {
        PhysicalTime(ms)
    }

    /// This instant, in milliseconds since the Unix epoch.
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// This instant moved forward by `drift`, saturating: the budget may be arbitrarily large.
    #[must_use]
    pub const fn saturating_add(self, drift: ClockDrift) -> PhysicalTime {
        PhysicalTime(self.0.saturating_add(drift.0))
    }

    /// The next millisecond, saturating at `u64::MAX`.
    #[must_use]
    pub const fn next_ms(self) -> PhysicalTime {
        PhysicalTime(self.0.saturating_add(1))
    }
}

/// The **logical counter** of a [`Timestamp`]: disambiguates events in one [`PhysicalTime`].
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct LogicalCounter(u32);

impl LogicalCounter {
    /// The counter a fresh physical-time bucket starts at.
    pub const ZERO: LogicalCounter = LogicalCounter(0);

    /// A counter holding `value`.
    pub const fn new(value: u32) -> LogicalCounter {
        LogicalCounter(value)
    }

    /// The raw counter value.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The next counter, or `None` on `u32::MAX` — the signal for [`Hlc::next_tick`]'s
    /// physical-time roll. Crate-private so no caller can rebuild that rule.
    #[must_use]
    pub(crate) const fn checked_next(self) -> Option<LogicalCounter> {
        match self.0.checked_add(1) {
            Some(c) => Some(LogicalCounter(c)),
            None => None,
        }
    }
}

/// A replica's identity: the deterministic tie-break that makes the conflict order total.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct NodeId(u64);

impl NodeId {
    /// The replica identified by `id`.
    pub const fn new(id: u64) -> NodeId {
        NodeId(id)
    }

    /// The raw identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A remote physical-time reading **admitted** to the local clock state: evidence the
/// [`MAX_CLOCK_DRIFT`] check has run.
///
/// Obtainable only via [`clamped_to_drift`](AdmittedTime::clamped_to_drift) or
/// [`trusted`](AdmittedTime::trusted) — no public field, no `Default`, no `From<PhysicalTime>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdmittedTime {
    physical: PhysicalTime,
    clamped: bool,
}

impl AdmittedTime {
    /// Admit an **untrusted** reading, clamping it to `local_now + max_drift`: the
    /// [`Clock::observe`] path. The remote's own [`Timestamp`] is untouched.
    ///
    /// [`was_clamped`](AdmittedTime::was_clamped) reports whether the cap fired.
    pub fn clamped_to_drift(
        remote: PhysicalTime,
        local_now: PhysicalTime,
        max_drift: ClockDrift,
    ) -> AdmittedTime {
        let cap = local_now.saturating_add(max_drift);
        if remote > cap {
            AdmittedTime {
                physical: cap,
                clamped: true,
            }
        } else {
            AdmittedTime {
                physical: remote,
                clamped: false,
            }
        }
    }

    /// Admit a reading **without** the clamp, on the caller's word.
    ///
    /// Correct for exactly one case: a stamp this node itself authored ([`Clock::observe_trusted`]).
    /// Anything off the network reopens the clock-poisoning hole.
    pub fn trusted(physical: PhysicalTime) -> AdmittedTime {
        AdmittedTime {
            physical,
            clamped: false,
        }
    }

    /// The admitted instant: the cap if the clamp fired, the original reading otherwise.
    pub fn physical(self) -> PhysicalTime {
        self.physical
    }

    /// Whether [`clamped_to_drift`](AdmittedTime::clamped_to_drift) actually replaced the
    /// reading with the cap. Always `false` for [`trusted`](AdmittedTime::trusted).
    pub fn was_clamped(self) -> bool {
        self.clamped
    }
}

/// A **Hybrid Logical Clock reading**: the pair `(physical, logical)` of Kulkarni et al. 2014.
///
/// Field declaration order *is* the first two thirds of the conflict order (`ARCHITECTURE.md` §5
/// invariant 2); no identity takes part in the arithmetic.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct Hlc {
    /// Physical time: the instant last observed by the clock.
    physical: PhysicalTime,
    /// Logical counter: disambiguates events sharing the same `physical`.
    logical: LogicalCounter,
}

impl Hlc {
    /// The reading a cold clock starts from: the Unix epoch, counter zero.
    pub const START: Hlc = Hlc {
        physical: PhysicalTime::EPOCH,
        logical: LogicalCounter::ZERO,
    };

    /// Build a reading from its two components.
    pub const fn new(physical: PhysicalTime, logical: LogicalCounter) -> Hlc {
        Hlc { physical, logical }
    }

    /// The physical time this reading sits in.
    pub const fn physical(&self) -> PhysicalTime {
        self.physical
    }

    /// The logical counter within that millisecond.
    pub const fn logical(&self) -> LogicalCounter {
        self.logical
    }

    /// This reading advanced by one logical tick, rolling `physical` forward on counter overflow.
    ///
    /// Strictly monotonic: `(physical + 1, 0) > (physical, u32::MAX)`, and saturating, so
    /// `u64::MAX` cannot wrap. Sole owner of the roll rule.
    #[must_use]
    pub fn next_tick(self) -> Hlc {
        match self.logical.checked_next() {
            Some(logical) => Hlc {
                physical: self.physical,
                logical,
            },
            None => Hlc {
                physical: self.physical.next_ms(),
                logical: LogicalCounter::ZERO,
            },
        }
    }

    /// The HLC "observe" arithmetic: move `self` strictly past both `phys_now` and a remote
    /// reading, ending at or above `max(self, phys_now, admitted remote)`.
    ///
    /// `remote_logical` comes from the original remote stamp, never affected by a clamp.
    pub fn advance_past_remote(
        &mut self,
        phys_now: PhysicalTime,
        remote_physical: AdmittedTime,
        remote_logical: LogicalCounter,
    ) {
        let remote_physical = remote_physical.physical();
        let max_physical = phys_now.max(self.physical).max(remote_physical);

        let base_logical = if max_physical == self.physical && max_physical == remote_physical {
            self.logical.max(remote_logical)
        } else if max_physical == self.physical {
            self.logical
        } else if max_physical == remote_physical {
            remote_logical
        } else {
            *self = Hlc {
                physical: max_physical,
                logical: LogicalCounter::ZERO,
            };
            return;
        };

        *self = Hlc {
            physical: max_physical,
            logical: base_logical,
        }
        .next_tick();
    }
}

/// A Hybrid Logical Clock timestamp: the **LWW ordering key**, `(Hlc, NodeId)`.
///
/// Field declaration order *is* the conflict order `(physical, logical, node_id)`
/// (`ARCHITECTURE.md` §5 invariant 2). Neither the newtypes nor the nesting costs anything on the
/// wire, pinned by `tests/timestamp_wire_format.rs`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct Timestamp {
    /// The clock reading: what time it was, as this node's clock sees time.
    hlc: Hlc,
    /// Identity of the node that minted this timestamp; provides the deterministic tie-break.
    node_id: NodeId,
}

impl Timestamp {
    /// Pair a clock reading with the identity of the node minting it.
    ///
    /// For tests and for reconstruction from external storage; normal code goes through the
    /// store's clock.
    pub const fn new(hlc: Hlc, node_id: NodeId) -> Timestamp {
        Timestamp { hlc, node_id }
    }

    /// The clock reading this stamp carries.
    pub const fn hlc(&self) -> Hlc {
        self.hlc
    }

    /// The physical time this stamp sits in — [`hlc`](Timestamp::hlc)'s, delegated.
    pub const fn physical(&self) -> PhysicalTime {
        self.hlc.physical()
    }

    /// The logical counter within that millisecond — [`hlc`](Timestamp::hlc)'s, delegated.
    pub const fn logical(&self) -> LogicalCounter {
        self.hlc.logical()
    }

    /// Identity of the node that minted this timestamp.
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }
}

/// The domain's **clock port** (`ARCHITECTURE.md` §3.2): the adapter behind it performs the single
/// physical-time read and owns this node's [`NodeId`].
///
/// Concrete [`Timestamp`] rather than an associated type, so the port stays object-safe and no
/// clock parameter leaks into the engine.
pub trait Clock: Send + Sync + 'static {
    /// Mint a strictly-monotonic local timestamp for a write or an outgoing message.
    fn now(&self) -> Timestamp;
    /// This node's identity, as stamped onto every timestamp minted. Costs no counter tick.
    fn node_id(&self) -> NodeId;
    /// Advance past a peer's timestamp, so a subsequent [`now`](Clock::now) is ordered after it.
    ///
    /// Holds for stamps within a bounded lead: an implementation may clamp beyond
    /// [`MAX_CLOCK_DRIFT`] via [`AdmittedTime::clamped_to_drift`]. The [`Timestamp`] order and the
    /// strict-`>` merge are unaffected.
    fn observe(&self, remote: Timestamp);
    /// Advance past a stamp **this node itself authored**, so the first post-restart
    /// [`now`](Clock::now) outranks every pre-restart write.
    ///
    /// Implementations must **not** clamp here — the one caller entitled to
    /// [`AdmittedTime::trusted`] — or a backward clock step re-introduces own-write shadowing.
    /// The default delegates to [`observe`](Clock::observe), safe only for clamp-free adapters.
    fn observe_trusted(&self, remote: Timestamp) {
        self.observe(remote);
    }
}

#[cfg(test)]
mod tests {
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

    /// Out-of-budget readings come back capped and flagged, in-budget ones untouched.
    #[test]
    fn clamped_to_drift_caps_a_far_future_reading() {
        let now = PhysicalTime::from_millis(1_000);
        let budget = ClockDrift::from_millis(500);

        let admitted =
            AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(u64::MAX), now, budget);
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
}
