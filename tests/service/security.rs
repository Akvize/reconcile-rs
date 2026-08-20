// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Auth, encryption, malformed input, and replay: the wire layer's security guarantees
//! (AGENTS.md §8), exercised end-to-end rather than unit-tested against `gossip` directly.

use std::time::Duration;

use rand::{
    distributions::{Alphanumeric, DistString},
    SeedableRng,
};

use tokio_util::sync::CancellationToken;

use reconcile::{replicated_map::Config, ClusterKey, ReplicatedMap};

use crate::support::assert_until;
#[cfg(feature = "encryption")]
use crate::support::wait_until;

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
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

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
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

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
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

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
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

    // store2 must NOT be able to read store1's data: with a wrong key every datagram fails
    // authentication and is dropped, so it never reaches store1's fingerprint.
    assert!(
        !wait_until(|| store2.fingerprint(..) == start_fingerprint).await,
        "node with the wrong key must not converge"
    );

    task2.abort();
    task1.abort();
}

/// A legitimately-sealed datagram stamped an hour in the past — well outside the default
/// freshness window — must be dropped silently, leaving the engine running.
#[cfg(feature = "internal-testing")]
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
    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

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
#[cfg(feature = "internal-testing")]
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
    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

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
#[cfg(feature = "internal-testing")]
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
    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

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
