// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Multi-network topology: cross-net reconciliation, discovery, and the runtime net/knob-mutation
//! API — independent of the LWW convergence semantics `basic.rs` covers.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reconcile::{replicated_map::Config, Fingerprint, ReplicatedMap};

use crate::support::{assert_until, wait_until};

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

    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

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

    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

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

    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

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

    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

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
