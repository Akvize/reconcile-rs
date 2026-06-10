// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Durability for a [`ReconcileStore`](crate::ReconcileStore).
//!
//! A [`ReconcileStore`](crate::ReconcileStore) **always** owns a persistence backend: the
//! [`Persistence`] trait is mandatory, not opt-in. What varies is *which* backend is plugged in.
//! The default is [`InMemoryPersistence`], which keeps the latest snapshot in RAM and therefore
//! **loses everything when the process restarts** — i.e. the historical, pre-persistence behaviour.
//! Swapping in a durable backend such as [`FileSnapshot`] via
//! [`ReconcileStore::with_persistence`](crate::ReconcileStore::with_persistence) makes a restart
//! recover the previous state instead of looking like a brand-new replica.
//!
//! Why this matters: a node that restarts with an empty map loses its **tombstones** too. Losing
//! tombstones is not just a durability problem — it is a correctness multiplier for tombstone
//! resurrection: the restarted node behaves like a fresh replica, re-learns
//! already-deleted values from peers, and can re-propagate them. A durable backend recovers the
//! tombstones (and the causal-stability state) before the node rejoins the gossip protocol.
//!
//! # What is persisted
//!
//! - **All map entries**, live values *and* tombstones (the map stores `(timestamp, Option<V>)`,
//!   and tombstones are `(timestamp, None)` retained until causal-stability-gated GC).
//! - The **causal-stability state**: the membership set and the per-tombstone
//!   acknowledgments, so a restarted node still holds back GC until every replica has seen a
//!   deletion.
//!
//! The tombstone-expiry timeout wheel is **not** persisted separately: replaying the entries
//! through the store's pre-insert hook rebuilds it, preserving each tombstone's original deletion
//! timestamp.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;

/// The store's keyed entries in their internal dated, tombstone-aware form: each key maps to a
/// `(timestamp, Option<V>)`, where a `None` payload is a tombstone.
pub type DatedEntries<K, V> = Vec<(K, (Timestamp, Option<V>))>;

/// A snapshot of everything a [`ReconcileStore`](crate::ReconcileStore) needs to durably survive a
/// restart without behaving like a fresh replica.
///
/// `V` is the user value type; entries store the internal dated, tombstone-aware representation
/// `(Timestamp, Option<V>)`.
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

/// A pluggable durable backend for a [`ReconcileStore`](crate::ReconcileStore).
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
/// behaviour of a store with no on-disk durability. Use [`FileSnapshot`] (or another durable
/// backend) when a restart must recover the previous state.
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
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()> {
        *self.state.lock().unwrap() = Some(state.clone());
        Ok(())
    }
}

/// A durable, file-based [`Persistence`] backend storing a single bincode-encoded snapshot.
///
/// Saves are **atomic**: the snapshot is written to a sibling `*.tmp` file, flushed, then renamed
/// over the target path, so a crash mid-write never corrupts a previously good snapshot.
#[derive(Clone, Debug)]
pub struct FileSnapshot {
    path: PathBuf,
}

impl FileSnapshot {
    /// Create a backend that reads from and writes to `path`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        FileSnapshot {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn tmp_path(&self) -> PathBuf {
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    }
}

impl<K, V> Persistence<K, V> for FileSnapshot
where
    K: Serialize + DeserializeOwned + Eq + std::hash::Hash + Send + Sync + 'static,
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let state = bincode::deserialize(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(Some(state))
    }

    fn save(&self, state: &PersistedState<K, V>) -> io::Result<()> {
        let bytes = bincode::serialize(state)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let tmp = self.tmp_path();
        // Write to a temporary file, flush it, then atomically rename over the target so a crash
        // mid-write cannot corrupt a previously good snapshot.
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                (1, (Timestamp::new(1_000, 0, 7), Some("alive".to_string()))),
                (2, (Timestamp::new(2_000, 1, 7), None)), // tombstone
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
    fn persisted_state_bincode_roundtrip() {
        let state = sample_state();
        let bytes = bincode::serialize(&state).unwrap();
        let back: PersistedState<i32, String> = bincode::deserialize(&bytes).unwrap();
        assert_states_eq(&back, &state);
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
        first.entries = vec![(1, (Timestamp::new(1, 0, 0), Some("first".to_string())))];
        backend.save(&first).unwrap();

        let mut second = sample_state();
        second.entries = vec![(1, (Timestamp::new(2, 0, 0), Some("second".to_string())))];
        backend.save(&second).unwrap();

        let loaded = backend.load().unwrap().unwrap();
        assert_eq!(loaded.entries[0].1 .1, Some("second".to_string()));
    }

    #[test]
    fn file_snapshot_save_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileSnapshot::new(dir.path().join("snapshot.bin"));

        // Nothing saved yet.
        assert!(Persistence::<i32, String>::load(&backend)
            .unwrap()
            .is_none());

        let state = sample_state();
        Persistence::<i32, String>::save(&backend, &state).unwrap();

        let loaded = Persistence::<i32, String>::load(&backend)
            .unwrap()
            .expect("a snapshot was saved");
        assert_states_eq(&loaded, &state);
    }

    #[test]
    fn file_snapshot_save_is_atomic_replace() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileSnapshot::new(dir.path().join("snapshot.bin"));

        let mut first = sample_state();
        first.entries = vec![(1, (Timestamp::new(1, 0, 0), Some("first".to_string())))];
        Persistence::<i32, String>::save(&backend, &first).unwrap();

        let mut second = sample_state();
        second.entries = vec![(1, (Timestamp::new(2, 0, 0), Some("second".to_string())))];
        Persistence::<i32, String>::save(&backend, &second).unwrap();

        // No leftover temporary file, and the latest snapshot wins.
        assert!(!dir.path().join("snapshot.bin.tmp").exists());
        let loaded = Persistence::<i32, String>::load(&backend).unwrap().unwrap();
        assert_eq!(loaded.entries[0].1 .1, Some("second".to_string()));
    }
}
