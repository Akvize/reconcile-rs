use std::time::Duration;

use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};

use reconcile::{clock::NodeId, replicated_map::Config, ClusterKey, Fingerprint, ReplicatedMap};

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

/// Like [`wait_until`] but waits up to ~10 s. Tombstone GC is gated on a 1 s scan loop
/// (`TOMBSTONE_CLEARING`) plus the wall-clock tombstone timeout, so events that depend on a
/// completed GC need a longer budget than the 1 s [`wait_until`].
async fn wait_until_slow<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
}

macro_rules! assert_until_slow {
    ( $x:expr ) => {
        assert!(wait_until_slow(|| $x).await, stringify!($x))
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
        .with_net(net)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_insecure_no_key();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    // Check the initial state *before* spawning the run loops: store1's `insert_bulk` already
    // spawned a background broadcast to its seeded peer (store2), so once store2 starts
    // receiving these asserts would race with reconciliation.
    assert_eq!(store2.fingerprint(..), Fingerprint::ZERO);
    assert_eq!(store1.fingerprint(..), start_fingerprint);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    assert_until!(store2.fingerprint(..) == start_fingerprint);

    assert_eq!(store1.fingerprint(..), start_fingerprint);

    let key = "42".to_string();
    let value = "Hello, World!".to_string();
    store2.insert(key.clone(), value.clone());
    assert_until!(store1.get(&key).as_deref() == Some(&value));

    store1.remove(&key);
    assert_until!(store2.get(&key).is_none());

    // A causally later write must win. Causality is established by waiting for the first value
    // to propagate — not by wall-clock order, which for concurrent writes means nothing.
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

    // A newer value can overwrite a tombstone.
    let key = "43".to_string();
    let value1 = "Hello, World!".to_string();
    let value2 = "Goodbye!".to_string();
    store1.insert(key.clone(), value1.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&value1));
    store2.remove(&key);
    assert_until!(store1.get(&key).is_none());
    store1.insert(key.clone(), value2.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&value2));

    task2.abort();
    task1.abort();
}

/// An in-place `get_mut` mutation must be re-stamped and broadcast, so peers converge to the
/// edited value exactly as for an `insert`.
#[tokio::test(flavor = "multi_thread")]
async fn get_mut_edit_propagates_to_peers() {
    let port = 8089;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.100".parse().unwrap();
    let addr2 = "127.0.0.101".parse().unwrap();
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

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());

    let key = "k".to_string();
    let before = "before".to_string();
    let after = "after".to_string();

    store1.insert(key.clone(), before.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&before));

    store1.get_mut(&key, |v| {
        if let Some(v) = v {
            *v = after.clone();
        }
    });
    assert_eq!(store1.get(&key).as_deref(), Some(&after));

    // The crux: the in-place edit must reach store2. This fails before the fix, because `get_mut`
    // did not re-stamp the timestamp or broadcast the change.
    assert_until!(store2.get(&key).as_deref() == Some(&after));

    task1.abort();
    task2.abort();
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
        .with_cluster_key(ClusterKey::new(key));
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_cluster_key(ClusterKey::new(key));

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // store2 should receive all of store1's values across the authenticated channel
    assert_until!(store2.fingerprint(..) == start_fingerprint);

    // a fresh incremental insert also propagates
    let key = "auth-key".to_string();
    let value = "authenticated value".to_string();
    store2.insert(key.clone(), value.clone());
    assert_until!(store1.get(&key).as_deref() == Some(&value));

    task2.abort();
    task1.abort();
}

/// Two replicas writing different values to one key concurrently must converge on one value with
/// matching fingerprints.
///
/// A non-commutative tie-break would leave each keeping its own, and since the timestamp is part
/// of the fingerprint, re-exchanging the pair forever. The assertions below time out if so.
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
        .with_node_id(NodeId::new(1))
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_node_id(NodeId::new(2))
        .with_insecure_no_key();

    let store1 = ReplicatedMap::<String, String>::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed")
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

/// A malformed datagram must not kill the receive loop: reconciliation must still work after one
/// is delivered to each node.
#[tokio::test(flavor = "multi_thread")]
async fn test_malformed_datagram_does_not_crash() {
    let port = 8082;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.46".parse().unwrap();
    let addr2 = "127.0.0.47".parse().unwrap();
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

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
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
/// payloads round-trip end-to-end through the XChaCha20-Poly1305 layer.
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
        .with_cluster_key(ClusterKey::new(key))
        .with_encryption();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_cluster_key(ClusterKey::new(key))
        .with_encryption();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // store2 should receive all of store1's values across the encrypted channel
    assert_until!(store2.fingerprint(..) == start_fingerprint);

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
        .with_cluster_key(ClusterKey::new([0x42u8; 32]))
        .with_encryption();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_cluster_key(ClusterKey::new([0x99u8; 32])) // different key
        .with_encryption();

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert("secret".to_string(), "value".to_string());
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // store2 must NOT be able to read store1's data: with a wrong key every datagram fails
    // authentication and is dropped, so it never reaches store1's fingerprint.
    assert!(
        !wait_until(|| store2.fingerprint(..) == start_fingerprint).await,
        "node with the wrong key must not converge"
    );

    task2.abort();
    task1.abort();
}

/// Two nodes in distinct networks — disjoint loopback /30s, each declaring both — converge over
/// cross-network anti-entropy.
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
        .with_remote_fanout(1)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net_b)
        .with_net(net_a)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert("key".to_string(), "value".to_string());
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    assert_eq!(store2.fingerprint(..), Fingerprint::ZERO);

    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // The remote-network peer eventually receives the value over cross-network anti-entropy.
    assert_until!(store2.get(&"key".to_string()).as_deref() == Some(&"value".to_string()));
    assert_until!(store2.fingerprint(..) == start_fingerprint);

    task1.abort();
    task2.abort();
}

/// A node auto-discovers a peer in another network from the CIDR alone, with no seed.
///
/// Each node declares the other's address as a /32 net, so the per-network probe is deterministic;
/// the local net stays a /30.
#[tokio::test(flavor = "multi_thread")]
async fn cross_net_discovery_without_seed() {
    let port = 8086;
    let net_a = "127.0.2.0/30".parse().unwrap();
    let net_b = "127.0.3.0/30".parse().unwrap();
    let addr1 = "127.0.2.1".parse().unwrap();
    let addr2 = "127.0.3.1".parse().unwrap();
    let peer2_host = "127.0.3.1/32".parse().unwrap();
    let peer1_host = "127.0.2.1/32".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net_a)
        .with_net(peer2_host)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net_b)
        .with_net(peer1_host)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();

    // No `with_seed`: the two nodes must find each other purely through per-network discovery probes.
    let store1 = ReplicatedMap::new(cfg1).await.expect("bind failed");
    store1.insert("k".to_string(), "v".to_string());
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed");

    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    assert_until!(store2.fingerprint(..) == start_fingerprint);

    task1.abort();
    task2.abort();
}

/// A network declared **while the loop is running** must take effect: two nodes that cannot
/// converge without the peer's net do so once [`add_net`](ReplicatedMap::add_net) injects it.
#[tokio::test(flavor = "multi_thread")]
async fn runtime_add_net_enables_discovery_and_convergence() {
    let port = 8087;
    let net_a = "127.0.4.0/30".parse().unwrap();
    let net_b = "127.0.5.0/30".parse().unwrap();
    let addr1 = "127.0.4.1".parse().unwrap();
    let addr2 = "127.0.5.1".parse().unwrap();
    let peer2_host = "127.0.5.1/32".parse().unwrap();
    let peer1_host = "127.0.4.1/32".parse().unwrap();
    // Fast cadence to converge quickly.
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net_a)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net_b)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();

    let store1 = ReplicatedMap::new(cfg1).await.expect("bind failed");
    store1.insert("k".to_string(), "v".to_string());
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed");

    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    assert!(
        !wait_until(|| store2.fingerprint(..) == start_fingerprint).await,
        "nodes converged before the peer network was declared"
    );

    // Inject the peer network at runtime on both nodes — discovery now probes the peer.
    assert!(store1.add_net(peer2_host));
    assert!(store2.add_net(peer1_host));

    assert_until!(store2.fingerprint(..) == start_fingerprint);

    task1.abort();
    task2.abort();
}

/// Repair is decoupled from net membership: a seeded peer in **none** of the declared networks
/// still converges — the guarantee that keeps a topology change from causing silent divergence.
#[tokio::test(flavor = "multi_thread")]
async fn unclassified_peer_is_still_reconciled() {
    let port = 8088;
    // A declared network that contains neither node.
    let foreign_net = "127.0.7.0/30".parse().unwrap();
    let addr1 = "127.0.6.1".parse().unwrap();
    let addr2 = "127.0.6.2".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(foreign_net)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(foreign_net)
        .with_remote_interval(1)
        .with_remote_fanout(1)
        .with_insecure_no_key();

    // Seeded so each knows the other, even though neither address is in any declared network.
    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert("k".to_string(), "v".to_string());
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);

    // Local net of last resort is each node's own host route (peer is not local).
    assert_eq!(store1.local_net(), "127.0.6.1/32".parse().unwrap());

    let task2 = tokio::spawn(store2.clone().run());
    let task1 = tokio::spawn(store1.clone().run());

    // Converges purely through the unclassified-peer repair bucket.
    assert_until!(store2.fingerprint(..) == start_fingerprint);

    task1.abort();
    task2.abort();
}

/// The runtime topology API mutates shared state and re-derives the local network consistently, and
/// the scalar knob setters do not panic. Pure API-level checks (no run loop needed).
#[tokio::test]
async fn runtime_config_setters() {
    let addr = "127.0.8.1".parse().unwrap();
    let net_c = "127.0.8.0/30".parse().unwrap(); // contains addr
    let net_d = "127.0.9.0/30".parse().unwrap(); // does not contain addr
    let host_route = "127.0.8.1/32".parse().unwrap();

    let store = ReplicatedMap::<i32, i32>::new(
        Config::default()
            .with_port(8090)
            .with_listen_addr(addr)
            .with_net(net_c)
            .with_insecure_no_key(),
    )
    .await
    .expect("bind failed");
    assert_eq!(store.nets(), vec![net_c]);
    assert_eq!(store.local_net(), net_c);

    // add_net: appends, leaves local net unchanged, and is idempotent.
    assert!(store.add_net(net_d));
    assert_eq!(store.local_net(), net_c);
    assert_eq!(store.nets(), vec![net_c, net_d]);
    assert!(store.add_net(net_d));
    assert_eq!(store.nets().len(), 2, "add_net must be idempotent");

    // remove_net: removing the local net re-derives it to the host route fallback.
    assert!(store.remove_net(net_c));
    assert_eq!(store.nets(), vec![net_d]);
    assert_eq!(store.local_net(), host_route);
    assert!(
        !store.remove_net(net_c),
        "removing an absent net returns false"
    );

    // set_nets: wholesale replacement re-derives the local net.
    store.set_nets(&[net_c]).unwrap();
    assert_eq!(store.nets(), vec![net_c]);
    assert_eq!(store.local_net(), net_c);

    // add_net enforces the MAX_NETS cap (no-op + false beyond it).
    for i in 0..(reconcile::replicated_map::MAX_NETS - 1) {
        let n = format!("127.1.{i}.0/30").parse().unwrap();
        assert!(store.add_net(n));
    }
    assert_eq!(store.nets().len(), reconcile::replicated_map::MAX_NETS);
    let overflow = "127.2.0.0/30".parse().unwrap();
    assert!(
        !store.add_net(overflow),
        "add_net past MAX_NETS must return false"
    );
    assert_eq!(store.nets().len(), reconcile::replicated_map::MAX_NETS);

    // Scalar knob setters: smoke (no getters, must not panic).
    store.set_remote_interval(3);
    store.set_remote_fanout(5);
    store.set_reconcile_interval(Duration::from_millis(200));
    store.set_tombstone_timeout(Duration::from_millis(500));
}

/// A legitimately-sealed datagram stamped an hour in the past — well outside the default
/// freshness window — must be dropped silently, leaving the engine running.
#[cfg(reconcile_internal_testing)]
#[tokio::test(flavor = "multi_thread")]
async fn stale_datagram_outside_freshness_window_is_rejected() {
    use reconcile::testing::seal_datagram;

    let port = 8092;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr_victim = "127.0.9.1".parse().unwrap();
    let key = [0xBBu8; 32];

    let cfg = Config::default()
        .with_port(port)
        .with_listen_addr(addr_victim)
        .with_net(net)
        .with_cluster_key(ClusterKey::new(key));

    let store = ReplicatedMap::<i32, i32>::new(cfg)
        .await
        .expect("bind failed");
    store.just_insert(0, 99);
    let task = tokio::spawn(store.clone().run());

    tokio::time::sleep(Duration::from_millis(20)).await;

    let sender_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = format!("{}:{}", addr_victim, port);

    // A stamp from 1 hour ago — definitively outside the 5-minute freshness window.
    let one_hour_ago_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .saturating_sub(Duration::from_secs(3600))
        .as_millis() as u64;

    // A minimal `Ack` message: bincode variant index 2, key=0 (i32), version token=0 (u64).
    // If the freshness check were absent, this would reach the handler and do nothing (no tombstone
    // for key=0 exists).  With the freshness check, it never reaches the handler at all.
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u32.to_le_bytes()); // Ack variant index
    payload.extend_from_slice(&0i32.to_le_bytes()); // key = 0
    payload.extend_from_slice(&0u64.to_le_bytes()); // version token = 0

    let stale_datagram = seal_datagram(key, 1, one_hour_ago_ms, &payload);
    sender_sock.send_to(&stale_datagram, &target).await.unwrap();

    // Give the engine time to (not) process.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The injected datagram must have been rejected silently: engine is still running.
    assert!(
        !task.is_finished(),
        "engine must still be running after stale datagram"
    );
    // No state change: the value inserted before the test started is unchanged.
    assert_eq!(store.get(&0).as_deref(), Some(&99));

    task.abort();
}

/// Redelivering identical sealed bytes must be dropped silently: the sequence number is already
/// recorded in the per-peer replay filter.
#[cfg(reconcile_internal_testing)]
#[tokio::test(flavor = "multi_thread")]
async fn replayed_sealed_datagram_is_rejected() {
    use reconcile::testing::seal_datagram;

    let port = 8093;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr_victim = "127.0.10.1".parse().unwrap();
    let key = [0xDDu8; 32];

    let cfg = Config::default()
        .with_port(port)
        .with_listen_addr(addr_victim)
        .with_net(net)
        .with_cluster_key(ClusterKey::new(key));

    let store = ReplicatedMap::<i32, i32>::new(cfg)
        .await
        .expect("bind failed");
    let task = tokio::spawn(store.clone().run());

    // Give the run loop time to start before injecting traffic.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let sender_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = format!("{}:{}", addr_victim, port);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // A minimal `Ack` message: bincode variant index 2, key=0 (i32), version token=0 (u64).
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u32.to_le_bytes()); // Ack variant index
    payload.extend_from_slice(&0i32.to_le_bytes()); // key = 0
    payload.extend_from_slice(&0u64.to_le_bytes()); // version token = 0

    // Seal once with seq=1 and a fresh stamp.
    let datagram = seal_datagram(key, 1, now_ms, &payload);

    // First delivery: seq=1 is new for this sender — passes the replay filter.
    sender_sock.send_to(&datagram, &target).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second delivery: identical bytes, seq=1 already recorded — must be dropped by the replay
    // filter without reaching the message handler.
    sender_sock.send_to(&datagram, &target).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The engine must still be alive: replay rejection is silent, not a crash.
    assert!(
        !task.is_finished(),
        "engine must still be running after replay"
    );

    task.abort();
}

/// A decommissioned peer's captured datagram, replayed while still fresh, must be rejected and
/// must not re-add the peer to membership (AGENTS.md §8).
///
/// Evicting the peer's replay state would make the replay read as first contact.
#[cfg(reconcile_internal_testing)]
#[tokio::test(flavor = "multi_thread")]
async fn decommissioned_peer_replay_is_rejected() {
    use reconcile::testing::{members_snapshot, seal_datagram};

    let port = 8094;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr_victim: std::net::IpAddr = "127.0.11.1".parse().unwrap();
    let addr_sender: std::net::IpAddr = "127.0.11.2".parse().unwrap();
    let key = [0xEEu8; 32];

    let cfg = Config::default()
        .with_port(port)
        .with_listen_addr(addr_victim)
        .with_net(net)
        .with_cluster_key(ClusterKey::new(key));

    let store = ReplicatedMap::<i32, i32>::new(cfg)
        .await
        .expect("bind failed");
    let task = tokio::spawn(store.clone().run());

    tokio::time::sleep(Duration::from_millis(20)).await;

    let sender_sock = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(addr_sender, 0))
        .await
        .unwrap();
    let target = format!("{}:{}", addr_victim, port);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // A minimal `Ack` message (variant 2): causes `spoke_dated = true` so the sender is
    // added to `members` on acceptance.
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u32.to_le_bytes()); // Ack variant index
    payload.extend_from_slice(&0i32.to_le_bytes()); // key = 0
    payload.extend_from_slice(&0u64.to_le_bytes()); // version token = 0

    // Seal the datagram — this is the "captured" datagram the adversary holds.
    let captured = seal_datagram(key, 1, now_ms, &payload);

    // First delivery: peer X joins members.
    sender_sock.send_to(&captured, &target).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        members_snapshot(&store).contains(&addr_sender),
        "sender must have joined members after first delivery"
    );

    // Decommission peer X: removes it from members/tombstone_acks but must NOT erase replay state.
    store.forget_peer(addr_sender);
    assert!(
        !members_snapshot(&store).contains(&addr_sender),
        "sender must be gone from members after decommission"
    );

    // Replay the SAME still-fresh captured datagram.
    sender_sock.send_to(&captured, &target).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The replay must be rejected: X must NOT be re-added to members.
    assert!(
        !members_snapshot(&store).contains(&addr_sender),
        "decommissioned peer must not be re-added to members by a replayed datagram"
    );

    assert!(!task.is_finished(), "engine must still be running");
    task.abort();
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

    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());
    let task3 = tokio::spawn(store3.clone().run());

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

    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());
    let task3 = tokio::spawn(store3.clone().run());

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
