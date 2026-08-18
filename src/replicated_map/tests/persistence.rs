// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::persistence::{PersistedState, Persistence};
use crate::replica::version_hash;
use crate::{FileSnapshot, ReplicatedMap};

use super::ephemeral_config;

/// A durable backend must let a restarted store recover both live values and tombstones, with
/// identical timestamps (hence an identical fingerprint).
#[tokio::test]
async fn persistence_roundtrip_recovers_entries_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.insert(1, 11); // live value
    store.insert(2, 22);
    store.remove(&2); // tombstone
    let expected = store.fingerprint(..);
    store.snapshot(); // force a durable write

    // A brand-new store recovers the previous state from the same file.
    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert_eq!(restarted.get(&1).as_deref(), Some(&11));
    assert!(restarted.get(&2).is_none(), "tombstone was not recovered");
    assert_eq!(
        restarted.fingerprint(..),
        expected,
        "recovered state must hash identically (timestamps preserved)"
    );
    // The recovered tombstone is back in the expiry wheel (replayed through the hook).
    assert!(restarted.tombstones.remove(&2).is_some());
}

/// A [`Persistence`] backend that fails `load` with a given `io::ErrorKind` a fixed number of
/// times before succeeding with `Ok(None)` — simulates a transient environmental failure (a
/// not-yet-mounted volume, a momentary permission error) that clears up on its own.
struct FlakyLoad {
    kind: std::io::ErrorKind,
    failures_remaining: std::sync::atomic::AtomicU32,
}

impl<K: Send + Sync + 'static, V: Send + Sync + 'static> Persistence<K, V> for FlakyLoad {
    fn load(&self) -> std::io::Result<Option<PersistedState<K, V>>> {
        use std::sync::atomic::Ordering;
        if self
            .failures_remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(std::io::Error::new(
                self.kind,
                "simulated transient failure",
            ));
        }
        Ok(None)
    }
    fn save(&self, _state: &PersistedState<K, V>) -> std::io::Result<()> {
        Ok(())
    }
}

/// Doubles from `LOAD_RETRY_BASE_DELAY` each attempt, 1-indexed: attempt 1 is the base delay
/// itself, not one doubling of it.
#[test]
fn backoff_delay_doubles_from_the_base() {
    assert_eq!(
        super::super::persistence::backoff_delay(1),
        super::super::persistence::LOAD_RETRY_BASE_DELAY
    );
    assert_eq!(
        super::super::persistence::backoff_delay(2),
        super::super::persistence::LOAD_RETRY_BASE_DELAY * 2
    );
    assert_eq!(
        super::super::persistence::backoff_delay(3),
        super::super::persistence::LOAD_RETRY_BASE_DELAY * 4
    );
    assert_eq!(
        super::super::persistence::backoff_delay(4),
        super::super::persistence::LOAD_RETRY_BASE_DELAY * 8
    );
}

/// A transient load failure (anything but `InvalidData`) must be retried, not turned
/// into an immediate crash — a slow-mounting volume at boot must not crash-loop the process.
#[tokio::test]
async fn transient_load_failure_is_retried_not_fatal() {
    let backend = Arc::new(FlakyLoad {
        kind: std::io::ErrorKind::PermissionDenied,
        failures_remaining: std::sync::atomic::AtomicU32::new(
            super::super::persistence::LOAD_RETRY_ATTEMPTS - 1,
        ),
    });
    // Must not panic: the store construction below succeeds once the backend stops failing,
    // within the retry budget.
    let _store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(backend);
}

/// A load failure that exhausts the retry budget still panics — retrying is a bounded
/// mitigation for a transient hiccup, not a way to silently start fresh forever.
#[tokio::test]
#[should_panic(expected = "failed to load persisted state after")]
async fn load_failure_beyond_retry_budget_still_panics() {
    let backend = Arc::new(FlakyLoad {
        kind: std::io::ErrorKind::PermissionDenied,
        failures_remaining: std::sync::atomic::AtomicU32::new(
            super::super::persistence::LOAD_RETRY_ATTEMPTS,
        ),
    });
    let _store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(backend);
}

/// `InvalidData` (corrupt or incompatible format) must panic **immediately**, with no
/// retry — corruption does not clear up on its own, and retrying would only delay the loud
/// failure the doc comment promises.
#[tokio::test]
#[should_panic(expected = "persisted state is corrupt or from an incompatible format")]
async fn invalid_data_panics_without_retrying() {
    let backend = Arc::new(FlakyLoad {
        kind: std::io::ErrorKind::InvalidData,
        failures_remaining: std::sync::atomic::AtomicU32::new(1),
    });
    let start = std::time::Instant::now();
    let _store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(backend);
    // Unreachable on panic, but documents intent: this must not have gone through even one
    // retry backoff.
    assert!(start.elapsed() < super::super::persistence::LOAD_RETRY_BASE_DELAY);
}

/// `snapshot` clones the map in `SNAPSHOT_CHUNK_SIZE`-entry chunks, releasing and
/// re-acquiring the read lock between them. Insert enough entries to force several chunk
/// boundaries and confirm every one of them still round-trips — the chunking must not drop,
/// duplicate, or reorder entries relative to the previous whole-map-under-one-lock snapshot.
#[tokio::test]
async fn snapshot_across_multiple_chunks_recovers_every_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");
    let n = super::super::persistence::SNAPSHOT_CHUNK_SIZE * 2 + 17; // spans three chunks, last one partial

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    for k in 0..n as i32 {
        store.just_insert(k, k * 2);
    }
    let expected = store.fingerprint(..);
    store.snapshot();

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert_eq!(
        restarted.fingerprint(..),
        expected,
        "chunked snapshot must recover every entry across chunk boundaries"
    );
    for k in 0..n as i32 {
        assert_eq!(restarted.get(&k).as_deref(), Some(&(k * 2)));
    }
}

/// The causal-stability state (membership + per-tombstone acks) must survive a
/// restart, otherwise GC gating is lost.
#[tokio::test]
async fn restart_preserves_membership_and_acks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");
    let peer: IpAddr = "127.0.0.99".parse().unwrap();

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.engine.members.write().insert(peer);
    store.insert(5, 55);
    store.remove(&5); // tombstone
    store
        .engine
        .tombstone_acks
        .write()
        .entry(5)
        .or_default()
        .insert(peer, 123);
    store.snapshot();

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert!(
        restarted.engine.members.read().contains(&peer),
        "membership set was not restored"
    );
    assert_eq!(
        restarted
            .engine
            .tombstone_acks
            .read()
            .get(&5)
            .and_then(|acks| acks.get(&peer)),
        Some(&123),
        "tombstone acknowledgments were not restored"
    );
}

/// A restart must not turn a held-back tombstone into a collectable one: recovered
/// membership must keep gating GC.
#[tokio::test]
async fn restart_keeps_tombstone_gc_gated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");
    let peer: IpAddr = "127.0.0.98".parse().unwrap();

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.engine.members.write().insert(peer);
    store.insert(1, 11);
    store.remove(&1); // tombstone, never acknowledged by `peer`

    let version = store.engine.map.read().get(&1).map(version_hash).unwrap();
    assert!(
        !store.engine.is_tombstone_stable(&1, version),
        "precondition: tombstone is gated before restart"
    );
    store.snapshot();

    // Sanity check the hazard: a *fresh* store (no recovered membership) would consider the
    // same tombstone stable and collect it.
    let fresh = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    fresh.insert(1, 11);
    fresh.remove(&1);
    let fresh_version = fresh.engine.map.read().get(&1).map(version_hash).unwrap();
    assert!(
        fresh.engine.is_tombstone_stable(&1, fresh_version),
        "a fresh restart with no membership would (wrongly) GC the tombstone — the hazard this guards against"
    );

    // The recovered store keeps the tombstone gated, preventing resurrection.
    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert!(restarted.get(&1).is_none(), "tombstone was not recovered");
    let version = restarted
        .engine
        .map
        .read()
        .get(&1)
        .map(version_hash)
        .unwrap();
    assert!(
        !restarted.engine.is_tombstone_stable(&1, version),
        "restart dropped causal-stability state: tombstone would be GC'd and could resurrect"
    );
}

// ----- Observe-on-load (HLC monotonicity across restarts) tests -----

/// Loading persisted state must advance the clock past the maximum persisted stamp, so the
/// first post-restart write outranks every pre-restart one.
#[tokio::test]
async fn restart_clock_advanced_past_persisted_max_stamp() {
    use crate::clock::{Hlc, LogicalCounter, ManualClock, NodeId, PhysicalTime};
    use crate::persistence::{InMemoryPersistence, PersistedState};

    // Build a ManualClock starting at (physical=0, logical=0).
    let clock = Arc::new(ManualClock::new(NodeId::new(1)));

    // Craft a PersistedState whose only entry carries a stamp well ahead of the clock.
    // physical=100 is in the "future" relative to the ManualClock's (0, 0) starting state.
    let persisted_stamp = crate::clock::Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(100), LogicalCounter::new(0)),
        NodeId::new(1),
    );
    let backend = Arc::new(InMemoryPersistence::<i32, i32>::new());
    backend
        .save(&PersistedState::from(vec![(
            42,
            crate::entry::Entry::present(persisted_stamp, 999),
        )]))
        .unwrap();

    // Create a store with the ManualClock and load the persisted state.
    let store = ReplicatedMap::<i32, i32>::new_with_clock(
        ephemeral_config().with_node_id(NodeId::new(1)),
        clock,
    )
    .await
    .expect("bind failed")
    .with_persistence(backend);

    // Insert a new value; the minted timestamp must be strictly greater than persisted_stamp.
    store.insert(99, 1);

    // Read the stored timestamp for key 99 via the internal map.
    let minted_stamp = store
        .engine
        .map
        .read()
        .get(&99)
        .map(|entry| entry.stamp)
        .expect("key 99 must be present after insert");

    assert!(
        minted_stamp > persisted_stamp,
        "post-restart write timestamp {minted_stamp:?} is not strictly greater than the \
         persisted max {persisted_stamp:?}; the clock was not advanced on load"
    );
}

/// Same, where the maximum persisted stamp is on a **tombstone**: a post-restart insert must
/// outrank it, or a peer still holding the tombstone re-applies it via LWW.
#[tokio::test]
async fn restart_insert_beats_persisted_tombstone() {
    use crate::clock::{Hlc, LogicalCounter, ManualClock, NodeId, PhysicalTime};
    use crate::persistence::{InMemoryPersistence, PersistedState};

    // ManualClock starting at (physical=0, logical=0).
    let clock = Arc::new(ManualClock::new(NodeId::new(2)));

    // A tombstone with a stamp in the "future" relative to the cold clock.
    let tombstone_stamp = crate::clock::Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(200), LogicalCounter::new(0)),
        NodeId::new(2),
    );
    let backend = Arc::new(InMemoryPersistence::<i32, i32>::new());
    backend
        .save(&PersistedState::from(vec![(
            7,
            crate::entry::Entry::tombstone(tombstone_stamp), // tombstone
        )]))
        .unwrap();

    let store = ReplicatedMap::<i32, i32>::new_with_clock(
        ephemeral_config().with_node_id(NodeId::new(2)),
        clock,
    )
    .await
    .expect("bind failed")
    .with_persistence(backend);

    // The tombstone was recovered (key 7 is absent from the live view).
    assert!(
        store.get(&7).is_none(),
        "tombstone was not recovered: expected key 7 to be absent after loading"
    );

    // Insert a fresh value for key 7 and inspect the minted timestamp.
    store.insert(7, 42);
    let minted_stamp = store
        .engine
        .map
        .read()
        .get(&7)
        .map(|entry| entry.stamp)
        .expect("key 7 must be present after insert");

    // The minted stamp must be strictly greater than the tombstone's stamp. Without this,
    // a peer that still holds the tombstone would win the LWW merge during anti-entropy
    // and silently re-apply the tombstone, undoing the fresh insert.
    assert!(
        minted_stamp > tombstone_stamp,
        "post-restart insert timestamp {minted_stamp:?} is not strictly greater than the \
         persisted tombstone stamp {tombstone_stamp:?}; the clock was not advanced on load, \
         so a peer reconciling with this node could resurrect the tombstone via LWW"
    );
}

/// The periodic snapshot loop actually calls into the persistence backend after
/// `SNAPSHOT_INTERVAL` elapses — a mutant collapsing the loop body to a no-op would leave the
/// backend untouched no matter how long `run()` keeps going.
#[tokio::test]
async fn snapshot_periodically_actually_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("periodic.bin");
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.just_insert(1, 10);

    let _ = tokio::time::timeout(
        super::super::persistence::SNAPSHOT_INTERVAL + Duration::from_secs(1),
        store.snapshot_periodically(),
    )
    .await;

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert_eq!(
        restarted.get(&1).as_deref(),
        Some(&10),
        "no snapshot was written after a full SNAPSHOT_INTERVAL"
    );
}
