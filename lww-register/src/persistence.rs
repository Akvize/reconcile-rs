// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Durability for a replicated map: the [`Persistence`] port and its snapshot value type.
//!
//! A `ReplicatedMap` **always** owns a persistence backend: the [`Persistence`] trait is mandatory,
//! not opt-in. What varies is *which* backend is plugged in. The default is
//! [`InMemoryPersistence`], which keeps the latest snapshot in RAM and therefore **loses everything
//! when the process restarts** — i.e. the historical, pre-persistence behaviour. Swapping in a
//! durable backend such as `reconcile::FileSnapshot` via `ReplicatedMap::with_persistence` makes a
//! restart recover the previous state instead of looking like a brand-new replica.
//!
//! Why this matters: a node that restarts with an empty map loses its **tombstones** too. Losing
//! tombstones is not just a durability problem — it is a correctness multiplier for tombstone
//! resurrection: the restarted node behaves like a fresh replica, re-learns
//! already-deleted values from peers, and can re-propagate them. A durable backend recovers the
//! tombstones (and the causal-stability state) before the node rejoins the gossip protocol.
//!
//! # What is persisted
//!
//! - **All map entries**, live values *and* tombstones (the map stores
//!   [`Entry<Timestamp, V>`](crate::entry::Entry), and a tombstone is an entry whose
//!   [`state`](crate::entry::Entry::state) is [`State::Tombstone`](crate::entry::State::Tombstone),
//!   retained until causal-stability-gated GC).
//! - The **causal-stability state**: the membership set and the per-tombstone
//!   acknowledgments, so a restarted node still holds back GC until every replica has seen a
//!   deletion.
//!
//! The tombstone-expiry timeout wheel is **not** persisted separately: replaying the entries
//! through the store's pre-insert hook rebuilds it, preserving each tombstone's original deletion
//! timestamp.
//!
//! # What lives here, and what does not
//!
//! Everything in this module is infrastructure-free: the port, the snapshot value type, and the
//! in-memory default that trivially implements the port it sits next to. The file-backed adapter —
//! `reconcile::FileSnapshot`, with its versioned on-disk header — touches `std::fs` and a wire
//! codec, so it cannot live here at all: this crate's manifest declares neither, and the domain-purity
//! gate keeps it that way. It sits on the adapter side, in `reconcile`'s `src/snapshot.rs`. Keeping
//! [`InMemoryPersistence`] here means the default backend costs no extra crate.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::IpAddr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::entry::Entry;

/// The store's keyed entries in their internal dated, tombstone-aware form: each key maps to an
/// [`Entry<Timestamp, V>`], whose [`State::Tombstone`](crate::entry::State::Tombstone) marks a
/// deletion.
pub type DatedEntries<K, V> = Vec<(K, Entry<Timestamp, V>)>;

/// A snapshot of everything a replicated map needs to durably survive a restart without behaving
/// like a fresh replica.
///
/// `V` is the user value type; entries store the internal dated, tombstone-aware representation
/// `Entry<Timestamp, V>`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(bound(
    serialize = "K: Serialize, V: Serialize",
    deserialize = "K: Deserialize<'de> + Eq + std::hash::Hash, V: Deserialize<'de>"
))]
pub struct PersistedState<K, V> {
    /// Every key with its dated value. A `None` payload is a tombstone.
    pub entries: DatedEntries<K, V>,
    /// Every peer this node has ever communicated with (causal-stability membership).
    pub members: HashSet<IpAddr>,
    /// Per-tombstone acknowledgments: `key -> (peer -> version token of the tombstone it holds)`.
    pub tombstone_acks: HashMap<K, HashMap<IpAddr, u64>>,
}

/// A pluggable durable backend for a replicated map.
///
/// Every store owns one (the trait is mandatory); [`InMemoryPersistence`] is the non-durable
/// default. Implementations must be cheap to share across tasks (`Send + Sync + 'static`) since the
/// store holds the backend behind an [`Arc`](std::sync::Arc) and snapshots from a background task.
pub trait Persistence<K, V>: Send + Sync + 'static {
    /// Load the previously saved state, or `Ok(None)` if nothing was ever saved.
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>>;
    /// Durably save the given state, atomically replacing any previous snapshot.
    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()>;
}

/// The **default** persistence backend: keeps the latest snapshot in RAM.
///
/// Within a running process a save followed by a load round-trips faithfully, but the snapshot
/// lives only in memory, so **a process restart loses everything** — exactly the historical
/// behaviour of a store with no on-disk durability. Use `reconcile::FileSnapshot` (or another
/// durable backend) when a restart must recover the previous state.
pub struct InMemoryPersistence<K, V> {
    state: Mutex<Option<PersistedState<K, V>>>,
}

impl<K, V> Default for InMemoryPersistence<K, V> {
    fn default() -> Self {
        InMemoryPersistence {
            state: Mutex::new(None),
        }
    }
}

impl<K, V> InMemoryPersistence<K, V> {
    /// Create an empty in-memory backend.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<K, V> Persistence<K, V> for InMemoryPersistence<K, V>
where
    K: Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Returns the last state saved via [`save`](Self::save), or `Ok(None)` if `save` was never
    /// called.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (i.e. a previous `load`/`save` panicked while
    /// holding it).
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>> {
        Ok(self.state.lock().unwrap().clone())
    }

    /// Replaces the in-memory snapshot with `state`.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned (i.e. a previous `load`/`save` panicked while
    /// holding it).
    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()> {
        *self.state.lock().unwrap() = Some(state.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime};

    /// A representative snapshot: a live entry, a tombstone, two members, one ack.
    ///
    /// Shared with `reconcile`'s `src/snapshot.rs` tests only by duplication — the two sides test
    /// different halves of the split (the port here, the on-disk format there), and a shared
    /// fixture would mean exporting test-only construction from this crate.
    fn sample_state() -> PersistedState<i32, String> {
        let mut members = HashSet::new();
        members.insert("127.0.0.1".parse().unwrap());
        members.insert("127.0.0.2".parse().unwrap());

        let mut acks = HashMap::new();
        let mut key_acks = HashMap::new();
        key_acks.insert("127.0.0.1".parse().unwrap(), 42u64);
        acks.insert(7, key_acks);

        PersistedState {
            entries: vec![
                (
                    1,
                    Entry::present(
                        Timestamp::new(
                            Hlc::new(PhysicalTime::from_millis(1_000), LogicalCounter::new(0)),
                            NodeId::new(7),
                        ),
                        "alive".to_string(),
                    ),
                ),
                (
                    2,
                    Entry::tombstone(Timestamp::new(
                        Hlc::new(PhysicalTime::from_millis(2_000), LogicalCounter::new(1)),
                        NodeId::new(7),
                    )),
                ),
            ],
            members,
            tombstone_acks: acks,
        }
    }

    fn assert_states_eq(a: &PersistedState<i32, String>, b: &PersistedState<i32, String>) {
        assert_eq!(a.entries, b.entries);
        assert_eq!(a.members, b.members);
        assert_eq!(a.tombstone_acks, b.tombstone_acks);
    }

    #[test]
    fn in_memory_roundtrips_within_process() {
        let backend = InMemoryPersistence::<i32, String>::new();
        assert!(backend.load().unwrap().is_none());

        let state = sample_state();
        backend.save(&state).unwrap();
        assert_states_eq(&backend.load().unwrap().unwrap(), &state);
    }

    #[test]
    fn in_memory_save_replaces_previous() {
        let backend = InMemoryPersistence::<i32, String>::new();

        let mut first = sample_state();
        first.entries = vec![(
            1,
            Entry::present(
                Timestamp::new(
                    Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                    NodeId::new(0),
                ),
                "first".to_string(),
            ),
        )];
        backend.save(&first).unwrap();

        let mut second = sample_state();
        second.entries = vec![(
            1,
            Entry::present(
                Timestamp::new(
                    Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
                    NodeId::new(0),
                ),
                "second".to_string(),
            ),
        )];
        backend.save(&second).unwrap();

        let loaded = backend.load().unwrap().unwrap();
        assert_eq!(loaded.entries[0].1.value(), Some(&"second".to_string()));
    }
}
