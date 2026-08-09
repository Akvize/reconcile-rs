// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Hybrid Logical Clock (HLC) timestamps: the ordering half.
//!
//! This module holds the *domain* half of the clock: the [`Hlc`] clock reading and the
//! [`Timestamp`] ordering key built from it, the newtypes they are made of ([`PhysicalTime`],
//! [`LogicalCounter`], [`NodeId`]), the [`Clock`] port through which the register reads time, and
//! the HLC ordering arithmetic ([`Hlc::next_tick`]/[`Hlc::advance_past_remote`]) that keeps stamps
//! strictly monotonic. It reads no wall clock — the single physical-time read lives in the
//! `HlcClock` adapter, which sits in the `reconcile` crate behind this port.
//!
//! # The reading and the ordering key are two different things
//!
//! An HLC in the sense of Kulkarni et al. (2014) is the **pair** `(physical, logical)`, and that
//! pair is all the clock arithmetic ever touches — [`Hlc`] is exactly that type. The `node_id` is
//! not part of a clock reading at all: it is the deterministic **tie-break** that turns the LWW
//! comparison into a total order, and it is attached where a reading is minted into a stamp.
//! [`Timestamp`] is that composite, `(Hlc, NodeId)` — the LWW ordering key.
//!
//! This is the construction N. Preguiça, C. Baquero and M. Shapiro describe in *Conflict-free
//! Replicated Data Types* (arXiv:1805.06358), in their account of the LWW-Register:
//!
//! > When combining the clock time with a site identifier, we have unique timestamps that are
//! > totally ordered.
//!
//! Mapping their terms onto these types: *clock time* is [`Hlc`], *site identifier* is [`NodeId`],
//! and the pair is [`Timestamp`] — which is why the composite keeps that name. The paper describes
//! the construction rather than prescribing this factoring, but the sentence names exactly the two
//! properties an LWW-Register needs of its stamps, and both come from the combination rather than
//! from the clock: **total order** (a clock reading alone leaves concurrent writes incomparable)
//! and **uniqueness** (it is the site identifier that keeps two writes in the same millisecond
//! distinguishable). That is also the sharpest statement of why `NodeId` belongs in the ordering
//! key and nowhere in the clock arithmetic.
//!
//! One vocabulary note: *site identifier* is a one-off phrasing in that sentence — the paper's
//! standing word for the entity is **replica**. This crate says `NodeId` because `Replica` is
//! already a type in this workspace (the reconciliation engine), and the collision would cost more
//! than the imprecision.
//!
//! Keeping them apart is not decoration. While they were one flat triple, both arithmetic entry
//! points took a [`NodeId`] they never read: it appeared only in the value they constructed, never
//! in a comparison or a computation, so an identity had to be threaded through the clock rule to
//! come out the other end unchanged. Nor could the identity simply be inferred from the receiver —
//! a tick minted by node A while advancing past B's stamp must carry **A**'s id. It legitimately
//! comes from outside, which is precisely why it belongs at the minting site (the `Clock` adapter,
//! which owns the node's identity) rather than in the arithmetic.
//!
//! Conflict resolution in the replicated map is last-write-wins (LWW). Keying LWW on a raw physical
//! wall-clock (`DateTime<Utc>`) is unsafe:
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
//! * pairs that reading with a [`NodeId`], giving a **globally deterministic total order**
//!   `(physical, logical, node_id)`. Every replica therefore picks the *same* survivor on a
//!   conflict, which makes the merge commutative, associative and idempotent — i.e. genuine
//!   Strong Eventual Consistency.
//!
//! LWW still discards one of two *genuinely concurrent* writes by design; recovering both
//! would require version vectors or a CRDT and is out of scope.
//!
//! # What the types enforce, and what is left to the caller
//!
//! Every component here is a newtype, per `AGENTS.md` §4 — the three components of a `Timestamp`
//! used to be a `u64`, a `u32` and a second `u64`, so transposing `physical` and `node_id` at a call
//! site compiled and silently changed LWW ordering. They are now distinct types, and so is the
//! *duration* [`ClockDrift`] the far-future check is expressed in: a drift budget can no longer be
//! compared against, or mistaken for, an instant.
//!
//! The load-bearing one is [`AdmittedTime`]. The far-future clamp (see [`MAX_CLOCK_DRIFT`]) is a
//! security property: without it a single forged datagram stamped near `u64::MAX` pins every
//! node's clock into the far future forever. It used to be carried by a parameter *name* — an
//! `effective_remote_time: u64` documented as "the clamped value on the adapter path, the raw value
//! on the trusted path" — which no compiler checks. [`Hlc::advance_past_remote`] now takes an
//! `AdmittedTime`, a value obtainable in exactly two ways:
//!
//! * [`AdmittedTime::clamped_to_drift`], which *performs* the clamp — the untrusted path; or
//! * [`AdmittedTime::trusted`], the explicitly-named escape hatch a caller must spell out to say
//!   it is vouching for the value (only correct for a stamp this node itself authored, see
//!   [`Clock::observe_trusted`]).
//!
//! There is no third constructor, no public field and no `From<PhysicalTime>`, so a raw reading
//! lifted off a datagram cannot reach the ordering arithmetic without one of those two words
//! appearing in the source. This is the same "parse, don't validate" shape as `gossip`'s
//! `Payload`, which is only obtainable via `Authenticator::open`.
//!
//! **What the types do not do.** They constrain the *local clock state*; they say nothing about a
//! stamp already stored as LWW data. A clamped remote stamp keeps its original, unclamped
//! `Timestamp` in the map — that is deliberate (the clamp must not rewrite data) — so downstream
//! arithmetic on stored stamps still has to defend itself against oversized values. No type here
//! prevents that; it remains the consumer's responsibility.
//!
//! There is exactly one such consumer, and it now defends itself: the tombstone-expiry conversion
//! in `replicated_map`, which turns a stored stamp into the wall-clock instant the expiry wheel
//! ages by. It runs the stamp's [`PhysicalTime`] back through
//! [`AdmittedTime::clamped_to_drift`] against *local* now, and converts the admitted value with a
//! total `i64::try_from` — so a stamp in the far future can no longer date a tombstone past every
//! plausible expiry (unbounded retention), and one above `i64::MAX` can no longer wrap negative
//! into 1970. The reusable piece lives in `reconcile::clock` as `BoundedInstant`, next to the
//! adapter, because the conversion needs both a physical-time read and a wall-clock type — neither
//! of which belongs here. The stored stamp itself is untouched, exactly as this module requires.

use serde::{Deserialize, Serialize};

/// A **duration** in milliseconds: how far a clock reading may lead another before it is suspect.
///
/// Deliberately *not* the same type as [`PhysicalTime`], which is an **instant**. The far-future
/// check adds a drift budget to an instant to obtain another instant; comparing a budget to an
/// instant is meaningless and is now a compile error rather than a plausible-looking line of code.
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

/// Default maximum a remote clock may lead physical time before its [`PhysicalTime`] is clamped
/// when updating the local clock state.
///
/// This is only the default the `HlcClock` adapter is built with; the threshold is a property of
/// the clock itself, overridable at construction (see `HlcClock::with_max_clock_drift`) rather
/// than a knob on the store's `Config`.
///
/// **Why 1 hour?**
/// NTP-disciplined clocks rarely deviate by more than a few hundred milliseconds in practice;
/// even aggressively skewed or misconfigured peers stay well under a minute ahead. One hour
/// (3 600 000 ms) is therefore orders of magnitude above any legitimate skew while still
/// being finite, giving huge headroom for leap-second smearing, suspended VMs resuming, and
/// other real-world anomalies. In the default unauthenticated mode the gossip socket accepts
/// packets from any sender, so a single malicious or buggy peer can inject arbitrary
/// physical-time values; without a cap, one packet stamped near `u64::MAX` would pin every node's
/// clock to that value permanently, destroying LWW recency. (The clamp protects the local
/// clock state only: a stored value keeps its original stamp as LWW data, so downstream
/// consumers of stored stamps must defend against oversized stamps themselves. The one such
/// consumer, the tombstone expiry arithmetic in `replicated_map`, does: it re-admits the stored
/// [`PhysicalTime`] through [`AdmittedTime::clamped_to_drift`] against local now before deriving
/// an expiry instant from it — see `reconcile::clock`'s `BoundedInstant`.)
///
/// **Clamp semantics (strict monotonicity is preserved):**
/// The clamp is applied only inside the [`Clock::observe`] implementation, by
/// [`AdmittedTime::clamped_to_drift`]; it limits how far a remote stamp may advance *the local
/// clock state* (`last`). It does **not** retroactively alter any timestamp that was already
/// minted: if the local clock was legitimately advanced to some `T` before encountering an
/// out-of-bounds remote, the next `now()` will still return a value `> T`. Put differently, the
/// clamp prevents *future* poisoning; it does not wind the clock back.
///
/// A clamped remote stamp is still valid data in the LWW comparison — the remote's own
/// [`Timestamp`] is returned to the caller unchanged and will win if it is numerically larger
/// than competing local values. The clamp only stops the *local clock* from chasing that
/// value into the far future.
pub const MAX_CLOCK_DRIFT: ClockDrift = ClockDrift::from_millis(3_600_000); // 1 hour

/// The **physical time** of a [`Timestamp`] (Kulkarni et al., 2014): an instant, in milliseconds
/// since the Unix epoch.
///
/// This is the type the adapter's single physical-time read produces. Arithmetic on it is
/// deliberately narrow and saturating — [`saturating_add`] moves an instant forward by a
/// [`ClockDrift`], [`next_ms`] by exactly one millisecond — so no call site has to reason about
/// wrapping.
///
/// [`saturating_add`]: PhysicalTime::saturating_add
/// [`next_ms`]: PhysicalTime::next_ms
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

    /// This instant moved forward by `drift`, saturating at `u64::MAX` rather than wrapping.
    ///
    /// This is how the far-future cap is computed: `phys_now.saturating_add(max_drift)`. Saturation
    /// matters because a caller may configure an arbitrarily large budget.
    #[must_use]
    pub const fn saturating_add(self, drift: ClockDrift) -> PhysicalTime {
        PhysicalTime(self.0.saturating_add(drift.0))
    }

    /// The next millisecond, saturating at `u64::MAX`.
    ///
    /// Used by the HLC counter-overflow fallback ([`Hlc::next_tick`]): rolling physical time
    /// forward one millisecond is what keeps `(physical + 1, 0)` greater than
    /// `(physical, u32::MAX)` without ever wrapping.
    #[must_use]
    pub const fn next_ms(self) -> PhysicalTime {
        PhysicalTime(self.0.saturating_add(1))
    }
}

/// The **logical counter** of a [`Timestamp`] (Kulkarni et al., 2014): disambiguates events
/// sharing the same [`PhysicalTime`].
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

    /// The next counter, or `None` when it would overflow `u32::MAX`.
    ///
    /// `None` is not an error: it is the signal for the HLC's physical-time-roll fallback. It is
    /// deliberately **crate-private**, because a `None` a caller has to interpret is exactly the
    /// counter-roll rule ([`Hlc::next_tick`]) leaking out of the one function that owns it —
    /// an outside caller reaching for it would inevitably re-implement that rule, and get it
    /// wrong. `Option` (mirroring `u32::checked_add`) is the right shape for the internal helper;
    /// the visibility was the mistake.
    #[must_use]
    pub(crate) const fn checked_next(self) -> Option<LogicalCounter> {
        match self.0.checked_add(1) {
            Some(c) => Some(LogicalCounter(c)),
            None => None,
        }
    }
}

/// A replica's identity — the deterministic tie-break that makes the conflict order total.
///
/// A distinct type rather than a bare `u64` so it can never be passed where a physical-time
/// reading is expected.
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

/// A remote physical-time reading that has been **admitted** to the local clock state.
///
/// Its existence is the proof that the far-future check of [`MAX_CLOCK_DRIFT`] was considered:
/// [`Hlc::advance_past_remote`] accepts nothing else, and the only two ways to obtain one are
/// [`clamped_to_drift`](AdmittedTime::clamped_to_drift) (which performs the clamp) and
/// [`trusted`](AdmittedTime::trusted) (which says, in so many words, that the caller vouches for
/// the value). There is no public field, no `Default` and no conversion from [`PhysicalTime`], so
/// a raw reading lifted straight off a datagram cannot reach the ordering arithmetic by accident.
///
/// This is "parse, don't validate" applied to a clock value, the same shape as `gossip`'s
/// `Payload`, which is only obtainable through `Authenticator::open`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdmittedTime {
    physical: PhysicalTime,
    clamped: bool,
}

impl AdmittedTime {
    /// Admit an **untrusted** remote physical-time reading, clamping it to
    /// `local_now + max_drift`.
    ///
    /// This is the constructor the adapter's [`Clock::observe`] path uses. A reading that leads
    /// physical time by more than the budget is replaced by the cap, which is what stops a forged
    /// stamp near `u64::MAX` from pinning the local clock into the far future. The remote's own
    /// [`Timestamp`] is untouched — it stays exactly as received for LWW purposes.
    ///
    /// [`was_clamped`](AdmittedTime::was_clamped) reports whether the cap fired, so the adapter
    /// can log the misbehaving peer.
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

    /// Admit a physical-time reading **without** the far-future clamp, on the caller's word.
    ///
    /// The escape hatch, deliberately named so that the trusted path has to *say* it is trusting
    /// the value. It is correct for exactly one case: a stamp this node itself authored (restored
    /// from its own persisted state — see [`Clock::observe_trusted`]), where refusing to chase our
    /// own past output would re-introduce own-write shadowing after a backward clock step. Reaching
    /// for it on anything that arrived over the network reopens the clock-poisoning hole.
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

/// A **Hybrid Logical Clock reading**: the pair `(physical, logical)` of Kulkarni et al. (2014).
///
/// This is the whole of the clock, and the whole of what the clock *arithmetic* touches — no
/// identity appears anywhere in [`next_tick`](Hlc::next_tick) or
/// [`advance_past_remote`](Hlc::advance_past_remote), because none is needed to compute a reading.
/// Pairing a reading with the node that produced it is [`Timestamp`]'s job.
///
/// The fields are compared in declaration order, so the derived [`Ord`] is `(physical, logical)`:
/// the first two thirds of the conflict order, with `Timestamp`'s `node_id` appended.
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

    /// This reading advanced by one logical tick.
    ///
    /// Normal case: increment the logical counter within the same millisecond.
    /// Overflow case: when the counter is already at `u32::MAX`, bump `physical` by 1 ms and reset
    /// the counter to zero. This is the standard HLC fallback and preserves strict monotonicity:
    /// the resulting `(physical + 1, 0)` is always greater than `(physical, u32::MAX)`. This
    /// function is the sole owner of that roll — the underlying `checked_next` on
    /// [`LogicalCounter`] is crate-private, so no caller outside can rebuild the rule by hand.
    ///
    /// [`PhysicalTime::next_ms`] saturates, so even a physical time of `u64::MAX` cannot wrap (it
    /// stays at `u64::MAX`, which keeps the pair non-decreasing in the degenerate case).
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

    /// The HLC "observe" arithmetic: move `self` strictly past both physical time and a remote
    /// reading.
    ///
    /// This is the ordering core shared by [`Clock::observe`] and [`Clock::observe_trusted`]
    /// implementations. It reads no clock of its own — `phys_now` is supplied by the adapter, which
    /// owns the single physical-time read — so the whole rule stays in the domain and stays
    /// testable without wall-clock time. It needs no [`NodeId`] either: the adapter attaches its
    /// own identity when it mints the resulting reading into a [`Timestamp`].
    ///
    /// `remote_physical` is an [`AdmittedTime`]: the untrusted path must have run it through
    /// [`AdmittedTime::clamped_to_drift`], and the trusted path must have said
    /// [`AdmittedTime::trusted`] out loud. A raw remote reading has no route in here.
    /// `remote_logical` is the logical counter from the original remote stamp (never affected by a
    /// clamp).
    ///
    /// Preserves strict monotonicity and counter semantics: the stored state always ends up greater
    /// than or equal to `max(self, physical now, admitted remote)`, advanced by one logical tick
    /// whenever the dominant bucket is not a fresh physical-time leap.
    pub fn advance_past_remote(
        &mut self,
        phys_now: PhysicalTime,
        remote_physical: AdmittedTime,
        remote_logical: LogicalCounter,
    ) {
        let remote_physical = remote_physical.physical();
        let max_physical = phys_now.max(self.physical).max(remote_physical);

        // Pick the base counter from the dominant physical-time bucket, then advance one logical
        // tick. next_tick() handles u32::MAX → (physical + 1, 0) so the result can never wrap.
        let base_logical = if max_physical == self.physical && max_physical == remote_physical {
            // Both self and the (admitted) remote share max_physical: take the larger counter.
            self.logical.max(remote_logical)
        } else if max_physical == self.physical {
            self.logical
        } else if max_physical == remote_physical {
            remote_logical
        } else {
            // Physical time leapt past both: fresh bucket, logical counter starts at 0.
            // We return early here rather than running through next_tick() to preserve the
            // original semantics (counter = 0, not 1) for the physical-time-dominates case.
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

/// A Hybrid Logical Clock timestamp: the **LWW ordering key**.
///
/// A clock reading ([`Hlc`]) paired with the identity of the node that minted it. The fields are
/// compared in declaration order and `Hlc`'s own order is `(physical, logical)`, so the derived
/// [`Ord`] composes to exactly the total order `(physical, logical, node_id)` used to resolve
/// conflicts. The `node_id` is the deterministic tie-break — not a clock component — which is why
/// it lives here and not in `Hlc`. See the [module documentation](crate::clock) for the rationale.
///
/// Each component is its own type, so they cannot be transposed at a call site; bincode and
/// `rsos`'s canonical encoding both write a struct as its fields in declaration order with no
/// framing, and a newtype struct as its inner value alone, so neither the newtypes nor the nesting
/// costs anything on the wire (pinned by `tests/timestamp_wire_format.rs` in the `reconcile`
/// package, where the codec lives).
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
    /// There is no "parse" step here because there is nothing left to parse: each component owns
    /// its own construction ([`Hlc::new`], [`NodeId::new`]) and every bit pattern of each is a
    /// legal value. What used to be the hazard — three interchangeable integers, two of them
    /// `u64` — is now a type error, and grouping the two clock components under [`Hlc`] keeps a
    /// reading from being spread across a flat argument list where a tie-break also lives.
    ///
    /// Mostly useful in tests and when reconstructing a timestamp from external storage;
    /// normal code obtains timestamps from the store's internal clock.
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

/// The domain's **clock port**: the seam through which the reconciliation engine reads time.
///
/// The Hybrid Logical Clock algorithm stays in the domain (see [`Hlc::next_tick`] /
/// [`Hlc::advance_past_remote`]); an adapter behind this port performs the single physical-time
/// read and owns this node's [`NodeId`], which it attaches to each reading it mints (`HlcClock`, in
/// the `reconcile` crate, is the default adapter; a test adapter can be a deterministic stub).
/// Pinning the timestamp to [`Timestamp`] — rather than a generic
/// associated type — keeps the port object-safe and avoids leaking a clock type parameter into the
/// engine, store and `Config` (`ARCHITECTURE.md` §3.4); the engine therefore holds the port as
/// `Arc<dyn Clock>`.
pub trait Clock: Send + Sync + 'static {
    /// Mint a strictly-monotonic local timestamp for a write or an outgoing message.
    fn now(&self) -> Timestamp;
    /// This node's identity — the [`NodeId`] component the adapter stamps onto every timestamp it
    /// mints, and the deterministic tie-break that makes the conflict order total.
    ///
    /// Reading it here rather than caching it elsewhere means the reported identity can never
    /// disagree with the one actually stamped, and — unlike calling [`now`](Clock::now) for it —
    /// costs no counter tick.
    fn node_id(&self) -> NodeId;
    /// Advance the clock past a timestamp received from a peer, so that a subsequent
    /// [`now`](Clock::now) is ordered after it (this is what prevents lost updates under skew).
    ///
    /// This "ordered-after" guarantee holds for remote stamps within a bounded lead over
    /// physical time; an implementation may clamp a remote stamp that leads physical time by
    /// an implausibly large, configurable margin (default [`MAX_CLOCK_DRIFT`]) so
    /// that a single poisoned stamp cannot pin the local clock into the far future — that is what
    /// [`AdmittedTime::clamped_to_drift`] is for. The total order on [`Timestamp`] and the
    /// strict-`>` merge are unaffected.
    fn observe(&self, remote: Timestamp);
    /// Advance the clock past a stamp that **this node itself authored** (e.g. restored from its
    /// own persisted state), so that the first post-restart [`now`](Clock::now) is strictly
    /// ordered after every pre-restart write.
    ///
    /// Unlike [`observe`](Clock::observe), implementations must **not** apply the far-future
    /// suspicion clamp to a self-authored stamp — this is the one caller entitled to
    /// [`AdmittedTime::trusted`]. The clamp guards against a remote peer injecting an arbitrarily
    /// large wall value; it must not fire on a stamp we wrote ourselves, because refusing to chase
    /// our own past output re-introduces own-write shadowing after a backward clock step (NTP
    /// correction, VM resume) that moved physical time behind the persisted max.
    ///
    /// The default implementation delegates to [`observe`](Clock::observe), which is safe for
    /// adapters that already have no clamp (e.g. the test `ManualClock`). `HlcClock` overrides
    /// this with a clamp-free advance to preserve the guarantee above.
    fn observe_trusted(&self, remote: Timestamp) {
        self.observe(remote);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terse constructors for the tests below; production call sites spell the components out.
    fn hlc(physical: u64, logical: u32) -> Hlc {
        Hlc::new(
            PhysicalTime::from_millis(physical),
            LogicalCounter::new(logical),
        )
    }

    fn ts(physical: u64, logical: u32, node_id: u64) -> Timestamp {
        Timestamp::new(hlc(physical, logical), NodeId::new(node_id))
    }

    /// The clock reading's own order is the first two thirds of the conflict order: physical time
    /// dominates the logical counter, and no identity takes part in it at all.
    #[test]
    fn hlc_order_is_physical_then_logical() {
        assert!(hlc(101, 0) > hlc(100, u32::MAX));
        assert!(hlc(100, 1) > hlc(100, 0));
        assert_eq!(hlc(100, 0), hlc(100, 0));

        let mut sorted = vec![hlc(100, 1), hlc(99, 9), hlc(100, 0)];
        sorted.sort();
        assert_eq!(sorted, vec![hlc(99, 9), hlc(100, 0), hlc(100, 1)]);
    }

    /// The derived `Ord` *is* the conflict-resolution policy, so pin it component by component:
    /// `physical` dominates `logical` dominates `node_id`, and each is decisive only when every
    /// component before it ties.
    #[test]
    fn total_order_is_physical_then_logical_then_node_id() {
        // physical dominates, whatever the other two say.
        assert!(ts(101, 0, 0) > ts(100, u32::MAX, u64::MAX));
        // On an equal physical time, the logical counter decides — whatever the node_id says.
        assert!(ts(100, 1, 0) > ts(100, 0, u64::MAX));
        // On an equal physical time *and* counter, the node_id is the tie-break: deterministic, and
        // identical on every replica, which is what makes the merge commutative.
        assert!(ts(100, 0, 2) > ts(100, 0, 1));
        assert!(ts(100, 0, 1) < ts(100, 0, 2));
        // All three equal: equal stamps, no arbitration.
        assert_eq!(ts(100, 0, 1), ts(100, 0, 1));
        assert!(ts(100, 0, 1) <= ts(100, 0, 1));
        // And the order is the lexicographic one on the whole triple.
        let mut sorted = vec![ts(100, 0, 2), ts(99, 9, 9), ts(100, 1, 0), ts(100, 0, 1)];
        sorted.sort();
        assert_eq!(
            sorted,
            vec![ts(99, 9, 9), ts(100, 0, 1), ts(100, 0, 2), ts(100, 1, 0)]
        );
    }

    /// Verify that `next_tick()` never wraps the counter: at u32::MAX it rolls physical forward.
    #[test]
    fn next_tick_never_wraps_counter() {
        assert_eq!(
            hlc(1000, u32::MAX).next_tick(),
            hlc(1001, 0),
            "physical should roll, counter reset"
        );

        // Non-overflow case: straightforward increment.
        assert_eq!(hlc(1000, 0).next_tick(), hlc(1000, 1));

        // Saturating physical time: u64::MAX + 1 must not wrap.
        assert_eq!(
            hlc(u64::MAX, u32::MAX).next_tick(),
            hlc(u64::MAX, 0),
            "physical time saturates at u64::MAX"
        );
    }

    /// The ordering core is pure: given a physical-time reading, it advances strictly past both
    /// the previous state and the remote stamp, with no clock — and no identity — of its own.
    #[test]
    fn advance_past_remote_is_strictly_monotonic() {
        let mut last = hlc(10, 3);

        // Remote dominates: result is ordered strictly after it.
        let remote = ts(50, 4, 9);
        last.advance_past_remote(
            PhysicalTime::EPOCH,
            AdmittedTime::trusted(remote.physical()),
            remote.logical(),
        );
        assert!(last > remote.hlc(), "{last:?} !> {:?}", remote.hlc());

        // Physical time leaps past both: fresh physical-time bucket, counter reset.
        let before = last;
        last.advance_past_remote(
            PhysicalTime::from_millis(100),
            AdmittedTime::trusted(PhysicalTime::EPOCH),
            LogicalCounter::ZERO,
        );
        assert_eq!(last, hlc(100, 0));
        assert!(last > before);

        // Neither physical time nor the remote advanced: the logical counter ticks.
        let before = last;
        last.advance_past_remote(
            PhysicalTime::EPOCH,
            AdmittedTime::trusted(PhysicalTime::EPOCH),
            LogicalCounter::ZERO,
        );
        assert_eq!(last, hlc(100, 1));
        assert!(last > before);
    }

    /// The clamping constructor is the whole security property: an out-of-budget reading comes back
    /// capped and flagged, an in-budget one comes back untouched.
    #[test]
    fn clamped_to_drift_caps_a_far_future_reading() {
        let now = PhysicalTime::from_millis(1_000);
        let budget = ClockDrift::from_millis(500);

        let admitted =
            AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(u64::MAX), now, budget);
        assert_eq!(admitted.physical(), PhysicalTime::from_millis(1_500));
        assert!(admitted.was_clamped());

        // Exactly at the cap is still admissible as-is.
        let at_cap = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(1_500), now, budget);
        assert_eq!(at_cap.physical(), PhysicalTime::from_millis(1_500));
        assert!(!at_cap.was_clamped());

        // Below the cap: passed through verbatim.
        let below = AdmittedTime::clamped_to_drift(PhysicalTime::from_millis(1_200), now, budget);
        assert_eq!(below.physical(), PhysicalTime::from_millis(1_200));
        assert!(!below.was_clamped());

        // A budget large enough to overflow the cap must saturate, not wrap — otherwise the cap
        // would land *below* `now` and clamp legitimate readings.
        let huge = AdmittedTime::clamped_to_drift(
            PhysicalTime::from_millis(u64::MAX),
            now,
            ClockDrift::from_millis(u64::MAX),
        );
        assert_eq!(huge.physical(), PhysicalTime::from_millis(u64::MAX));
        assert!(!huge.was_clamped());

        // The escape hatch admits anything, by construction.
        assert_eq!(
            AdmittedTime::trusted(PhysicalTime::from_millis(u64::MAX)).physical(),
            PhysicalTime::from_millis(u64::MAX)
        );
    }

    /// A clamped remote reading must not drag the local clock past the cap, while an unclamped one
    /// must. This is the same distinction the adapter's `observe`/`observe_trusted` pair draws,
    /// checked here without any wall-clock read.
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
