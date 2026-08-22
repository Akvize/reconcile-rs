// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `Replica::insert`'s immediate push (`just_insert` + `broadcast`) is a distinct delivery
//! path from the periodic anti-entropy round `start_reconciliation` drives — a mutant that
//! guts `broadcast` into a no-op is invisible to a convergence test whose only peer also
//! polls the sender periodically, since the round would eventually pull the same data. These
//! tests starve that fallback (a `reconcile_interval` far longer than the test can run, and
//! the receiver never told about the sender) so only the immediate push can possibly deliver.
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::{ManualClock, NodeId};
use crate::entry::Entry;
use crate::replica::Replica;
use crate::replicated_map::Config;
use crate::transport::InMemoryNetwork;

/// `insert`'s broadcast must reach an already-known peer without waiting for a reconciliation
/// round: both engines' `reconcile_interval` is set far beyond the test's deadline, and only
/// the sender is told about the peer — the receiver never queries anyone.
#[tokio::test]
async fn insert_broadcasts_immediately_without_a_reconciliation_round() {
    let net = InMemoryNetwork::new();
    let port = 5006u16;
    let a_ip: IpAddr = "127.0.4.10".parse().unwrap();
    let b_ip: IpAddr = "127.0.4.11".parse().unwrap();
    let cfg = |ip: IpAddr| {
        Config::default()
            .with_listen_addr(ip)
            .with_port(port)
            // Long enough that this test's deadline is reached first if the immediate
            // broadcast path is the only way the value can arrive.
            .with_reconcile_interval(Duration::from_secs(3600))
            .with_insecure_no_key()
    };
    let a: Replica<u32, u32> = Replica::new_with_transport(
        cfg(a_ip),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(1))),
    );
    let b: Replica<u32, u32> = Replica::new_with_transport(
        cfg(b_ip),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(2))),
    );

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());
    // Let each engine's unconditional round-0 settle while neither knows any peer, so it is a
    // no-op — otherwise a stray first round could race the assertions below.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Only A is told about B: B must learn of A purely by receiving a datagram from it.
    a.peers.write().insert(b_ip, Instant::now());
    a.insert(7, Entry::present(a.clock_now(), 42));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut received = None;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(entry) = b.map.read().get(&7) {
            received = entry.value().copied();
            if received.is_some() {
                break;
            }
        }
    }

    ta.abort();
    tb.abort();
    assert_eq!(
        received,
        Some(42),
        "B never received A's insert via the immediate broadcast path — both engines' \
         reconcile_interval is far beyond this test's deadline, and B was never told about A, \
         so only insert's own broadcast call could have delivered it"
    );
}

/// `broadcast_update`'s push — the path `ReplicatedMap::get_mut`/`update`/`upsert` use for an
/// in-place mutation that already wrote the map directly and only needs peers notified — must
/// reach an already-known peer immediately, exactly like `insert`'s own broadcast above.
#[tokio::test]
async fn broadcast_update_pushes_immediately_without_a_reconciliation_round() {
    let net = InMemoryNetwork::new();
    let port = 5008u16;
    let a_ip: IpAddr = "127.0.4.14".parse().unwrap();
    let b_ip: IpAddr = "127.0.4.15".parse().unwrap();
    let cfg = |ip: IpAddr| {
        Config::default()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_secs(3600))
            .with_insecure_no_key()
    };
    let a: Replica<u32, u32> = Replica::new_with_transport(
        cfg(a_ip),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(1))),
    );
    let b: Replica<u32, u32> = Replica::new_with_transport(
        cfg(b_ip),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(2))),
    );

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());
    tokio::time::sleep(Duration::from_millis(100)).await;

    a.peers.write().insert(b_ip, Instant::now());
    // Mirrors `get_mut`'s shape: the map is written directly (`just_insert`, no broadcast of its
    // own), then `broadcast_update` notifies peers with the same stamp, without re-inserting.
    let stamp = a.clock_now();
    a.just_insert(7, Entry::present(stamp, 42));
    a.broadcast_update(7, Entry::present(stamp, 42));

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut received = None;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(entry) = b.map.read().get(&7) {
            received = entry.value().copied();
            if received.is_some() {
                break;
            }
        }
    }

    ta.abort();
    tb.abort();
    assert_eq!(
        received,
        Some(42),
        "B never received A's update via broadcast_update's immediate push path — both \
         engines' reconcile_interval is far beyond this test's deadline, and B was never told \
         about A, so only broadcast_update's own call could have delivered it"
    );
}
