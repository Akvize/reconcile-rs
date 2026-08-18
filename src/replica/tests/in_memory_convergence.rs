// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Deterministic convergence over the [`InMemoryTransport`](crate::transport::InMemoryTransport):
//! two engines exchange datagrams entirely in-process — no real sockets — so the anti-entropy
//! protocol is exercised without any network flakiness. The engine reaches its I/O only through
//! the `Transport` port, which is what makes this substitution possible.
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use crate::clock::{ManualClock, NodeId};
use crate::entry::Entry;
use crate::replica::Replica;
use crate::replicated_map::Config;
use crate::transport::InMemoryNetwork;

/// Live (value-only) view of an engine's map, for comparing convergence regardless of stamps.
fn live_view(eng: &Replica<u32, u32>) -> BTreeMap<u32, u32> {
    eng.map
        .read()
        .iter()
        .filter_map(|(k, e)| e.value().map(|v| (*k, *v)))
        .collect()
}

/// Load `entries` into engine A and drive both engines over an in-memory network until B's live
/// view matches A's (or a short deadline elapses). Returns whether they converged.
async fn converges(entries: &[(u32, u32)]) -> bool {
    let net = InMemoryNetwork::new();
    let port = 5000u16;
    let a_ip: IpAddr = "127.0.0.2".parse().unwrap();
    let b_ip: IpAddr = "127.0.0.3".parse().unwrap();
    let cfg = |ip: IpAddr| {
        Config::default()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_millis(5))
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
    // Seed each as the other's known gossip peer (no real discovery over the in-memory fabric).
    a.peers.write().insert(b_ip, Instant::now());
    b.peers.write().insert(a_ip, Instant::now());
    // Load A only; B must learn every entry purely through anti-entropy.
    for (k, v) in entries {
        a.just_insert(*k, Entry::present(a.clock_now(), *v));
    }
    let want = live_view(&a);

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut converged = false;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if live_view(&b) == want {
            converged = true;
            break;
        }
    }
    ta.abort();
    tb.abort();
    converged
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// For any small map, a cold replica converges to it over the in-memory transport.
    #[test]
    fn cold_replica_converges_over_in_memory_transport(
        entries in prop::collection::vec((0u32..64, 0u32..1000), 0..24)
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ok = rt.block_on(converges(&entries));
        prop_assert!(ok, "B did not converge to A's {} entries over the in-memory transport", entries.len());
    }
}
