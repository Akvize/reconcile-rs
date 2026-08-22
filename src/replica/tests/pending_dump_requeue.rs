// Copyright 2026 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Regression test for #516: a `differences` batch that loses the per-peer dump-slot race
//! (`try_claim_dump_slot` allows only one bulk dump in flight per peer) must be requeued and
//! drained once that slot frees, not silently dropped.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// A single `start_reconciliation` fully resolves a wide scattered divergence. `n = 10 000`,
/// `d = 1 000` scattered is #516's own repro: reliably produces enough recursive `SPLIT`/
/// `ENUMERATE` traffic that at least one `ENUMERATE` batch is discovered while another dump to
/// the same peer is already in flight, so it must lose the per-peer dump-slot race — pre-fix,
/// that batch (and everything else `just_remove`d in the same range) is silently dropped and
/// never repaired.
///
/// `reconcile_interval` is set far longer than this test's own deadline, so convergence within
/// that deadline can only come from the requeue-and-drain fix (`stash_pending_dump`/
/// `spawn_paced_send`'s own loop) — never from the idle timeout papering over a lost batch, which
/// is what made the underlying bug hide behind `service_reconcile_rtt`'s manual retriggers rather
/// than fail outright.
#[tokio::test]
async fn wide_scattered_divergence_converges_on_a_single_round() {
    let net = InMemoryNetwork::new();
    let port = crate::replica::tests::next_ephemeral_test_port();
    let a_ip: IpAddr = "127.0.0.4".parse().unwrap();
    let b_ip: IpAddr = "127.0.0.5".parse().unwrap();
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
    a.peers.write().insert(b_ip, Instant::now());
    b.peers.write().insert(a_ip, Instant::now());

    const N: u32 = 10_000;
    for k in 0..N {
        a.just_insert(
            k,
            Entry::present(a.clock_now(), k.wrapping_mul(2_654_435_761)),
        );
    }

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());

    // Cold-sync: B starts empty and must pull the whole corpus first.
    let want = live_view(&a);
    let settle_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < settle_deadline && live_view(&b) != want {
        tokio::task::yield_now().await;
    }
    assert_eq!(live_view(&b), want, "cold-sync bootstrap did not settle");

    // 1000 scattered keys, out of 10 000 -- #516's own repro.
    const D: u32 = 1_000;
    let missing: Vec<u32> = (1..=D as u64)
        .map(|i| ((N as u64 / (D as u64 + 1)) * i) as u32)
        .collect();
    for &k in &missing {
        a.just_insert(k, Entry::tombstone(a.clock_now()));
    }

    a.start_reconciliation(&mut Vec::new()).await;

    let repair_deadline = Instant::now() + Duration::from_secs(15);
    let mut repaired = false;
    while Instant::now() < repair_deadline {
        let all_tombstoned = missing.iter().all(|k| {
            b.map
                .read()
                .get(k)
                .is_some_and(crate::entry::Entry::is_tombstone)
        });
        if all_tombstoned {
            repaired = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    ta.abort();
    tb.abort();
    assert!(
        repaired,
        "a single start_reconciliation did not repair a wide scattered divergence -- #516"
    );
}
