use std::time::Duration;

use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};

use reconcile::{reconcile_store::Config, Fingerprint, ReconcileStore};

/// Wait for a while until the provided predicate becomes true
///
/// If the predicate become true in the delay, return true, otherwise return false. This functions
/// minimizes the wait time by checking regularly if the predicate is true.
async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
}

macro_rules! assert_until {
    ( $x:expr ) => {
        assert!(wait_until(|| $x).await, stringify!($x))
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let port = 8080;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.44".parse().unwrap();
    let addr2 = "127.0.0.45".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net);

    // create tree1 with many values
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    // start reconciliation stores for tree1 and tree2
    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_hash = store1.fingerprint(..);
    let store2 = ReconcileStore::new(cfg2).await.with_seed(addr1);
    // Check the initial state *before* spawning the run loops: store1's `insert_bulk` already
    // spawned a background broadcast to its seeded peer (store2), so once store2 starts
    // receiving these asserts would race with reconciliation.
    assert_eq!(store2.fingerprint(..), Fingerprint::ZERO);
    assert_eq!(store1.fingerprint(..), start_hash);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // check that tree2 is filled with the values from tree1
    assert_until!(store2.fingerprint(..) == start_hash);

    // check that tree1 is unchanged
    assert_eq!(store1.fingerprint(..), start_hash);

    // add value to tree2, and check that it is transferred to tree1
    let key = "42".to_string();
    let value = "Hello, World!".to_string();
    store2.insert(key.clone(), value.clone());
    assert_until!(store1.get(&key).as_deref() == Some(&value));

    // remove value from tree1, and check that the tombstone is transferred to tree2
    store1.remove(&key);
    assert_until!(store2.get(&key).is_none());

    // Check that a *causally later* write always wins. The conflict order is established by
    // making the second writer observe the first write before acting (we wait for the first
    // value to propagate). Under the Hybrid Logical Clock this means the second writer's clock
    // has advanced past the first timestamp, so its write is ordered strictly after — the
    // deterministic, causality-respecting LWW contract. (We deliberately do *not* rely on
    // wall-clock real-time order across the two independent node clocks: two writes in the same
    // millisecond on different nodes are genuinely concurrent and resolved by node id, which is
    // exactly the ambiguity issue #110 is about.)
    let key = "42".to_string();
    for i in 0..20 {
        // Unique values per iteration so each `assert_until` observes *this* write, not a value
        // left over from a previous iteration.
        let first = format!("first-{i}");
        let second = format!("second-{i}");
        if rng.gen() {
            // store1 writes, store2 observes it, then store2 overwrites: store2's value wins.
            store1.insert(key.clone(), first.clone());
            assert_until!(store2.get(&key).as_deref() == Some(&first));
            store2.insert(key.clone(), second.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&second));
            assert_until!(store2.get(&key).as_deref() == Some(&second));
        } else if rng.gen() {
            // Symmetric: store2 writes first, store1 observes, then store1 wins.
            store2.insert(key.clone(), first.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&first));
            store1.insert(key.clone(), second.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&second));
            assert_until!(store2.get(&key).as_deref() == Some(&second));
        } else if rng.gen() {
            // value then tombstone: the deletion observes the value and wins.
            store1.insert(key.clone(), first.clone());
            assert_until!(store2.get(&key).as_deref() == Some(&first));
            store2.remove(&key);
            assert_until!(store1.get(&key).is_none());
            assert_until!(store2.get(&key).is_none());
        } else {
            // tombstone then value: the insert observes the tombstone and wins.
            store1.insert(key.clone(), first.clone());
            assert_until!(store2.get(&key).as_deref() == Some(&first));
            store1.remove(&key);
            assert_until!(store2.get(&key).is_none());
            store2.insert(key.clone(), second.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&second));
            assert_until!(store2.get(&key).as_deref() == Some(&second));
        }
    }

    // check that a newer value can overwrite a tombstone
    let key = "43".to_string();
    let value1 = "Hello, World!".to_string();
    let value2 = "Goodbye!".to_string();
    // insert (key, value1) pair
    store1.insert(key.clone(), value1.clone());
    // wait until store2 has received it
    assert_until!(store2.get(&key).as_deref() == Some(&value1));
    // remove the key from store2
    store2.remove(&key);
    // wait until store1 has received the tombstone
    assert_until!(store1.get(&key).is_none());
    // overwrite tombstone by inserting (key, value2)
    store1.insert(key.clone(), value2.clone());
    // check that instance2 receives value2
    assert_until!(store2.get(&key).as_deref() == Some(&value2));

    task2.abort();
    task1.abort();
}

/// Two nodes sharing the same cluster key must still converge, proving that authenticated
/// datagrams round-trip end-to-end through the MAC layer.
#[tokio::test(flavor = "multi_thread")]
async fn authenticated_nodes_converge() {
    let port = 8081;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.46".parse().unwrap();
    let addr2 = "127.0.0.47".parse().unwrap();
    let key = [0x42u8; 32];
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_cluster_key(key);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_cluster_key(key);

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_hash = store1.fingerprint(..);
    let store2 = ReconcileStore::new(cfg2).await.with_seed(addr1);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // store2 should receive all of store1's values across the authenticated channel
    assert_until!(store2.fingerprint(..) == start_hash);

    // a fresh incremental insert also propagates
    let key = "auth-key".to_string();
    let value = "authenticated value".to_string();
    store2.insert(key.clone(), value.clone());
    assert_until!(store1.get(&key).as_deref() == Some(&value));

    task2.abort();
    task1.abort();
}

/// Regression test for issue #110: two replicas that concurrently write *different* values to
/// the same key must converge to a single agreed value, with matching fingerprints.
///
/// Before the Hybrid Logical Clock fix, conflict resolution keyed on the physical wall clock
/// with a non-commutative tie-break: on equal timestamps each replica kept its own value, and
/// because the timestamp is part of the reconciliation hash the fingerprints never matched, so
/// the protocol re-exchanged the pair forever (permanent divergence + livelock). With the
/// total-order HLC the survivor is deterministic, so both replicas agree and the fingerprints
/// equalize. If the regression returned, the convergence assertions below would time out.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_converge() {
    let port = 8083;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.80".parse().unwrap();
    let addr2 = "127.0.0.81".parse().unwrap();
    // Fixed, distinct node ids give a deterministic conflict winner (the higher id).
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_node_id(1);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_node_id(2);

    let store1 = ReconcileStore::<String, String>::new(cfg1)
        .await
        .with_seed(addr2);
    let store2 = ReconcileStore::<String, String>::new(cfg2)
        .await
        .with_seed(addr1);
    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());

    // Hammer the same key from both nodes with different values, back to back, so that some
    // writes race closely in time.
    let key = "contended".to_string();
    for i in 0..50 {
        store1.insert(key.clone(), format!("from-1-{i}"));
        store2.insert(key.clone(), format!("from-2-{i}"));
    }

    // Both replicas must converge: identical fingerprints over the whole range, and the same
    // value for the contended key. (A surviving divergence/livelock would never equalize.)
    assert_until!(store1.fingerprint(..) == store2.fingerprint(..));
    let v1 = store1.get(&key).map(|g| g.clone());
    let v2 = store2.get(&key).map(|g| g.clone());
    assert_eq!(
        v1, v2,
        "replicas disagree on the contended key: {v1:?} vs {v2:?}"
    );
    assert!(v1.is_some(), "the contended key vanished entirely");

    task1.abort();
    task2.abort();
}

/// Regression test for issue #109: a tombstone must not be garbage-collected while a replica
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
        .with_net(net);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net);

    // Aggressive wall-clock expiry so that, without causal-stability gating, the tombstone
    // would be GC'd almost immediately.
    let store1 = ReconcileStore::<i32, i32>::new(cfg1)
        .await
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(50));
    let store2 = ReconcileStore::<i32, i32>::new(cfg2)
        .await
        .with_seed(addr1)
        .with_tombstone_timeout(Duration::from_millis(50));

    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());

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
    let hash_with_tombstone = store1.fingerprint(..);

    // Wait well past both the tombstone timeout (50 ms) and the GC scan period (1 s): the
    // tombstone must still be present because store2 has not acknowledged it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        store1.fingerprint(..),
        hash_with_tombstone,
        "tombstone was garbage-collected before the partitioned peer acknowledged it (resurrection hazard)"
    );

    // Decommission the silent peer: the tombstone is now causally stable and may be GC'd.
    store1.forget_peer(addr2);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_ne!(
        store1.fingerprint(..),
        hash_with_tombstone,
        "tombstone was not collected after the silent peer was decommissioned"
    );

    task1.abort();
}

/// Regression test for issue #109: a value deleted while a replica is partitioned must not be
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
        .with_net(net);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net);

    let store1 = ReconcileStore::<i32, i32>::new(cfg1)
        .await
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(50));
    let store2 = ReconcileStore::<i32, i32>::new(cfg2)
        .await
        .with_seed(addr1)
        .with_tombstone_timeout(Duration::from_millis(50));

    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());

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
    let task2 = tokio::spawn(store2.clone().run());

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

/// Regression test for the remote DoS where a single malformed UDP datagram panicked the
/// receive loop, silently killing reconciliation (issue #107).
///
/// We send a malformed datagram to each node, then check that reconciliation still works.
/// Before the fix, the receive loop task would panic and die, and the propagation assertion
/// below would time out.
#[tokio::test(flavor = "multi_thread")]
async fn test_malformed_datagram_does_not_crash() {
    let port = 8082;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.46".parse().unwrap();
    let addr2 = "127.0.0.47".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net);

    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
    let store2 = ReconcileStore::new(cfg2).await.with_seed(addr1);
    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());

    // 0x02 is an invalid bincode enum tag for `Message`; before the fix this panicked the
    // receive loop. Send it to both nodes' protocol sockets from an unrelated socket.
    let attacker = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    attacker.send_to(&[0x02], (addr1, port)).await.unwrap();
    attacker.send_to(&[0x02], (addr2, port)).await.unwrap();

    // Reconciliation must still work: a value inserted on one node reaches the other.
    let key = "key".to_string();
    let value = "value".to_string();
    store1.insert(key.clone(), value.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&value));

    task2.abort();
    task1.abort();
}

/// Two nodes sharing the same cluster key with encryption enabled must converge, proving that
/// payloads round-trip end-to-end through the XChaCha20-Poly1305 layer (issue #96).
#[cfg(feature = "encryption")]
#[tokio::test(flavor = "multi_thread")]
async fn encrypted_nodes_converge() {
    let port = 8083;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.48".parse().unwrap();
    let addr2 = "127.0.0.49".parse().unwrap();
    let key = [0x42u8; 32];
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_cluster_key(key)
        .with_encryption();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_cluster_key(key)
        .with_encryption();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_hash = store1.fingerprint(..);
    let store2 = ReconcileStore::new(cfg2).await.with_seed(addr1);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // store2 should receive all of store1's values across the encrypted channel
    assert_until!(store2.fingerprint(..) == start_hash);

    // a fresh incremental insert also propagates
    let key = "enc-key".to_string();
    let value = "encrypted value".to_string();
    store2.insert(key.clone(), value.clone());
    assert_until!(store1.get(&key).as_deref() == Some(&value));

    task2.abort();
    task1.abort();
}

/// A node with the wrong key must be rejected: its encrypted datagrams fail to decrypt on the
/// peer (and vice versa), so the two never converge. This is the confidentiality analog of an
/// "invalid certificate" rejection — only a holder of the shared secret can join.
#[cfg(feature = "encryption")]
#[tokio::test(flavor = "multi_thread")]
async fn encrypted_node_with_wrong_key_is_rejected() {
    let port = 8084;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.50".parse().unwrap();
    let addr2 = "127.0.0.51".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_cluster_key([0x42u8; 32])
        .with_encryption();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_cluster_key([0x99u8; 32]) // different key
        .with_encryption();

    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
    store1.insert("secret".to_string(), "value".to_string());
    let start_hash = store1.fingerprint(..);
    let store2 = ReconcileStore::<String, String>::new(cfg2)
        .await
        .with_seed(addr1);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // store2 must NOT be able to read store1's data: with a wrong key every datagram fails
    // authentication and is dropped, so it never reaches store1's fingerprint.
    assert!(
        !wait_until(|| store2.fingerprint(..) == start_hash).await,
        "node with the wrong key must not converge"
    );

    task2.abort();
    task1.abort();
}

/// Issue #53: two nodes in distinct geographical networks converge over cross-network anti-entropy.
///
/// Networks are simulated by two disjoint /30 subnets inside the loopback range, each node living in
/// one of them and declaring both. A dedicated port isolates the test.
#[tokio::test(flavor = "multi_thread")]
async fn cross_net_reconciliation() {
    let port = 8085;
    let net_a = "127.0.0.0/30".parse().unwrap();
    let net_b = "127.0.1.0/30".parse().unwrap();
    let addr1 = "127.0.0.1".parse().unwrap();
    let addr2 = "127.0.1.1".parse().unwrap();
    // Each node is local to its own network and declares the other as a remote one. A short
    // cross-network cadence keeps the test fast.
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net_a)
        .with_net(net_b)
        .with_remote_interval(1)
        .with_remote_fanout(1);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net_b)
        .with_net(net_a)
        .with_remote_interval(1)
        .with_remote_fanout(1);

    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
    store1.insert("key".to_string(), "value".to_string());
    let start_hash = store1.fingerprint(..);
    let store2 = ReconcileStore::<String, String>::new(cfg2)
        .await
        .with_seed(addr1);
    assert_eq!(store2.fingerprint(..), Fingerprint::ZERO);

    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // The remote-network peer eventually receives the value over cross-network anti-entropy.
    assert_until!(store2.get(&"key".to_string()).as_deref() == Some(&"value".to_string()));
    assert_until!(store2.fingerprint(..) == start_hash);

    task1.abort();
    task2.abort();
}

/// Issue #53: a node auto-discovers a peer in another network purely from the network's CIDR, with
/// no seed. Discovery probes one random address per network each round. To keep the test
/// deterministic (rather than relying on a random probe landing on the peer within a subnet), each
/// node declares the *other node's exact address* as a network (a /32), so the per-network discovery
/// probe reliably targets the peer. The local network stays a /30 so local-network probing is
/// unaffected.
#[tokio::test(flavor = "multi_thread")]
async fn cross_net_discovery_without_seed() {
    let port = 8086;
    let net_a = "127.0.2.0/30".parse().unwrap();
    let net_b = "127.0.3.0/30".parse().unwrap();
    let addr1 = "127.0.2.1".parse().unwrap();
    let addr2 = "127.0.3.1".parse().unwrap();
    // Each node declares the peer's exact address as a network, so its discovery probe always hits it.
    let peer2_host = "127.0.3.1/32".parse().unwrap();
    let peer1_host = "127.0.2.1/32".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net_a)
        .with_net(peer2_host)
        .with_remote_interval(1)
        .with_remote_fanout(1);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net_b)
        .with_net(peer1_host)
        .with_remote_interval(1)
        .with_remote_fanout(1);

    // No `with_seed`: the two nodes must find each other purely through per-network discovery probes.
    let store1 = ReconcileStore::new(cfg1).await;
    store1.insert("k".to_string(), "v".to_string());
    let start_hash = store1.fingerprint(..);
    let store2 = ReconcileStore::<String, String>::new(cfg2).await;

    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    assert_until!(store2.fingerprint(..) == start_hash);

    task1.abort();
    task2.abort();
}
