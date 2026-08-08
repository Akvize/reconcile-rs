// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Hybrid Logical Clock (HLC) timestamps: the ordering half.
//!
//! This module holds the *domain* half of the clock: the [`Timestamp`] value type, the [`Clock`]
//! port through which the register reads time, and the HLC ordering arithmetic
//! ([`advance`]/[`advance_past`]) that keeps stamps strictly monotonic. It reads no wall clock —
//! the single physical-time read lives in the `HlcClock` adapter, which sits in the `reconcile`
//! crate behind this port.
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
//! * carries a `node_id`, giving a **globally deterministic total order**
//!   `(wall_ms, counter, node_id)`. Every replica therefore picks the *same* survivor on a
//!   conflict, which makes the merge commutative, associative and idempotent — i.e. genuine
//!   Strong Eventual Consistency.
//!
//! LWW still discards one of two *genuinely concurrent* writes by design; recovering both
//! would require version vectors or a CRDT and is out of scope.

use serde::{Deserialize, Serialize};

/// Default maximum number of milliseconds by which a remote clock may lead physical time before
/// its `wall_ms` is clamped when updating the local clock state.
///
/// This is only the default the `HlcClock` adapter is built with; the threshold is a property of
/// the clock itself, overridable at construction (see `HlcClock::with_max_clock_drift_ms`) rather
/// than a knob on the store's `Config`. The rationale below explains why this default value was
/// chosen.
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
/// consumers of stored stamps — e.g. the tombstone expiry arithmetic in `replicated_map`,
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
pub const MAX_CLOCK_DRIFT_MS: u64 = 3_600_000; // 1 hour

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

/// Advance a `(wall_ms, counter)` pair by one logical tick without wrapping.
///
/// Normal case: increment the counter within the same millisecond.
/// Overflow case: when the counter is already at `u32::MAX`, bump `wall_ms` by 1 ms and
/// reset the counter to 0. This is the standard HLC fallback and preserves strict
/// monotonicity: the resulting `(wall_ms + 1, 0)` is always greater than `(wall_ms, u32::MAX)`.
///
/// `wall_ms.saturating_add(1)` ensures that even a wall value of `u64::MAX` cannot wrap
/// (it saturates at `u64::MAX`, which keeps the pair non-decreasing in the degenerate case).
pub fn advance(wall_ms: u64, counter: u32) -> (u64, u32) {
    match counter.checked_add(1) {
        Some(c) => (wall_ms, c),
        None => (wall_ms.saturating_add(1), 0),
    }
}

/// The HLC "observe" arithmetic: move `last` strictly past both physical time and a remote stamp.
///
/// This is the ordering core shared by [`Clock::observe`] and [`Clock::observe_trusted`]
/// implementations. It reads no clock of its own — `phys_now_ms` is supplied by the adapter, which
/// owns the single physical-time read — so the whole rule stays in the domain and stays testable
/// without wall-clock time.
///
/// `effective_remote_wall` is the wall value to use for the remote stamp: an adapter applying the
/// far-future clamp passes the clamped value, the trusted path passes the raw value.
/// `remote_counter` is the counter from the original remote stamp (never affected by a clamp).
/// `node_id` is the local node's identity, stamped onto the resulting state.
///
/// Preserves strict monotonicity and counter semantics: the stored state always ends up greater
/// than or equal to `max(last, physical now, effective remote)`, advanced by one logical tick
/// whenever the dominant bucket is not a fresh physical-time leap.
pub fn advance_past(
    last: &mut Timestamp,
    phys_now_ms: u64,
    effective_remote_wall: u64,
    remote_counter: u32,
    node_id: u64,
) {
    let max_wall = phys_now_ms.max(last.wall_ms).max(effective_remote_wall);

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
            node_id,
        };
        return;
    };

    let (new_wall, new_counter) = advance(max_wall, base_counter);
    *last = Timestamp {
        wall_ms: new_wall,
        counter: new_counter,
        node_id,
    };
}

/// The domain's **clock port**: the seam through which the reconciliation engine reads time.
///
/// The Hybrid Logical Clock algorithm stays in the domain (see [`advance`]/[`advance_past`]); an
/// adapter behind this port performs the single physical-time read (`HlcClock`, in the `reconcile`
/// crate, is the default adapter; a test adapter can be a deterministic stub). Pinning the
/// timestamp to [`Timestamp`] — rather than a generic associated type — keeps the port object-safe
/// and avoids leaking a clock type parameter into the engine, store and `Config`
/// (`ARCHITECTURE.md` §3.4); the engine therefore holds the port as `Arc<dyn Clock>`.
pub trait Clock: Send + Sync + 'static {
    /// Mint a strictly-monotonic local timestamp for a write or an outgoing message.
    fn now(&self) -> Timestamp;
    /// This node's identity — the `node_id` component the adapter stamps onto every timestamp it
    /// mints, and the deterministic tie-break that makes the conflict order total.
    ///
    /// Reading it here rather than caching it elsewhere means the reported identity can never
    /// disagree with the one actually stamped, and — unlike calling [`now`](Clock::now) for it —
    /// costs no counter tick.
    fn node_id(&self) -> u64;
    /// Advance the clock past a timestamp received from a peer, so that a subsequent
    /// [`now`](Clock::now) is ordered after it (this is what prevents lost updates under skew).
    ///
    /// This "ordered-after" guarantee holds for remote stamps within a bounded lead over
    /// physical time; an implementation may clamp a remote stamp that leads physical time by
    /// an implausibly large, configurable margin (default [`MAX_CLOCK_DRIFT_MS`]) so
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The ordering core is pure: given a physical-time reading, it advances strictly past both
    /// the previous state and the remote stamp, with no clock of its own.
    #[test]
    fn advance_past_is_strictly_monotonic() {
        let mut last = Timestamp::new(10, 3, 1);

        // Remote dominates: result is ordered strictly after it.
        let remote = Timestamp::new(50, 4, 9);
        advance_past(&mut last, 0, remote.wall_ms(), remote.counter(), 1);
        assert!(last > remote, "{last:?} !> {remote:?}");

        // Physical time leaps past both: fresh wall bucket, counter reset.
        let before = last;
        advance_past(&mut last, 100, 0, 0, 1);
        assert_eq!(last, Timestamp::new(100, 0, 1));
        assert!(last > before);

        // Neither physical time nor the remote advanced: the logical counter ticks.
        let before = last;
        advance_past(&mut last, 0, 0, 0, 1);
        assert_eq!(last, Timestamp::new(100, 1, 1));
        assert!(last > before);
    }
}
