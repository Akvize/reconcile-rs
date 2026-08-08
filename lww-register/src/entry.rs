// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Entry`] / [`State`] domain types (see `ARCHITECTURE.md` §3.6).
//!
//! A stored cell used to be represented as an untyped `(Timestamp, Option<V>)` tuple, with three
//! value-shape helper traits (`Reconcilable`, `MaybeTombstone`, `Projectable`) each carrying a
//! single implementation over that tuple. [`Entry`] replaces the tuple with a named domain type,
//! and its inherent methods absorb what those traits used to provide:
//!
//! - [`Entry::merge`] is the last-write-wins conflict-resolution policy (formerly `Reconcilable`).
//! - [`Entry::is_tombstone`] / [`Entry::value`] read the tombstone/live state (formerly
//!   `MaybeTombstone`).
//! - [`Entry::project`] produces the timestamp-less [`State<V>`] projection consumed by a dateless
//!   `ReadReplicaMap` (formerly `Projectable` / `ValueOnly<V>`).
//!
//! [`State<V>`] is isomorphic to the old `ValueOnly<V>(Option<V>)`: `Present(v) ↔ Some(v)`,
//! `Tombstone ↔ None`. It is *itself* the value-only wire/projection type — a dated store's
//! projection tree is `FingerprintTreeMap<K, State<V>>` and a read replica stores
//! `FingerprintTreeMap<K, State<V>>` directly.
//!
//! # Invariant 8 — the two summaries must stay distinct
//!
//! The distinction is **structural**, so every content summary derived field-by-field inherits it:
//! [`Entry`] has a `stamp` field, [`State`] has no [`Timestamp`](crate::clock::Timestamp) field at
//! all. That covers the canonical serde encoding `rsos` fingerprints elements through (and which
//! also feeds `version_hash`, the causal-stability tombstone acknowledgment token in
//! `replica.rs`), and equally the derived [`Hash`]. This is what lets a dated store and a dateless `ReadReplicaMap` compute
//! *identical* per-element fingerprints for the same logical value regardless of when (or on which
//! node) it was last written — guarded by
//! `read_replica_map.rs::value_fingerprint_is_timestamp_independent` and by the test below.

use serde::{Deserialize, Serialize};

/// A timestamp-less projection of a value: either a live value or a tombstone (deletion marker).
///
/// Isomorphic to `Option<V>` (`Present(v) ↔ Some(v)`, `Tombstone ↔ None`), but a named type reads
/// better at the call sites that matter here (an [`Entry`]'s projection, and the value a
/// `ReadReplicaMap` stores) and its content summary is value-only *by construction* — no
/// [`Timestamp`](crate::clock::Timestamp) field exists to accidentally include, in the serde
/// encoding fingerprints are computed from or in the derived [`Hash`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum State<V> {
    /// A live value.
    Present(V),
    /// A deletion marker.
    Tombstone,
}

impl<V> State<V> {
    /// Borrow the live value, or `None` if this is a tombstone.
    pub fn as_value(&self) -> Option<&V> {
        match self {
            State::Present(v) => Some(v),
            State::Tombstone => None,
        }
    }

    /// Returns `true` if this is a deletion marker (tombstone).
    pub fn is_tombstone(&self) -> bool {
        matches!(self, State::Tombstone)
    }

    /// Mutably borrow the live value, or `None` if this is a tombstone.
    pub fn as_value_mut(&mut self) -> Option<&mut V> {
        match self {
            State::Present(v) => Some(v),
            State::Tombstone => None,
        }
    }
}

impl<V> From<Option<V>> for State<V> {
    fn from(value: Option<V>) -> Self {
        match value {
            Some(v) => State::Present(v),
            None => State::Tombstone,
        }
    }
}

impl<V> From<State<V>> for Option<V> {
    fn from(state: State<V>) -> Self {
        match state {
            State::Present(v) => Some(v),
            State::Tombstone => None,
        }
    }
}

/// A stored cell: a value (or tombstone) stamped with a causality/conflict-resolution token `T`
/// (in practice, always [`Timestamp`](crate::clock::Timestamp)).
///
/// Replaces the untyped `(Timestamp, Option<V>)` tuple that used to be threaded through the whole
/// crate. See the [module documentation](self) for how it absorbs the three dissolved
/// value-shape traits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry<T, V> {
    /// The causality/conflict-resolution stamp (a Hybrid Logical Clock [`Timestamp`](crate::clock::Timestamp)).
    pub stamp: T,
    /// The live value, or a tombstone.
    pub state: State<V>,
}

impl<T, V> Entry<T, V> {
    /// Build a live entry.
    pub fn present(stamp: T, value: V) -> Self {
        Entry {
            stamp,
            state: State::Present(value),
        }
    }

    /// Build a tombstone entry.
    pub fn tombstone(stamp: T) -> Self {
        Entry {
            stamp,
            state: State::Tombstone,
        }
    }

    /// Build an entry from an `Option<V>` payload: `Some` is live, `None` is a tombstone.
    pub fn new(stamp: T, value: Option<V>) -> Self {
        Entry {
            stamp,
            state: value.into(),
        }
    }

    /// Returns `true` if this entry is a deletion marker (tombstone).
    pub fn is_tombstone(&self) -> bool {
        self.state.is_tombstone()
    }

    /// Borrow the live value, or `None` if this entry is a tombstone.
    pub fn value(&self) -> Option<&V> {
        self.state.as_value()
    }

    /// Mutably borrow the live value, or `None` if this entry is a tombstone.
    pub fn value_mut(&mut self) -> Option<&mut V> {
        self.state.as_value_mut()
    }
}

impl<T: Ord + Copy, V: Clone> Entry<T, V> {
    /// Project this dated entry to its timestamp-less [`State<V>`] form.
    ///
    /// This is what a dated store's value-only *projection* tree stores, and what a dateless
    /// `ReadReplicaMap` converges on. See invariant 8 in `ARCHITECTURE.md` §5: the projection
    /// deliberately has no `stamp` field, so no content summary of it can include one.
    pub fn project(&self) -> State<V> {
        self.state.clone()
    }

    /// Last-write-wins conflict resolution: the entry with the strictly greater `stamp` wins.
    ///
    /// Because `T` (in practice [`Timestamp`](crate::clock::Timestamp)) is a **total order**,
    /// `merge` is `max` over that order: commutative, associative and idempotent, so every
    /// replica converges to the same entry (Strong Eventual Consistency). See `clock.rs` for why
    /// the equal-stamp branch never actually has to arbitrate between *distinct* values.
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
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<H: Hash>(value: &H) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn merge_is_commutative_on_equal_wall_and_counter() {
        let a = Entry::present(Timestamp::new(100, 0, 1), "a");
        let b = Entry::present(Timestamp::new(100, 0, 2), "b");
        assert_eq!(a.merge(&b), b.merge(&a));
        assert_eq!(a.merge(&b).value(), Some(&"b"));
    }

    #[test]
    fn merge_is_idempotent() {
        let a = Entry::present(Timestamp::new(7, 3, 42), "x");
        assert_eq!(a.merge(&a), a);
    }

    #[test]
    fn merge_picks_the_greater_stamp() {
        let older = Entry::present(Timestamp::new(10, 0, 5), "old");
        let newer = Entry::present(Timestamp::new(11, 0, 1), "new");
        assert_eq!(older.merge(&newer).value(), Some(&"new"));
        assert_eq!(newer.merge(&older).value(), Some(&"new"));
    }

    #[test]
    fn is_tombstone_reflects_state() {
        let live = Entry::present(Timestamp::new(1, 0, 0), 42);
        let dead: Entry<Timestamp, i32> = Entry::tombstone(Timestamp::new(2, 0, 0));
        assert!(!live.is_tombstone());
        assert_eq!(live.value(), Some(&42));
        assert!(dead.is_tombstone());
        assert_eq!(dead.value(), None);
    }

    #[test]
    fn project_is_isomorphic_to_option() {
        let live = Entry::present(Timestamp::new(1, 0, 0), "v");
        let dead: Entry<Timestamp, &str> = Entry::tombstone(Timestamp::new(2, 0, 0));
        assert_eq!(live.project(), State::Present("v"));
        assert_eq!(dead.project(), State::Tombstone);
    }

    /// Invariant 8 (`ARCHITECTURE.md` §5): the dated `Entry` summarizes **with** its `stamp`, its
    /// projected `State<V>` summarizes the value **alone** — so two entries that differ only by
    /// timestamp project to fingerprints that agree, while the dated entries themselves do not.
    ///
    /// Checked here through the derived `Hash`, which is a stand-in for the field-by-field walk the
    /// real range fingerprint does: this crate is infrastructure-free and knows nothing of `rsos`.
    /// The same invariant is asserted on the actual fingerprint in
    /// `read_replica_map.rs::value_fingerprint_is_timestamp_independent`.
    #[test]
    fn projection_hash_is_timestamp_independent_but_entry_hash_is_not() {
        let early = Entry::present(Timestamp::new(1, 0, 0), "same-value");
        let late = Entry::present(Timestamp::new(2, 0, 0), "same-value");

        // The dated entries differ (different stamp) and, overwhelmingly likely, hash
        // differently.
        assert_ne!(early, late);
        assert_ne!(
            hash_of(&early),
            hash_of(&late),
            "dated Entry hashes should differ when only the stamp differs"
        );

        // Their projections are the identical `State::Present("same-value")` and therefore hash
        // identically, regardless of which node/timestamp produced them.
        assert_eq!(early.project(), late.project());
        assert_eq!(hash_of(&early.project()), hash_of(&late.project()));

        // Same check for tombstones.
        let early_tomb: Entry<Timestamp, &str> = Entry::tombstone(Timestamp::new(1, 0, 0));
        let late_tomb: Entry<Timestamp, &str> = Entry::tombstone(Timestamp::new(2, 0, 0));
        assert_ne!(hash_of(&early_tomb), hash_of(&late_tomb));
        assert_eq!(
            hash_of(&early_tomb.project()),
            hash_of(&late_tomb.project())
        );
    }
}
