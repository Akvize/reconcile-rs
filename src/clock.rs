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
//! `HlcClock` owns the crate's primary wall-clock read and the node's [`NodeId`], which it
//! attaches when minting a reading into a [`Timestamp`]. Every other wall-clock read in `reconcile`
//! lives in this file too, alongside it: `BoundedInstant` needs a physical-time read and `chrono`
//! for the tombstone-expiry instant derived from a *stored* stamp, bounded by the same
//! [`MAX_CLOCK_DRIFT`] budget; `wall_clock_now` is the plain `DateTime<Utc>` reading a caller
//! like `TimeoutWheel::expired` needs for an instant of its own, supplied instead of read locally so
//! the wheel itself never touches the wall clock.
//!
//! `HlcClock` is the default [`Clock`] adapter, not the only one that can be plugged in:
//! [`ReplicatedMap::new_with_clock`](crate::ReplicatedMap::new_with_clock) accepts any `Arc<dyn
//! Clock>`. [`assert_conformance`] is what an implementor runs before trusting a
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

/// Read the current wall-clock instant for a caller that needs a `DateTime<Utc>` rather than the
/// [`PhysicalTime`] scale — e.g. `TimeoutWheel::expired`, which takes `now` as a parameter instead
/// of reading the clock itself so the wheel stays adapter-free. Keeps that read here, alongside
/// [`phys_now`], rather than scattered across the facade.
pub(crate) fn wall_clock_now() -> DateTime<Utc> {
    Utc::now()
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

    /// Override the clamp threshold (default [`MAX_CLOCK_DRIFT`]). Wired from
    /// [`Config::max_clock_drift`](crate::replicated_map::Config::max_clock_drift) at
    /// construction (`Replica::new`/`with_transport`) whenever the default [`HlcClock`] adapter is
    /// in use; a caller-supplied [`Clock`] (`new_with_clock`) is responsible for its own drift
    /// policy, if any.
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
mod tests;
