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
//!
//! [`Clock`] is a public, injectable port (`reconcile::Replica::new_with_clock`,
//! `reconcile::ReplicatedMap::new_with_clock`): nothing about it is enforced by the type
//! system beyond the method signatures, so a monotonicity bug in a third-party adapter compiles
//! clean and fails only at runtime, silently, as writes that never converge. [`assert_conformance`]
//! is the gate — run it over any [`Clock`] before trusting it, including [`Clock::observe_trusted`],
//! which this trait now requires every implementor to state explicitly (no default body: see its
//! docs for why a default is unsound).
//!
//! Split across siblings by concern: `primitives` owns the plain-newtype impls
//! ([`ClockDrift`]/[`PhysicalTime`]/[`LogicalCounter`]/[`NodeId`] construction and accessors);
//! `admitted`/`hlc`/`timestamp` each own one `impl` group (drift-clamped admission, the HLC
//! advance/tick arithmetic, and the `(Hlc, NodeId)` pairing). This file keeps the public type
//! definitions (their module location is their `cargo public-api`-visible path — see AGENTS.md
//! §11), the [`Clock`] port trait, and [`assert_conformance`] — none of which can move to a
//! submodule without changing a public path or, for the trait, its own definition site.

use serde::{Deserialize, Serialize};

mod admitted;
mod hlc;
mod primitives;
mod timestamp;

/// A **duration** in milliseconds: how far a clock reading may lead another before it is suspect.
///
/// Not [`PhysicalTime`], which is an **instant**: a budget is never comparable to an instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClockDrift(u64);

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

/// The **logical counter** of a [`Timestamp`]: disambiguates events in one [`PhysicalTime`].
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct LogicalCounter(u32);

/// A replica's identity: the deterministic tie-break that makes the conflict order total.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct NodeId(u64);

/// A remote physical-time reading **admitted** to the local clock state: evidence the
/// [`MAX_CLOCK_DRIFT`] check has run.
///
/// Obtainable only via [`clamped_to_drift`](AdmittedTime::clamped_to_drift) or
/// [`trusted`](AdmittedTime::trusted) — no public field, no `Default`, no `From<PhysicalTime>`.
///
/// ```
/// use lww_register::clock::{AdmittedTime, ClockDrift, PhysicalTime};
///
/// let local_now = PhysicalTime::from_millis(1_000);
/// let budget = ClockDrift::from_millis(500);
///
/// // A remote reading within the drift budget is admitted unchanged.
/// let in_budget = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(1_200), local_now, budget);
/// assert_eq!(in_budget.physical(), PhysicalTime::from_millis(1_200));
/// assert!(!in_budget.was_clamped());
///
/// // A hostile reading far beyond the budget is capped at `local_now + budget`, not admitted as-is.
/// let hostile = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(u64::MAX), local_now, budget);
/// assert_eq!(hostile.physical(), PhysicalTime::from_millis(1_500));
/// assert!(hostile.was_clamped());
///
/// // A stamp this node authored itself skips the clamp entirely.
/// let own = AdmittedTime::trusted(PhysicalTime::from_millis(u64::MAX));
/// assert_eq!(own.physical(), PhysicalTime::from_millis(u64::MAX));
/// assert!(!own.was_clamped());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdmittedTime {
    physical: PhysicalTime,
    clamped: bool,
}

/// A **Hybrid Logical Clock reading**: the pair `(physical, logical)` of Kulkarni et al. 2014.
///
/// Field declaration order *is* the first two thirds of the conflict order (`ARCHITECTURE.md` §5
/// invariant 2); no identity takes part in the arithmetic.
///
/// ```
/// use lww_register::clock::{AdmittedTime, Hlc, LogicalCounter, PhysicalTime};
///
/// // A local tick with no remote input just advances the logical counter.
/// let mut clock = Hlc::START;
/// clock = clock.next_tick();
/// assert_eq!(clock, Hlc::new(PhysicalTime::EPOCH, LogicalCounter::new(1)));
///
/// // A remote reading strictly ahead of local physical time pulls the clock forward to it,
/// // then ticks once more -- the result is strictly greater than both inputs.
/// let local_phys = PhysicalTime::from_millis(10);
/// let remote = Hlc::new(PhysicalTime::from_millis(500), LogicalCounter::new(3));
/// clock.advance_past_remote(
///     local_phys,
///     AdmittedTime::trusted(remote.physical()),
///     remote.logical(),
/// );
/// assert!(clock > remote);
/// assert!(clock.physical() >= remote.physical());
/// ```
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct Hlc {
    /// Physical time: the instant last observed by the clock.
    physical: PhysicalTime,
    /// Logical counter: disambiguates events sharing the same `physical`.
    logical: LogicalCounter,
}

/// A Hybrid Logical Clock timestamp: the **LWW ordering key**, `(Hlc, NodeId)`.
///
/// Field declaration order *is* the conflict order `(physical, logical, node_id)`
/// (`ARCHITECTURE.md` §5 invariant 2). Neither the newtypes nor the nesting costs anything on the
/// wire, pinned by `tests/timestamp_wire_format.rs`.
///
/// ```
/// use lww_register::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
///
/// let hlc_at = |ms| Hlc::new(PhysicalTime::from_millis(ms), LogicalCounter::new(0));
///
/// // Later physical time always wins, whatever the node identities.
/// let earlier = Timestamp::new(hlc_at(100), NodeId::new(9));
/// let later = Timestamp::new(hlc_at(200), NodeId::new(1));
/// assert!(later > earlier);
///
/// // A tie on (physical, logical) breaks on node_id -- the deterministic tie-break every replica
/// // computes identically, with no coordination.
/// let same_time_low_node = Timestamp::new(hlc_at(100), NodeId::new(1));
/// let same_time_high_node = Timestamp::new(hlc_at(100), NodeId::new(9));
/// assert!(same_time_high_node > same_time_low_node);
/// ```
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct Timestamp {
    /// The clock reading: what time it was, as this node's clock sees time.
    hlc: Hlc,
    /// Identity of the node that minted this timestamp; provides the deterministic tie-break.
    node_id: NodeId,
}

/// The domain's **clock port** (`ARCHITECTURE.md` §3.2): the adapter behind it performs the single
/// physical-time read and owns this node's [`NodeId`].
///
/// Concrete [`Timestamp`] rather than an associated type, so the port stays object-safe and no
/// clock parameter leaks into the engine.
///
/// A minimal, correct implementor -- verified against [`assert_conformance`], the check every
/// real adapter should run before it is trusted in production:
///
/// ```
/// use std::sync::Mutex;
/// use lww_register::clock::{assert_conformance, AdmittedTime, Clock, Hlc, NodeId, PhysicalTime, Timestamp};
///
/// struct MyClock {
///     node_id: NodeId,
///     last: Mutex<Hlc>,
/// }
///
/// impl Clock for MyClock {
///     fn node_id(&self) -> NodeId {
///         self.node_id
///     }
///
///     fn now(&self) -> Timestamp {
///         let mut last = self.last.lock().unwrap();
///         *last = last.next_tick();
///         Timestamp::new(*last, self.node_id)
///     }
///
///     fn observe(&self, remote: Timestamp) {
///         let mut last = self.last.lock().unwrap();
///         last.advance_past_remote(
///             PhysicalTime::EPOCH,
///             AdmittedTime::trusted(remote.physical()),
///             remote.logical(),
///         );
///     }
///
///     fn observe_trusted(&self, remote: Timestamp) {
///         // No clamp: the one caller entitled to trust a stamp this node itself authored.
///         self.observe(remote);
///     }
/// }
///
/// let clock = MyClock {
///     node_id: NodeId::new(1),
///     last: Mutex::new(Hlc::START),
/// };
/// assert_conformance(&clock); // panics with a diagnostic if the contract is violated
/// ```
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
    /// [`AdmittedTime::trusted`] — or a backward clock step re-introduces own-write shadowing. No
    /// default body: delegating to [`observe`](Clock::observe) is only sound for a clamp-free
    /// adapter, and a default silently makes that the fallback for every adapter that clamps,
    /// including one written after this trait. Stating the clamp policy explicitly, every time, is
    /// the point. [`assert_conformance`] checks it holds.
    fn observe_trusted(&self, remote: Timestamp);
}

/// Assert that `clock` upholds the [`Clock`] contract [`Entry::merge`](crate::entry::Entry::merge)'s
/// strict `>` and the tombstone garbage collector depend on. Call this from an implementor's own
/// test suite before trusting a [`Clock`] adapter in production — nothing in this crate can call it
/// for you, since it has no way to know your adapter exists.
///
/// # What a violation costs
///
/// [`Clock`] is `pub`, `now`/`observe`/`observe_trusted` are the only seam the domain reads
/// physical time through, and nothing here stops a caller from handing `Replica`/`ReplicatedMap` a
/// clock that is not actually monotonic. That is not a hypothetical: the naive implementation —
/// read the wall clock, stamp `logical = 0` — type-checks, compiles, and passes review by
/// inspection. It still breaks correctness, silently, because every place a [`Timestamp`] is
/// compared assumes strict monotonicity holds:
///
/// - **`now()` not monotonic.** Two calls to `now()` returning an equal or decreasing reading let
///   two local writes to the same key race to an equal `(physical, logical, node_id)`.
///   `Entry::merge`'s strict `>` (`ARCHITECTURE.md` §5 invariant 2) then keeps *each side's* value
///   depending on merge order — the two replicas never agree which write won, so the fingerprint
///   never matches and the anti-entropy round re-exchanges the same key forever.
/// - **`observe(t)` not chased by a later `now() > t`.** A remote write can then be shadowed by a
///   local one carrying an *earlier* effective order, even though the remote write causally
///   happened first from this node's point of view — the causal edge HLC exists to preserve is
///   lost.
/// - **`observe_trusted` clamping.** A backward wall-clock step across a restart (NTP correction,
///   VM pause, manual clock set) leaves the post-restart clock below this node's own
///   already-persisted stamps; `observe_trusted` restores it, but only if it is never clamped. A
///   clamped `observe_trusted` re-admits own-write shadowing — the first post-restart write can be
///   silently discarded by the pre-restart one still on disk.
///
/// None of these failure modes panic, error, or log by default: they surface as writes that
/// mysteriously do not stick, or as a cluster that never converges. `assert_conformance` cannot
/// prove an adapter correct in general (that would require modeling every possible wall-clock and
/// scheduler interleaving), but it does what a type system cannot: it drives the specific
/// interleavings above and panics with a diagnostic the moment one is violated, rather than letting
/// a faulty adapter ship silently.
///
/// # What is checked
///
/// - [`now`](Clock::now) is strictly monotonic across a burst of calls with no observation
///   in between.
/// - After [`observe`](Clock::observe) of a timestamp with a modest (in-budget) lead over the
///   clock's own reading, the next [`now`](Clock::now) is strictly greater than it.
/// - After [`observe_trusted`](Clock::observe_trusted) of a timestamp far beyond
///   [`MAX_CLOCK_DRIFT`], the next [`now`](Clock::now) is still strictly greater than it —
///   `observe_trusted` must never clamp.
///
/// # Panics
///
/// Panics with a diagnostic message identifying which invariant failed and the two readings that
/// violated it.
pub fn assert_conformance<C: Clock>(clock: &C) {
    // 1. `now()` alone must be strictly monotonic.
    let mut prev = clock.now();
    for _ in 0..1_000 {
        let next = clock.now();
        assert!(
            next > prev,
            "Clock::now() must be strictly monotonic: {next:?} is not > {prev:?}"
        );
        prev = next;
    }

    // 2. A modest, in-budget lead must be chased by `observe`, not ignored.
    let modest_future = Timestamp::new(
        Hlc::new(
            prev.physical()
                .saturating_add(ClockDrift::from_millis(1_000)),
            LogicalCounter::ZERO,
        ),
        NodeId::new(clock.node_id().get().wrapping_add(1)),
    );
    clock.observe(modest_future);
    let after_observe = clock.now();
    assert!(
        after_observe > modest_future,
        "Clock::observe(t) must be followed by a now() > t for an in-budget t: {after_observe:?} \
         is not > {modest_future:?}"
    );

    // 3. `observe_trusted` must never clamp, even for a stamp far beyond `MAX_CLOCK_DRIFT`.
    let far_future = Timestamp::new(
        Hlc::new(
            after_observe
                .physical()
                .saturating_add(MAX_CLOCK_DRIFT)
                .saturating_add(ClockDrift::from_millis(1)),
            LogicalCounter::ZERO,
        ),
        clock.node_id(),
    );
    clock.observe_trusted(far_future);
    let after_trusted = clock.now();
    assert!(
        after_trusted > far_future,
        "Clock::observe_trusted(t) must never clamp: now() must be > t even for t far beyond \
         MAX_CLOCK_DRIFT, or a backward wall-clock step across a restart can shadow this node's \
         own pre-restart writes: {after_trusted:?} is not > {far_future:?}"
    );
}

#[cfg(test)]
mod tests;
