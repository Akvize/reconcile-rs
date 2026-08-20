// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Tombstone causal-stability and garbage collection: a tombstone must survive until every
//! member has acknowledged it, and must eventually collect once they have — pairwise and across
//! a cluster with no manual decommissioning.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reconcile::{replicated_map::Config, ReplicatedMap};

use crate::support::{assert_until, assert_until_slow};

/// Regression test: a tombstone must not be garbage-collected while a replica
/// that has not acknowledged it is still a member (causal stability), and decommissioning that
/// replica must release the tombstone for GC.
#[tokio::test(flavor = "multi_thread")]
async fn tombstone_is_retained_until_peer_acknowledges() {
    // A dedicated port isolates this test from the others: peer discovery probes a random
    // address in 127.0.0.0/8 on this port, so sharing a port lets concurrently-running tests
    // cross-talk and pollute each other's stores.
    let port = 8084;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.72".parse().unwrap();
    let addr2 = "127.0.0.73".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_insecure_no_key();

    // Aggressive wall-clock expiry so that, without causal-stability gating, the tombstone
    // would be GC'd almost immediately.
    let store1 = ReplicatedMap::<i32, i32>::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(50));
    let store2 = ReplicatedMap::<i32, i32>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1)
        .with_tombstone_timeout(Duration::from_millis(50));

    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

    // Establish mutual membership by exchanging a value in each direction.
    store1.insert(1, 11);
    assert_until!(store2.get(&1).as_deref() == Some(&11));
    store2.insert(2, 22);
    assert_until!(store1.get(&2).as_deref() == Some(&22));

    // "Partition" store2: stop processing its network/GC but keep its in-memory data.
    task2.abort();

    // Delete key 1 on store1; store2 (a member) cannot acknowledge while partitioned.
    store1.remove(&1);
    assert!(store1.get(&1).is_none());
    let fingerprint_with_tombstone = store1.fingerprint(..);

    // Wait well past both the tombstone timeout (50 ms) and the GC scan period (1 s): the
    // tombstone must still be present because store2 has not acknowledged it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        store1.fingerprint(..),
        fingerprint_with_tombstone,
        "tombstone was garbage-collected before the partitioned peer acknowledged it (resurrection hazard)"
    );

    // Decommission the silent peer: the tombstone is now causally stable and may be GC'd.
    store1.forget_peer(addr2);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_ne!(
        store1.fingerprint(..),
        fingerprint_with_tombstone,
        "tombstone was not collected after the silent peer was decommissioned"
    );

    task1.abort();
}

/// Regression test: a value deleted while a replica is partitioned must not be
/// resurrected when that replica returns with the stale value.
#[tokio::test(flavor = "multi_thread")]
async fn deleted_value_is_not_resurrected_by_returning_peer() {
    // Dedicated port for test isolation (see `tombstone_is_retained_until_peer_acknowledges`).
    let port = 8085;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.70".parse().unwrap();
    let addr2 = "127.0.0.71".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_insecure_no_key();

    let store1 = ReplicatedMap::<i32, i32>::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(50));
    let store2 = ReplicatedMap::<i32, i32>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1)
        .with_tombstone_timeout(Duration::from_millis(50));

    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

    // Both replicas hold key 1 = v, and become members of each other.
    store1.insert(1, 11);
    assert_until!(store2.get(&1).as_deref() == Some(&11));
    store2.insert(2, 22);
    assert_until!(store1.get(&1).as_deref() == Some(&11));
    assert_until!(store1.get(&2).as_deref() == Some(&22));

    // Partition store2 while it still holds the stale value 1 = 11.
    task2.abort();
    assert_eq!(store2.get(&1).as_deref(), Some(&11));

    // Delete key 1 on store1. The tombstone is held back (store2 has not acknowledged it),
    // even across many GC scans.
    store1.remove(&1);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(store1.get(&1).is_none());

    // store2 returns with the stale value and reconciles.
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

    // The deletion propagates to store2; crucially, the stale value never resurrects on store1.
    assert_until!(store2.get(&1).is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        store1.get(&1).is_none(),
        "deleted value was resurrected by the returning partitioned peer"
    );
    assert!(
        store2.get(&1).is_none(),
        "deletion did not reach the returning peer"
    );

    task1.abort();
    task2.abort();
}

/// At N≥3, a tombstone must be collected on **every** node with no manual decommission — which
/// needs the periodic ack resend, acks otherwise being pairwise. Full-mesh topology.
#[tokio::test(flavor = "multi_thread")]
async fn tombstone_gc_converges_in_3_node_cluster_mesh() {
    // Dedicated port for test isolation.
    let port = 8120;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.110".parse().unwrap();
    let addr2 = "127.0.0.111".parse().unwrap();
    let addr3 = "127.0.0.112".parse().unwrap();

    // Short tombstone timeout (quick GC eligibility) and short reconcile interval (ack resends
    // flow fast) keep the test brief; GC still cannot fire until causal stability is reached.
    let mk = |addr| {
        Config::default()
            .with_port(port)
            .with_listen_addr(addr)
            .with_net(net)
            .with_reconcile_interval(Duration::from_millis(100))
            .with_insecure_no_key()
    };
    let store1 = ReplicatedMap::<i32, i32>::new(mk(addr1))
        .await
        .expect("bind failed")
        .with_seed(addr2)
        .with_seed(addr3)
        .with_tombstone_timeout(Duration::from_millis(200));
    let store2 = ReplicatedMap::<i32, i32>::new(mk(addr2))
        .await
        .expect("bind failed")
        .with_seed(addr1)
        .with_seed(addr3)
        .with_tombstone_timeout(Duration::from_millis(200));
    let store3 = ReplicatedMap::<i32, i32>::new(mk(addr3))
        .await
        .expect("bind failed")
        .with_seed(addr1)
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(200));

    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task3 = tokio::spawn(store3.clone().run(CancellationToken::new()));

    // Each node writes a distinct live key; once all keys are visible everywhere, every pair has
    // exchanged dated messages, so all three are mutual causal-stability members.
    store1.insert(1, 11);
    store2.insert(2, 22);
    store3.insert(3, 33);
    assert_until!(store1.get(&2).as_deref() == Some(&22) && store1.get(&3).as_deref() == Some(&33));
    assert_until!(store2.get(&1).as_deref() == Some(&11) && store2.get(&3).as_deref() == Some(&33));
    assert_until!(store3.get(&1).as_deref() == Some(&11) && store3.get(&2).as_deref() == Some(&22));

    // Delete key 1 on store1 and let the tombstone propagate to both other nodes.
    store1.remove(&1);
    assert_until!(store2.get(&1).is_none() && store3.get(&1).is_none());

    // Once all three converge on the tombstone state, capture that fingerprint. It only changes
    // again when the tombstone is actually GC'd (the live keys 2 and 3 are never rewritten).
    let fp_tombstone = store1.fingerprint(..);
    assert_until!(store2.fingerprint(..) == fp_tombstone && store3.fingerprint(..) == fp_tombstone);

    // The regression: with NO decommissioning, the tombstone must be collected on all three
    // nodes (this hung before the fix), and they must converge to the same post-GC fingerprint.
    assert_until_slow!(store1.fingerprint(..) != fp_tombstone);
    assert_until_slow!(store2.fingerprint(..) != fp_tombstone);
    assert_until_slow!(store3.fingerprint(..) != fp_tombstone);
    let fp_collected = store1.fingerprint(..);
    assert_until_slow!(
        store2.fingerprint(..) == fp_collected && store3.fingerprint(..) == fp_collected
    );

    task1.abort();
    task2.abort();
    task3.abort();
}

/// The same in a line topology (A↔B↔C), where propagation is relayed rather than broadcast from
/// one origin. The mesh test above is the strict guard; a line completes the ack matrix through
/// the stale-value ack path anyway.
#[tokio::test(flavor = "multi_thread")]
async fn tombstone_gc_converges_in_3_node_cluster_line() {
    let port = 8121;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.113".parse().unwrap();
    let addr2 = "127.0.0.114".parse().unwrap();
    let addr3 = "127.0.0.115".parse().unwrap();

    let mk = |addr| {
        Config::default()
            .with_port(port)
            .with_listen_addr(addr)
            .with_net(net)
            .with_reconcile_interval(Duration::from_millis(100))
            .with_insecure_no_key()
    };
    // Line: A seeds B; B seeds A and C; C seeds B. Seeds define the intended topology; a stray
    // discovery probe could only add connectivity, which never prevents GC convergence.
    let store1 = ReplicatedMap::<i32, i32>::new(mk(addr1))
        .await
        .expect("bind failed")
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(200));
    let store2 = ReplicatedMap::<i32, i32>::new(mk(addr2))
        .await
        .expect("bind failed")
        .with_seed(addr1)
        .with_seed(addr3)
        .with_tombstone_timeout(Duration::from_millis(200));
    let store3 = ReplicatedMap::<i32, i32>::new(mk(addr3))
        .await
        .expect("bind failed")
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(200));

    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task3 = tokio::spawn(store3.clone().run(CancellationToken::new()));

    store1.insert(1, 11);
    store3.insert(3, 33);
    assert_until!(store1.get(&3).as_deref() == Some(&33));
    assert_until!(store3.get(&1).as_deref() == Some(&11));
    assert_until!(store2.get(&1).as_deref() == Some(&11) && store2.get(&3).as_deref() == Some(&33));

    // Delete key 1 on store1; the tombstone must reach C via B.
    store1.remove(&1);
    assert_until!(store2.get(&1).is_none() && store3.get(&1).is_none());

    let fp_tombstone = store1.fingerprint(..);
    assert_until!(store2.fingerprint(..) == fp_tombstone && store3.fingerprint(..) == fp_tombstone);

    assert_until_slow!(store1.fingerprint(..) != fp_tombstone);
    assert_until_slow!(store2.fingerprint(..) != fp_tombstone);
    assert_until_slow!(store3.fingerprint(..) != fp_tombstone);
    let fp_collected = store1.fingerprint(..);
    assert_until_slow!(
        store2.fingerprint(..) == fp_collected && store3.fingerprint(..) == fp_collected
    );

    task1.abort();
    task2.abort();
    task3.abort();
}
