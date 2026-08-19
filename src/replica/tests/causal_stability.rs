// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::IpAddr;

use bincode::{DefaultOptions, Deserializer};
use serde::Deserialize;

use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::entry::{Entry, State};
use crate::replica::{version_hash, Message, Replica};
use crate::replicated_map::Config;

type Tombstoned = Entry<Timestamp, i32>;

async fn engine(addr: &str) -> Replica<i32, i32> {
    let config = Config::default()
        .with_port(8080)
        .with_listen_addr(addr.parse().unwrap())
        .with_insecure_no_key();
    Replica::new(config).await.expect("bind failed")
}

#[tokio::test]
async fn tombstone_not_stable_until_all_members_ack() {
    let eng = engine("127.0.0.60").await;
    let peer_a: IpAddr = "127.0.0.61".parse().unwrap();
    let peer_b: IpAddr = "127.0.0.62".parse().unwrap();
    let key = 7;
    let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
        NodeId::new(0),
    ));
    let version = version_hash(&tombstone);

    // No member known yet: nothing could resurrect the value, so GC is allowed.
    assert!(eng.is_tombstone_stable(&key, version));

    // Two members known, neither has acknowledged: not stable.
    eng.members.write().insert(peer_a);
    eng.members.write().insert(peer_b);
    assert!(!eng.is_tombstone_stable(&key, version));

    // Only one member acknowledges: still not stable.
    eng.tombstone_acks
        .write()
        .entry(key)
        .or_default()
        .insert(peer_a, version);
    assert!(!eng.is_tombstone_stable(&key, version));

    // A stale acknowledgment (wrong version) from the other member does not count.
    eng.tombstone_acks
        .write()
        .entry(key)
        .or_default()
        .insert(peer_b, version.wrapping_add(1));
    assert!(!eng.is_tombstone_stable(&key, version));

    // The correct acknowledgment from every member makes it stable.
    eng.tombstone_acks
        .write()
        .entry(key)
        .or_default()
        .insert(peer_b, version);
    assert!(eng.is_tombstone_stable(&key, version));
}

/// Every reconciliation round must resend an `Ack` for each tombstone the node currently
/// holds, so the causal-stability ack matrix keeps converging at three or more nodes (where a
/// held tombstone is otherwise never re-advertised once two replicas agree on it).
/// Deterministic, socket-free: insert a tombstone, run one round, and inspect the datagram.
#[tokio::test]
async fn reconciliation_round_resends_acks_for_held_tombstones() {
    let eng = engine("127.0.0.66").await;
    let live_key = 1;
    let tombstone_key = 2;
    // A live value's ack must NOT be resent; a tombstone's must be.
    eng.just_insert(
        live_key,
        Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            11,
        ),
    );
    let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
        NodeId::new(0),
    ));
    let expected_version = version_hash(&tombstone);
    eng.just_insert(tombstone_key, tombstone);

    let mut buf = Vec::new();
    eng.start_reconciliation(&mut buf).await;

    // Decode every message in the datagram and collect the acks.
    let mut acks: Vec<(i32, u64)> = Vec::new();
    let mut de = Deserializer::from_slice(&buf, DefaultOptions::new());
    while let Ok(msg) = Message::<i32, Tombstoned, State<i32>>::deserialize(&mut de) {
        if let Message::Ack(ack) = msg {
            acks.push(ack);
        }
    }

    assert!(
        acks.contains(&(tombstone_key, expected_version)),
        "the round must resend the ack for the held tombstone (key {tombstone_key}); got \
         {acks:?}"
    );
    assert!(
        !acks.iter().any(|(k, _)| *k == live_key),
        "a live value's ack must never be resent; got {acks:?}"
    );
}

#[tokio::test]
async fn decommission_releases_a_silent_peer() {
    let eng = engine("127.0.0.63").await;
    let live: IpAddr = "127.0.0.64".parse().unwrap();
    let gone: IpAddr = "127.0.0.65".parse().unwrap();
    let key = 9;
    let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
        NodeId::new(0),
    ));
    let version = version_hash(&tombstone);

    eng.members.write().insert(live);
    eng.members.write().insert(gone);
    eng.tombstone_acks
        .write()
        .entry(key)
        .or_default()
        .insert(live, version);
    // `gone` never acknowledges: not stable.
    assert!(!eng.is_tombstone_stable(&key, version));

    // Decommissioning the silent peer makes the tombstone stable.
    eng.decommission_peer(gone);
    assert!(eng.is_tombstone_stable(&key, version));

    // Forgetting the tombstone clears its bookkeeping.
    eng.forget_tombstone(&key);
    assert!(eng.tombstone_acks.read().get(&key).is_none());
}

/// A tombstone with zero acknowledgments — the state a deletion is in the instant it happens —
/// must read as pending. This is the exact case a predicate walking `tombstone_acks` (rather than
/// `live_tombstones`) gets wrong, because no entry exists there until the first ack arrives.
#[tokio::test]
async fn fresh_tombstone_with_zero_acks_is_pending() {
    let eng = engine("127.0.0.67").await;
    let peer: IpAddr = "127.0.0.68".parse().unwrap();
    let key = 3;
    eng.just_insert(
        key,
        Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
            NodeId::new(0),
        )),
    );

    assert!(
        eng.tombstone_acks.read().get(&key).is_none(),
        "test setup: no ack should exist yet"
    );
    assert!(
        eng.has_pending_tombstone_acks(peer),
        "a freshly deleted, zero-ack tombstone must count as pending"
    );
}

#[tokio::test]
async fn no_local_tombstones_is_never_pending() {
    let eng = engine("127.0.0.69").await;
    let peer: IpAddr = "127.0.0.70".parse().unwrap();
    eng.just_insert(
        1,
        Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            42,
        ),
    );
    assert!(!eng.has_pending_tombstone_acks(peer));
}

#[tokio::test]
async fn acked_tombstone_is_not_pending_but_stale_or_missing_ack_is() {
    let eng = engine("127.0.0.71").await;
    let acked: IpAddr = "127.0.0.72".parse().unwrap();
    let stale: IpAddr = "127.0.0.73".parse().unwrap();
    let silent: IpAddr = "127.0.0.74".parse().unwrap();
    let key = 5;
    let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
        NodeId::new(0),
    ));
    let version = version_hash(&tombstone);
    eng.just_insert(key, tombstone);

    eng.tombstone_acks
        .write()
        .entry(key)
        .or_default()
        .insert(acked, version);
    eng.tombstone_acks
        .write()
        .entry(key)
        .or_default()
        .insert(stale, version.wrapping_add(1));

    assert!(!eng.has_pending_tombstone_acks(acked));
    assert!(eng.has_pending_tombstone_acks(stale));
    assert!(eng.has_pending_tombstone_acks(silent));
}
