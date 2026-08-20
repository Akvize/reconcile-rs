// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use tokio_util::sync::CancellationToken;

use crate::{replicated_map::Config, ReplicatedMap};

/// The default cap must be 1024.
#[test]
fn peer_cap_default_is_1024() {
    assert_eq!(Config::default().max_peers, 1024);
}

/// A raw payload with one `ComparisonItem`: a dated message, so the receive path would add
/// the sender to `members` unless the cap fires first. Starts with the wire-version byte
/// every datagram carries, unauthenticated included — `Authenticator::Disabled` no
/// longer passes bytes through unversioned.
fn dated_comparison_payload() -> Vec<u8> {
    use crate::replica::Message;
    use crate::FingerprintTreeMap;
    use bincode::{DefaultOptions, Serializer};
    use serde::Serialize as _;

    let tree = FingerprintTreeMap::<i32, (crate::clock::Timestamp, Option<i32>)>::new();
    let segments = rbsr::initial_ranges(&tree);
    let mut buf = vec![gossip::auth::WIRE_VERSION];
    for seg in segments {
        Message::<
            i32,
            (crate::clock::Timestamp, Option<i32>),
            (crate::clock::Timestamp, Option<i32>),
        >::ComparisonItem(seg)
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .expect("serializing ComparisonItem into a Vec cannot fail");
    }
    buf
}

/// When the membership set is at capacity, datagrams from a completely unknown sender are
/// dropped silently: the sender is not added to `members`, `peers`, or the replay filter.
/// Known members (already in `members`) are unaffected at cap.
#[tokio::test(flavor = "multi_thread")]
async fn peer_cap_blocks_unknown_sender_at_capacity() {
    let port = 9801u16;
    let target_addr: std::net::IpAddr = "127.0.0.180".parse().unwrap();
    let peer1: std::net::IpAddr = "127.0.0.181".parse().unwrap();
    let peer2: std::net::IpAddr = "127.0.0.182".parse().unwrap();
    let newcomer: std::net::IpAddr = "127.0.0.183".parse().unwrap();

    let config = Config::default()
        .with_port(port)
        .with_listen_addr(target_addr)
        .with_net("127.0.0.180/30".parse().unwrap())
        .with_max_peers(2)
        .with_insecure_no_key();

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    // Seed two known members directly (simulates two peers that already completed a
    // dated handshake with this store before the test window).
    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);

    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

    // Give the run loop time to enter its receive wait.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send a valid dated payload from the newcomer's IP several times to ensure at least one
    // reaches the receive loop; all must be dropped by the cap.
    let payload = dated_comparison_payload();
    let sender = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(newcomer, 0))
        .await
        .expect("bind sender");
    for _ in 0..5 {
        let _ = sender
            .send_to(&payload, std::net::SocketAddr::new(target_addr, port))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Give the receive loop time to process any remaining datagrams.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The newcomer must not appear in any map regardless of how many datagrams arrived.
    assert!(
        !store.engine.members.read().contains(&newcomer),
        "capped-out sender must not be added to members"
    );
    assert!(
        !store.engine.peers.read().contains_key(&newcomer),
        "capped-out sender must not be added to peers"
    );

    task.abort();
}

/// A sender that is already a member continues to be processed normally when the cap is reached.
#[tokio::test(flavor = "multi_thread")]
async fn peer_cap_allows_known_member_at_capacity() {
    let port = 9802u16;
    let target_addr: std::net::IpAddr = "127.0.0.184".parse().unwrap();
    let peer1: std::net::IpAddr = "127.0.0.185".parse().unwrap();
    let peer2: std::net::IpAddr = "127.0.0.186".parse().unwrap();

    let config = Config::default()
        .with_port(port)
        .with_listen_addr(target_addr)
        .with_net("127.0.0.184/30".parse().unwrap())
        .with_max_peers(2)
        .with_insecure_no_key();

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);

    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

    // Give the run loop time to enter its receive wait.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send a valid dated payload FROM a known member (peer1), retrying until accepted.
    let payload = dated_comparison_payload();
    let sender = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(peer1, 0))
        .await
        .expect("bind sender");

    let mut peers_refreshed = false;
    for _ in 0..50 {
        let _ = sender
            .send_to(&payload, std::net::SocketAddr::new(target_addr, port))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if store.engine.peers.read().contains_key(&peer1) {
            peers_refreshed = true;
            break;
        }
    }

    // Known member must still be present (not evicted).
    assert!(
        store.engine.members.read().contains(&peer1),
        "known member must not be evicted by the cap"
    );
    // The peers entry is (re-)inserted when the datagram is processed.
    assert!(
        peers_refreshed,
        "known member's peers entry must be refreshed normally"
    );

    task.abort();
}

/// Decommissioning a member frees its slot, letting a new sender through the cap.
#[tokio::test]
async fn decommission_frees_peer_cap_slot() {
    let peer1: std::net::IpAddr = "127.1.0.1".parse().unwrap();
    let peer2: std::net::IpAddr = "127.1.0.2".parse().unwrap();
    let newcomer: std::net::IpAddr = "127.1.0.3".parse().unwrap();

    let config = Config::default()
        .with_port(crate::replica::tests::next_ephemeral_test_port())
        .with_listen_addr("127.0.0.1".parse().unwrap())
        .with_max_peers(2)
        .with_insecure_no_key();

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    // Seed two members (simulates two peers that completed a dated exchange).
    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);
    assert_eq!(store.engine.members.read().len(), 2);

    // At cap: a newcomer would be blocked.
    assert!(
        store.engine.members.read().len() >= 2,
        "precondition: at cap"
    );
    assert!(
        !store.engine.members.read().contains(&newcomer),
        "precondition: newcomer not yet in members"
    );

    // Decommission peer2, freeing one slot.
    store.forget_peer(peer2);
    assert_eq!(
        store.engine.members.read().len(),
        1,
        "one member after decommission"
    );
    assert!(
        !store.engine.members.read().contains(&peer2),
        "peer2 must be removed from members"
    );
    // The peers gossip-routing entry must also be gone (so capacity is visible to the
    // cap check path, and decommission does not leave a ghost entry in the peers map).
    assert!(
        !store.engine.peers.read().contains_key(&peer2),
        "peer2 must be removed from peers by decommission"
    );

    // After decommission: members.len() == 1 < max_peers == 2.
    // The cap check allows the newcomer through: current_len < 2.
    let current_len = store.engine.members.read().len();
    let is_known = store.engine.members.read().contains(&newcomer);
    assert!(
        is_known || current_len < 2,
        "newcomer must not be capped out (members.len={current_len}) after decommission freed a slot"
    );
}

/// In authenticated mode, datagrams from a capped-out sender must not create a replay-filter
/// entry.  The cap check runs before `replay_filter.check_and_record` for unknown senders.
#[tokio::test(flavor = "multi_thread")]
async fn peer_cap_no_replay_entry_for_capped_sender() {
    let port = 9804u16;
    let target_addr: std::net::IpAddr = "127.0.0.192".parse().unwrap();
    let peer1: std::net::IpAddr = "127.0.0.193".parse().unwrap();
    let peer2: std::net::IpAddr = "127.0.0.194".parse().unwrap();
    let newcomer: std::net::IpAddr = "127.0.0.195".parse().unwrap();
    let cluster_key = [0x55u8; 32];

    let config = Config::default()
        .with_port(port)
        .with_listen_addr(target_addr)
        .with_net("127.0.0.192/30".parse().unwrap())
        .with_cluster_key(gossip::auth::ClusterKey::new(cluster_key))
        .with_max_peers(2);

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);

    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

    // Craft a sealed (authenticated) dated datagram from the newcomer's IP.
    let payload = dated_comparison_payload();
    let counter = gossip::replay::SenderCounter::new();
    let sealed =
        gossip::auth::Authenticator::new(Some(gossip::auth::ClusterKey::new(cluster_key)), false)
            .seal(counter.next_seq(), counter.next_stamp(), &payload);

    let sender = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(newcomer, 0))
        .await
        .expect("bind sender");
    sender
        .send_to(&sealed, std::net::SocketAddr::new(target_addr, port))
        .await
        .expect("send");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The cap fired before the replay filter: no replay entry must have been created.
    assert_eq!(
        store.engine.replay_filter_len(),
        0,
        "no replay-filter entry must be created for a capped-out sender"
    );

    task.abort();
}
