// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::hash::Hash;
use std::io;
use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::Entry;
use crate::persistence::{DatedEntries, PersistedState, Persistence};

use super::ReplicatedMap;

/// How often the background task writes a full snapshot to the persistence backend.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

/// Attempts [`with_persistence`](ReplicatedMap::with_persistence) makes to load persisted state
/// before giving up.
pub(super) const LOAD_RETRY_ATTEMPTS: u32 = 5;

/// Base delay before the first load retry; each subsequent attempt doubles it (see
/// [`backoff_delay`]) — 100 ms, 200 ms, 400 ms, 800 ms, under 2 s of total backoff across
/// [`LOAD_RETRY_ATTEMPTS`].
pub(super) const LOAD_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

/// Delay before retry `attempt` (1-indexed): `LOAD_RETRY_BASE_DELAY` doubled `attempt - 1` times.
pub(super) fn backoff_delay(attempt: u32) -> Duration {
    LOAD_RETRY_BASE_DELAY * 2u32.pow(attempt - 1)
}

/// Entries cloned per map read-lock acquisition while building a snapshot (`Self::snapshot`).
///
/// Cloning the whole map under one continuous read lock stalls every writer for as long as the
/// clone takes — proportional to map size, unbounded. Chunking bounds a single stall to
/// the time to clone this many entries, releasing the lock between chunks so a waiting writer can
/// interleave. The resulting snapshot is not a single linearizable instant — later chunks can
/// reflect writes concurrent with earlier ones — but that is no different from what the gossip
/// protocol itself already reconciles range-by-range, and each individual entry is still read
/// atomically (`ARCHITECTURE.md` §5 invariant 8's per-key LWW model needs no more).
pub(super) const SNAPSHOT_CHUNK_SIZE: usize = 4096;

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Plug in a durable persistence backend, **loading any previously saved state first**.
    ///
    /// Call between [`new`](ReplicatedMap::new) and [`run`](ReplicatedMap::run), so entries,
    /// tombstones and the causal-stability membership are recovered before the node rejoins gossip.
    /// Loaded entries replay through the pre-insert hook, preserving each tombstone's deletion
    /// timestamp and rebuilding the expiry wheel.
    ///
    /// # Panics
    ///
    /// If the backend fails to load: a damaged durable state must be an explicit decision, never a
    /// silent fresh start. A *transient* failure (anything other than
    /// [`InvalidData`](io::ErrorKind::InvalidData) — a not-yet-mounted volume, a momentary
    /// permission or I/O hiccup) is retried up to `LOAD_RETRY_ATTEMPTS` (5) times with exponential
    /// backoff before this panics, so a slow-starting environment does not crash-loop on every
    /// restart attempt; a decode/format error ([`InvalidData`](io::ErrorKind::InvalidData)) is
    /// never transient and panics immediately, unretried.
    pub fn with_persistence(mut self, backend: Arc<dyn Persistence<K, V>>) -> Self {
        // A random node id changes every restart, so the LWW tie-break is stable only within one
        // process lifetime — durable state wants an explicit `Config::with_node_id`.
        if self.engine.node_id_is_random() {
            warn!(
                "persistence is enabled but no stable node_id was configured \
                 (Config::with_node_id was not called). The node id is randomly generated on \
                 every start, so this node's LWW conflict-resolution identity changes across \
                 restarts. Conflicts between a pre-restart write and a post-restart write from \
                 the same node are resolved non-deterministically. Set a stable, unique \
                 Config::with_node_id to preserve consistent LWW ordering across restarts."
            );
        }
        let loaded = {
            let mut attempt = 0u32;
            loop {
                match backend.load() {
                    Ok(state) => break state,
                    Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                        panic!("persisted state is corrupt or from an incompatible format, refusing to silently start fresh: {err}");
                    }
                    Err(err) if attempt + 1 < LOAD_RETRY_ATTEMPTS => {
                        attempt += 1;
                        let delay = backoff_delay(attempt);
                        warn!(
                            "transient failure loading persisted state (attempt {attempt}/{LOAD_RETRY_ATTEMPTS}): \
                             {err}; retrying in {delay:?}"
                        );
                        std::thread::sleep(delay);
                    }
                    Err(err) => {
                        panic!(
                            "failed to load persisted state after {LOAD_RETRY_ATTEMPTS} attempts: {err}"
                        );
                    }
                }
            }
        };
        if let Some(state) = loaded {
            *self.engine.members.write() = state.members;
            *self.engine.tombstone_acks.write() = state.tombstone_acks;
            // Advance past every persisted stamp, or a fresh write can lose LWW to this node's
            // own older value after a backward clock step. Trusted path: these stamps are
            // self-authored, and the clamp would refuse to chase them in exactly that scenario.
            for (_, entry) in &state.entries {
                self.engine.clock_observe_trusted(entry.stamp);
            }
            // Replay through the wrapped hook: the public insert helpers would re-stamp.
            self.engine.just_insert_bulk(&state.entries);
        }
        self.persistence = backend;
        self
    }

    /// Capture the full store state and hand it to the persistence backend.
    ///
    /// Clones the map in [`SNAPSHOT_CHUNK_SIZE`]-entry chunks, releasing the read lock between
    /// chunks, rather than holding it for one continuous `O(map size)` clone — see
    /// [`SNAPSHOT_CHUNK_SIZE`]'s doc for why a non-instantaneous snapshot is an acceptable
    /// trade-off here.
    pub(super) fn snapshot(&self) {
        let mut entries: DatedEntries<K, V> = Vec::new();
        let mut cursor: Option<K> = None;
        loop {
            let guard = self.engine.map.read();
            let chunk: Vec<(K, Entry<Timestamp, V>)> = match &cursor {
                None => guard
                    .range(..)
                    .take(SNAPSHOT_CHUNK_SIZE)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                Some(last) => guard
                    .range((Bound::Excluded(last.clone()), Bound::Unbounded))
                    .take(SNAPSHOT_CHUNK_SIZE)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };
            drop(guard);
            let Some((last_key, _)) = chunk.last() else {
                break;
            };
            cursor = Some(last_key.clone());
            entries.extend(chunk);
        }
        let state = PersistedState::new(
            entries,
            self.engine.members.read().clone(),
            self.engine.tombstone_acks.read().clone(),
        );
        if let Err(err) = self.persistence.save(&state) {
            warn!("failed to persist reconcile store snapshot: {err}");
        }
    }

    /// Periodically snapshot the full store state to the persistence backend.
    pub(super) async fn snapshot_periodically(&self) {
        loop {
            tokio::time::sleep(SNAPSHOT_INTERVAL).await;
            self.snapshot();
        }
    }
}
