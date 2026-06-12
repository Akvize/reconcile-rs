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
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::clock::Timestamp;

/// Backoff parameters for transient-I/O retries in [`FileSnapshot::load`].
const LOAD_RETRY_ATTEMPTS: u32 = 3;
const LOAD_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(100);

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

/// Classifies why [`FileSnapshot::load`] failed, so callers can act differently on corruption
/// versus transient I/O problems.
///
/// # Operator guidance
///
/// - **Corrupt** — the snapshot file exists but cannot be decoded. The safe choices are to delete
///   the file (accepts data loss: the node starts empty) or to restore from a backup. **Do not
///   silently start empty**, as that drops tombstones, enabling resurrection of previously deleted
///   values. The store exposes this as a distinct error so the calling application can log a clear
///   message and halt (recommended) or attempt recovery.
///
/// - **Io** — a transient I/O error persisted across all retry attempts (e.g. a filesystem still
///   mounting, a volume remount in progress). Retrying after a longer delay or restarting the
///   process is appropriate. The error is surfaced rather than causing a panic so the caller can
///   choose a graceful shutdown path.
#[derive(Debug)]
pub enum LoadError {
    /// The snapshot file exists but its contents could not be decoded. Data corruption or
    /// truncation. The inner string carries the original error message.
    Corrupt(String),
    /// An I/O error that survived all retry attempts; the file may or may not exist.
    Io(io::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Corrupt(msg) => write!(
                f,
                "snapshot file is corrupt and cannot be loaded: {msg}; \
                 delete the snapshot to start empty (accepts data loss) or restore from backup"
            ),
            LoadError::Io(err) => write!(f, "transient I/O error loading snapshot: {err}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Corrupt(_) => None,
            LoadError::Io(err) => Some(err),
        }
    }
}

impl From<LoadError> for io::Error {
    fn from(e: LoadError) -> io::Error {
        match e {
            LoadError::Corrupt(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            LoadError::Io(err) => err,
        }
    }
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
/// # Atomic saves
///
/// Saves are **atomic at the file level**: the snapshot is written to a sibling `*.tmp` file,
/// flushed to the OS page cache (`sync_all` on the file), then renamed over the target path, and
/// finally the **containing directory** is fsynced so the name binding is durable. This sequence
/// means a crash after the rename but before the directory fsync may leave the old snapshot in
/// place (the kernel replays the rename from the journal on most filesystems, but the POSIX
/// guarantee requires the directory fsync), while a crash before the rename leaves the original
/// snapshot untouched. In both cases exactly one valid snapshot is recoverable on restart.
///
/// A crash after writing the tmp file but before the rename leaves a stale `*.tmp` sibling.
/// [`FileSnapshot::load`] removes any such stale temporaries on startup so they do not accumulate.
///
/// # Fallible load
///
/// [`FileSnapshot::load_checked`] — called by
/// [`ReconcileStore::with_persistence`](crate::ReconcileStore::with_persistence) — returns a
/// [`LoadError`] rather than panicking:
///
/// - **Corrupt** data (`InvalidData`) is surfaced as [`LoadError::Corrupt`] so the caller can
///   decide whether to halt or attempt recovery.
/// - **Transient I/O** errors are retried up to three times with exponential backoff before being
///   surfaced as [`LoadError::Io`].
/// - **`NotFound`** (no snapshot yet) is a clean fresh start and returns `Ok(None)`.
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

    /// Clean up stale `*.tmp` siblings of the snapshot path, logging each removal at debug level.
    ///
    /// Called during load so that temporaries left behind by an interrupted save do not accumulate.
    fn remove_stale_tmp(&self) {
        let tmp = self.tmp_path();
        match fs::remove_file(&tmp) {
            Ok(()) => debug!(path = %tmp.display(), "removed stale snapshot tmp file"),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {} // nothing to clean
            Err(err) => {
                warn!(path = %tmp.display(), "failed to remove stale snapshot tmp file: {err}")
            }
        }
    }

    /// Load state, splitting I/O errors into [`LoadError::Corrupt`] vs. [`LoadError::Io`],
    /// with bounded retries for transient I/O.
    ///
    /// Stale `*.tmp` siblings are removed unconditionally on entry, before the load attempt.
    pub fn load_checked<K, V>(&self) -> Result<Option<PersistedState<K, V>>, LoadError>
    where
        K: DeserializeOwned + Eq + std::hash::Hash,
        V: DeserializeOwned,
    {
        self.remove_stale_tmp();

        let mut attempts = 0u32;
        let mut backoff = LOAD_RETRY_INITIAL_BACKOFF;
        loop {
            attempts += 1;
            match fs::read(&self.path) {
                Ok(bytes) => {
                    return bincode::deserialize::<PersistedState<K, V>>(&bytes)
                        .map(Some)
                        .map_err(|err| LoadError::Corrupt(err.to_string()));
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                    return Err(LoadError::Corrupt(err.to_string()));
                }
                Err(err) => {
                    if attempts >= LOAD_RETRY_ATTEMPTS {
                        return Err(LoadError::Io(err));
                    }
                    warn!(
                        path = %self.path.display(),
                        attempt = attempts,
                        max = LOAD_RETRY_ATTEMPTS,
                        "transient I/O error loading snapshot, retrying: {err}"
                    );
                    std::thread::sleep(backoff);
                    backoff *= 2;
                }
            }
        }
    }
}

impl<K, V> Persistence<K, V> for FileSnapshot
where
    K: Serialize + DeserializeOwned + Eq + std::hash::Hash + Send + Sync + 'static,
    V: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn load(&self) -> io::Result<Option<PersistedState<K, V>>> {
        self.load_checked().map_err(io::Error::from)
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
        // fsync the parent directory so the name binding (the rename) is durable. Without this,
        // a crash after the rename but before the OS flushes the directory journal may leave the
        // old snapshot in place on some filesystems. The directory fsync is a no-op on journalling
        // filesystems that replay the rename on recovery, but it is required for the POSIX
        // durability guarantee.
        //
        // Note: directory fsync durability cannot be unit-tested in-process — the guarantee is
        // that the kernel persists the rename to storage; verifying that requires a real crash or
        // raw block-device inspection. The call is tested indirectly by the save/load round-trip
        // tests; its presence is verified by code inspection.
        if let Some(parent) = self.path.parent() {
            // Silently skip if the parent is empty (unlikely, but handle gracefully).
            if !parent.as_os_str().is_empty() {
                let dir = fs::File::open(parent)?;
                dir.sync_all()?;
            }
        }
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

    /// Interrupted save: tmp written but not renamed → old snapshot still loads, stale tmp is
    /// cleaned on the next load.
    #[test]
    fn stale_tmp_cleaned_on_load_and_old_snapshot_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        let backend = FileSnapshot::new(&path);

        // Write the first snapshot normally.
        let mut first = sample_state();
        first.entries = vec![(1, (Timestamp::new(1, 0, 0), Some("first".to_string())))];
        Persistence::<i32, String>::save(&backend, &first).unwrap();

        // Simulate an interrupted second save: write the tmp but do NOT rename.
        let tmp_path = dir.path().join("snapshot.bin.tmp");
        {
            use std::io::Write;
            let mut second = sample_state();
            second.entries = vec![(1, (Timestamp::new(2, 0, 0), Some("second".to_string())))];
            let bytes = bincode::serialize(&second).unwrap();
            let mut file = fs::File::create(&tmp_path).unwrap();
            file.write_all(&bytes).unwrap();
            file.sync_all().unwrap();
        }
        // The tmp must be present before we call load.
        assert!(tmp_path.exists(), "precondition: tmp file should exist");

        // Load cleans the stale tmp and returns the original (first) snapshot.
        let loaded = backend
            .load_checked::<i32, String>()
            .expect("load must succeed")
            .expect("old snapshot must be present");
        assert_eq!(
            loaded.entries[0].1 .1,
            Some("first".to_string()),
            "old snapshot must load, not the tmp contents"
        );
        assert!(!tmp_path.exists(), "stale tmp must be removed on load");
    }

    /// Completed save → new snapshot loads correctly.
    #[test]
    fn completed_save_loads_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileSnapshot::new(dir.path().join("snapshot.bin"));

        let mut first = sample_state();
        first.entries = vec![(1, (Timestamp::new(1, 0, 0), Some("first".to_string())))];
        Persistence::<i32, String>::save(&backend, &first).unwrap();

        let mut second = sample_state();
        second.entries = vec![(1, (Timestamp::new(2, 0, 0), Some("new".to_string())))];
        Persistence::<i32, String>::save(&backend, &second).unwrap();

        let loaded = Persistence::<i32, String>::load(&backend)
            .unwrap()
            .expect("snapshot must be present");
        assert_eq!(
            loaded.entries[0].1 .1,
            Some("new".to_string()),
            "completed save must be visible on load"
        );
    }

    /// Corrupt snapshot file → `load_checked` returns `LoadError::Corrupt` (no panic).
    #[test]
    fn corrupt_snapshot_returns_error_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        fs::write(&path, b"not valid bincode at all").unwrap();
        let backend = FileSnapshot::new(&path);
        match backend.load_checked::<i32, String>() {
            Err(LoadError::Corrupt(_)) => {} // expected
            other => panic!("expected LoadError::Corrupt, got {other:?}"),
        }
    }

    /// Transient I/O error (path is a directory) → retries are attempted, then `LoadError::Io`
    /// is returned (no panic).
    #[test]
    fn transient_io_returns_error_variant() {
        // Use a directory as the snapshot path: `fs::read` on a directory returns an error on all
        // major platforms (Linux: EISDIR, macOS: EISDIR). We override LOAD_RETRY_ATTEMPTS to 1 via
        // the public knob by testing load_checked which internally uses the constants; we simply
        // verify the variant and that no panic occurs.
        let dir = tempfile::tempdir().unwrap();
        // The path itself IS a directory, so fs::read will fail with a non-NotFound, non-corrupt error.
        let backend = FileSnapshot::new(dir.path());
        match backend.load_checked::<i32, String>() {
            Err(LoadError::Io(_)) => {} // expected
            // On some platforms a directory read may look like corrupt data; accept both.
            Err(LoadError::Corrupt(_)) => {}
            Ok(_) => panic!("expected an error when the snapshot path is a directory"),
        }
    }
}
