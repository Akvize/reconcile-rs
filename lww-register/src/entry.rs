// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The [`Entry`] / [`State`] domain types: `ARCHITECTURE.md` §4.
//!
//! [`Entry`] is the stored cell and [`Entry::merge`] the LWW policy; [`State<V>`] is its
//! timestamp-less projection, which a dateless `ReadReplicaMap` stores directly.
//!
//! The two summaries stay distinct **structurally** — [`Entry`] has a `stamp` field, [`State`] has
//! none — so every field-by-field content summary inherits it (`ARCHITECTURE.md` §5 invariant 8).

use serde::{Deserialize, Serialize};

/// A timestamp-less projection of a value: a live value or a tombstone.
///
/// Isomorphic to `Option<V>`, but carrying no [`Timestamp`](crate::clock::Timestamp) field there
/// is none to include in a content summary.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum State<V> {
    /// A live value.
    Present(V),
    /// No live value: the key was deleted (or never inserted).
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

    /// `true` if this is a tombstone (no live value).
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

/// A stored cell: a value (or tombstone) stamped with a conflict-resolution token `T`, in practice
/// always [`Timestamp`](crate::clock::Timestamp).
///
/// ```
/// use lww_register::Entry;
/// use lww_register::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
///
/// let stamp_at = |ms| Timestamp::new(Hlc::new(PhysicalTime::from_millis(ms), LogicalCounter::new(0)), NodeId::new(1));
///
/// let older = Entry::present(stamp_at(100), "a");
/// let newer = Entry::present(stamp_at(200), "b");
///
/// // Last-write-wins: the entry with the strictly greater stamp wins, regardless of merge order.
/// assert_eq!(older.merge(&newer).value(), Some(&"b"));
/// assert_eq!(newer.merge(&older).value(), Some(&"b"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry<T, V> {
    /// The conflict-resolution stamp.
    pub stamp: T,
    /// The live value, or a tombstone.
    pub state: State<V>,
}

impl<T, V> Entry<T, V> {
    /// Build a live entry: `value`, stamped with `stamp`.
    pub fn present(stamp: T, value: V) -> Self {
        Entry {
            stamp,
            state: State::Present(value),
        }
    }

    /// Build a tombstone entry stamped with `stamp` — no live value.
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

    /// `true` if this entry is a tombstone (no live value).
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
    /// Project to the timestamp-less [`State<V>`] a `ReadReplicaMap` converges on
    /// (`ARCHITECTURE.md` §5 invariant 8).
    pub fn project(&self) -> State<V> {
        self.state.clone()
    }

    /// Last-write-wins: the entry with the strictly greater `stamp` wins.
    ///
    /// `max` over a total order, hence commutative, associative and idempotent.
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
    use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<H: Hash>(value: &H) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn merge_is_commutative_on_equal_wall_and_counter() {
        let a = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(100), LogicalCounter::new(0)),
                NodeId::new(1),
            ),
            "a",
        );
        let b = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(100), LogicalCounter::new(0)),
                NodeId::new(2),
            ),
            "b",
        );
        assert_eq!(a.merge(&b), b.merge(&a));
        assert_eq!(a.merge(&b).value(), Some(&"b"));
    }

    #[test]
    fn merge_is_idempotent() {
        let a = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(7), LogicalCounter::new(3)),
                NodeId::new(42),
            ),
            "x",
        );
        assert_eq!(a.merge(&a), a);
    }

    #[test]
    fn merge_picks_the_greater_stamp() {
        let older = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(10), LogicalCounter::new(0)),
                NodeId::new(5),
            ),
            "old",
        );
        let newer = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(11), LogicalCounter::new(0)),
                NodeId::new(1),
            ),
            "new",
        );
        assert_eq!(older.merge(&newer).value(), Some(&"new"));
        assert_eq!(newer.merge(&older).value(), Some(&"new"));
    }

    #[test]
    fn is_tombstone_reflects_state() {
        let live = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            42,
        );
        let dead: Entry<Timestamp, i32> = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        assert!(!live.is_tombstone());
        assert_eq!(live.value(), Some(&42));
        assert!(dead.is_tombstone());
        assert_eq!(dead.value(), None);
    }

    #[test]
    fn project_is_isomorphic_to_option() {
        let live = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            "v",
        );
        let dead: Entry<Timestamp, &str> = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        assert_eq!(live.project(), State::Present("v"));
        assert_eq!(dead.project(), State::Tombstone);
    }

    /// `ARCHITECTURE.md` §5 invariant 8, through the derived `Hash` as a stand-in for the real
    /// fingerprint walk — this crate knows nothing of `rsos`.
    #[test]
    fn projection_hash_is_timestamp_independent_but_entry_hash_is_not() {
        let early = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            "same-value",
        );
        let late = Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            "same-value",
        );

        assert_ne!(early, late);
        assert_ne!(
            hash_of(&early),
            hash_of(&late),
            "dated Entry hashes should differ when only the stamp differs"
        );

        assert_eq!(early.project(), late.project());
        assert_eq!(hash_of(&early.project()), hash_of(&late.project()));

        let early_tomb: Entry<Timestamp, &str> = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        let late_tomb: Entry<Timestamp, &str> = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        assert_ne!(hash_of(&early_tomb), hash_of(&late_tomb));
        assert_eq!(
            hash_of(&early_tomb.project()),
            hash_of(&late_tomb.project())
        );
    }
}
