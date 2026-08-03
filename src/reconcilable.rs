// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The domain value types: [`Entry`] — a stamped stored cell — and [`State`] — the live-value /
//! tombstone payload — together with the last-write-wins conflict policy carried by
//! [`Entry::merge`].
//!
//! A single, intention-revealing [`Entry`] replaces the historical `(Timestamp, Option<V>)` tuple
//! and the value-shape helper traits it used to carry (`MaybeTombstone`, `Timestamped`,
//! `Reconcilable`). Its [`State`] payload doubles as the **timestamp-less projection** that powers
//! the dateless [`ReconcileMirror`](crate::mirror::ReconcileMirror) (see [`Entry::project`]).

/// The logical state of a stored cell: a live value ([`Present`](State::Present)) or a deletion
/// marker ([`Tombstone`](State::Tombstone)).
///
/// `State` is *also* the **timestamp-less projection** of an [`Entry`] (see [`Entry::project`]).
/// Its [`Hash`] is **value-only by construction**: it carries no [`Timestamp`](crate::Timestamp),
/// so two replicas that agree on the logical value compute the *same* per-element fingerprint
/// regardless of when (or on which node) each last wrote it. This is what lets a dateless
/// [`ReconcileMirror`](crate::mirror::ReconcileMirror) and a dated
/// [`ReconcileStore`](crate::reconcile_store::ReconcileStore) converge over the shared range-diff
/// protocol without the mirror ever storing timestamps (invariant #8 in `docs/ARCHITECTURE.md` §5).
///
/// A dated [`Entry`] deliberately hashes **with** its stamp (required by the engine's
/// `version_hash` for the causal-stability acks), while its `State` projection hashes the value
/// alone; the two hashes must stay distinct.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum State<V> {
    /// A live value.
    Present(V),
    /// A deletion marker.
    Tombstone,
}

impl<V> State<V> {
    /// Returns `true` if this state is a deletion marker (tombstone).
    pub fn is_tombstone(&self) -> bool {
        matches!(self, State::Tombstone)
    }

    /// Borrow the live value, or `None` if this is a tombstone.
    pub fn value(&self) -> Option<&V> {
        match self {
            State::Present(v) => Some(v),
            State::Tombstone => None,
        }
    }

    /// Mutably borrow the live value, or `None` if this is a tombstone.
    pub fn value_mut(&mut self) -> Option<&mut V> {
        match self {
            State::Present(v) => Some(v),
            State::Tombstone => None,
        }
    }

    /// Consume the state, yielding the live value or `None` if it was a tombstone.
    pub fn into_value(self) -> Option<V> {
        match self {
            State::Present(v) => Some(v),
            State::Tombstone => None,
        }
    }
}

/// A stored cell: a [`State`] payload stamped with the timestamp `T` that ordered its last write.
///
/// This replaces the leaky `(Timestamp, Option<V>)` tuple with a concrete domain type that carries
/// the tombstone, timestamp, and merge semantics itself. `T` is the stamp type — in this crate
/// always [`Timestamp`](crate::Timestamp) — kept generic so the merge policy is expressed once over
/// any totally-ordered stamp.
///
/// Its [`Hash`] covers **both** `stamp` and `state`, so `version_hash` distinguishes two tombstones
/// written at different times (invariant #7 in `docs/ARCHITECTURE.md` §5). Contrast [`State`], whose hash
/// is value-only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Entry<T, V> {
    /// The stamp that ordered this cell's last write.
    pub stamp: T,
    /// The live value or tombstone.
    pub state: State<V>,
}

impl<T, V> Entry<T, V> {
    /// A live value stamped at `stamp`.
    pub fn present(stamp: T, value: V) -> Self {
        Entry {
            stamp,
            state: State::Present(value),
        }
    }

    /// A tombstone stamped at `stamp`.
    pub fn tombstone(stamp: T) -> Self {
        Entry {
            stamp,
            state: State::Tombstone,
        }
    }

    /// Returns `true` if this entry is a deletion marker (tombstone).
    ///
    /// The reconciliation engine uses this to decide which applied updates require
    /// causal-stability tracking before the corresponding key can be garbage-collected.
    /// See the tombstone-resurrection discussion on
    /// [`ReconcileStore`](crate::reconcile_store::ReconcileStore).
    pub fn is_tombstone(&self) -> bool {
        self.state.is_tombstone()
    }

    /// Borrow the live value, or `None` if this entry is a tombstone.
    pub fn value(&self) -> Option<&V> {
        self.state.value()
    }

    /// Mutably borrow the live value, or `None` if this entry is a tombstone.
    pub fn value_mut(&mut self) -> Option<&mut V> {
        self.state.value_mut()
    }

    /// Consume the entry, yielding the live value or `None` if it was a tombstone.
    pub fn into_value(self) -> Option<V> {
        self.state.into_value()
    }
}

impl<T, V: Clone> Entry<T, V> {
    /// Project this dated entry to its timestamp-less [`State`] form.
    ///
    /// The dated store maintains a parallel value-only *projection* tree so it can reconcile with
    /// dateless mirrors over the range-diff protocol without exposing timestamps. Because `State`
    /// hashes value-only, the projection's per-element fingerprints match a mirror's (invariant #8).
    pub fn project(&self) -> State<V> {
        self.state.clone()
    }
}

impl<T: Ord + Copy, V: Clone> Entry<T, V> {
    /// Resolve a conflict between two entries by **last-write-wins** over the stamp order.
    ///
    /// Because [`Timestamp`](crate::Timestamp) is a **total order** `(wall_ms, counter, node_id)`,
    /// `merge` is `max` over that order with a **strict `>`**: it is commutative, associative and
    /// idempotent, so every replica converges to the same value (Strong Eventual Consistency). Two
    /// distinct writes from the *same* node never share a `Timestamp` (the node bumps the counter);
    /// two writes from *different* nodes are kept apart by `node_id`, the deterministic tie-break.
    ///
    /// That tie-break is only as unique as `node_id` itself. The `node_id` is a random 64-bit value
    /// (see [`Config::node_id`](crate::reconcile_store::Config::node_id)), so distinctness is
    /// **probabilistic, not guaranteed**: by the birthday bound, the probability that any two of `n`
    /// nodes draw the same id is roughly `n² / 2^65` (≈ 3e-14 at 1000 nodes, ≈ 3e-10 at 100 000). If
    /// two live nodes *do* collide, two genuinely different values can share a full `Timestamp`; the
    /// equal-stamp branch then keeps the local value on each side, so the two replicas can diverge
    /// permanently for that key. There is no collision detection: a `node_id` rides inside every
    /// `Timestamp` on the wire, but nothing asserts ownership of an id, so there is no identity
    /// handshake by which a node could notice another claiming its id (out of scope). Pin a stable,
    /// distinct id per node via [`Config::with_node_id`](crate::reconcile_store::Config::with_node_id)
    /// when you need a guarantee rather than an overwhelming probability. See the
    /// [`clock`](crate::clock) module for why the previous physical-clock scheme was unsafe.
    pub fn merge(&self, other: &Self) -> Self {
        if other.stamp > self.stamp {
            other.clone()
        } else {
            self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Timestamp;

    fn present(
        wall_ms: u64,
        counter: u32,
        node_id: u64,
        v: &'static str,
    ) -> Entry<Timestamp, &'static str> {
        Entry::present(Timestamp::new(wall_ms, counter, node_id), v)
    }

    #[test]
    fn merge_is_commutative_on_equal_wall_and_counter() {
        // Distinct node_ids with the same wall+counter: the old physical-clock tie-break
        // kept the local value, so the two replicas diverged forever (defect (b)). The
        // total order makes the survivor deterministic regardless of argument order.
        let a = present(100, 0, 1, "a");
        let b = present(100, 0, 2, "b");
        assert_eq!(a.merge(&b), b.merge(&a));
        // The higher node_id wins consistently.
        assert_eq!(a.merge(&b).value(), Some(&"b"));
    }

    #[test]
    fn merge_is_idempotent() {
        let a = present(7, 3, 42, "x");
        assert_eq!(a.merge(&a), a);
    }

    #[test]
    fn merge_picks_the_greater_timestamp() {
        let older = present(10, 0, 5, "old");
        let newer = present(11, 0, 1, "new");
        assert_eq!(older.merge(&newer).value(), Some(&"new"));
        assert_eq!(newer.merge(&older).value(), Some(&"new"));
    }

    #[test]
    fn tombstone_is_a_tombstone() {
        let live = present(1, 0, 0, "v");
        let dead: Entry<Timestamp, &'static str> = Entry::tombstone(Timestamp::new(2, 0, 0));
        assert!(!live.is_tombstone());
        assert!(dead.is_tombstone());
        assert_eq!(live.value(), Some(&"v"));
        assert_eq!(dead.value(), None);
    }

    #[test]
    fn projection_is_the_state() {
        let live = present(1, 0, 0, "v");
        assert_eq!(live.project(), State::Present("v"));
        let dead: Entry<Timestamp, &'static str> = Entry::tombstone(Timestamp::new(2, 0, 0));
        assert_eq!(dead.project(), State::Tombstone);
    }
}
