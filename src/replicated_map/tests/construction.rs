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

use crate::clock::NodeId;
use crate::{replicated_map::Config, ReplicatedMap};

use super::ephemeral_config;

/// D4: the node id is settable through `Config` and readable back off the store, and the
/// value reported is the one the clock actually stamps onto minted timestamps.
#[tokio::test]
async fn node_id_is_readable_and_matches_the_minted_stamp() {
    let store =
        ReplicatedMap::<i32, i32>::new(ephemeral_config().with_node_id(NodeId::new(0xABCD)))
            .await
            .unwrap();
    assert_eq!(store.node_id(), NodeId::new(0xABCD));
    store.insert(1, 1);
    let stamp = store.engine.map.read().get(&1).unwrap().stamp;
    assert_eq!(stamp.node_id(), store.node_id());
}

/// D2: two stores wired to a caller-supplied `Transport` converge over it, with no UDP socket
/// bound anywhere. This is the public seam, exercised exactly as a downstream crate would.
#[tokio::test]
async fn stores_converge_over_an_injected_transport() {
    use std::net::SocketAddr;
    use std::time::Instant;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5100u16;
    let a_ip: IpAddr = "127.0.0.4".parse().unwrap();
    let b_ip: IpAddr = "127.0.0.5".parse().unwrap();
    let cfg = |ip: IpAddr, id: u64| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_node_id(NodeId::new(id))
            .with_reconcile_interval(Duration::from_millis(5))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip, 1),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip, 2),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    // Seed each as the other's gossip peer: the in-memory fabric has no discovery.
    a.engine.peers.write().insert(b_ip, Instant::now());
    b.engine.peers.write().insert(a_ip, Instant::now());
    a.insert(7, 42);

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut converged = false;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.get(&7).as_deref() == Some(&42) {
            converged = true;
            break;
        }
    }
    ta.abort();
    tb.abort();
    assert!(
        converged,
        "B never learned A's write over the injected transport"
    );
}

/// `new` must return `Err`, not panic, when the port is already in use.
#[tokio::test]
async fn new_returns_err_on_bind_failure() {
    // Occupy a loopback port chosen by the OS (bind to :0, then read it back) so this test
    // cannot collide with a fixed port already taken by the parallel suite or a stray process.
    let holder =
        std::net::UdpSocket::bind("127.0.0.50:0").expect("pre-condition: a port must be free");
    let busy = holder
        .local_addr()
        .expect("holder must report its bound address");

    let config = Config::default()
        .with_port(busy.port())
        .with_listen_addr(busy.ip())
        .with_insecure_no_key();
    let result = ReplicatedMap::<i32, i32>::new(config).await;
    let err = result
        .err()
        .expect("expected Err when the bind address is already in use");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AddrInUse,
        "bind failure should surface as AddrInUse, got {err:?}"
    );
}
