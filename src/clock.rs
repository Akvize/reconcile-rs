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
//! would require version vectors or a CRDT and is out of scope (see issue #110).

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

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
    fn observe(&self, remote: Timestamp);
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
    /// Last timestamp produced or observed; the wall/counter pair is updated atomically
    /// under the mutex so that [`now`](HlcClock::now) stays strictly monotonic.
    last: Mutex<Timestamp>,
}

impl HlcClock {
    /// Create a clock for the node identified by `node_id`.
    pub fn new(node_id: u64) -> HlcClock {
        HlcClock {
            node_id,
            last: Mutex::new(Timestamp {
                wall_ms: 0,
                counter: 0,
                node_id,
            }),
        }
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
            Timestamp {
                wall_ms: last.wall_ms,
                counter: last.counter + 1,
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
    fn observe(&self, remote: Timestamp) {
        let pt = phys_now_ms();
        let mut last = self.last.lock();
        let max_wall = pt.max(last.wall_ms).max(remote.wall_ms);
        let counter = if max_wall == last.wall_ms && max_wall == remote.wall_ms {
            last.counter.max(remote.counter) + 1
        } else if max_wall == last.wall_ms {
            last.counter + 1
        } else if max_wall == remote.wall_ms {
            remote.counter + 1
        } else {
            0
        };
        *last = Timestamp {
            wall_ms: max_wall,
            counter,
            node_id: self.node_id,
        };
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
        // Force the clock far into the future so physical time cannot advance past it for
        // the duration of the test: every `now()` must then bump the counter.
        clock.observe(Timestamp::new(u64::MAX - 100, 0, 9));
        let a = clock.now();
        let b = clock.now();
        assert_eq!(a.wall_ms(), b.wall_ms());
        assert_eq!(b.counter(), a.counter() + 1);
    }

    #[test]
    fn observe_advances_past_a_future_timestamp() {
        // Reproduces defect (a): a peer with a clock running ahead. After observing its
        // timestamp, our next local write must be ordered *after* it, not lost.
        let clock = HlcClock::new(1);
        let future = Timestamp::new(phys_now_ms() + 10_000_000, 5, 2);
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
}
