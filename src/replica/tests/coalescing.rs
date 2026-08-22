// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Broadcast coalescing (#187). `coalesce_window == Duration::ZERO` (the default) is already
//! covered by every other test in this directory — `insert` still calls `queue_broadcast`, which
//! takes the immediate-broadcast branch unconditionally at that setting — so these tests only
//! cover the `> 0` behavior: same-key collapse, batching multiple keys into one flush, and that
//! coalescing does not change what a cluster converges to.
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use crate::clock::{ManualClock, NodeId};
use crate::entry::Entry;
use crate::replica::Replica;
use crate::replicated_map::Config;
use crate::transport::{InMemoryNetwork, Transport};

use super::next_ephemeral_test_port;

/// Three writes to the same key, well inside the coalescing window, must collapse to exactly one
/// pending entry carrying the last write's (greatest-stamped, under `ManualClock`'s monotonic
/// `now()`) value — not three queued messages a flush would send separately.
#[tokio::test]
async fn same_key_writes_within_the_window_collapse_to_one_pending_entry() {
    let net = InMemoryNetwork::new();
    let ip: IpAddr = "127.0.10.1".parse().unwrap();
    let port = next_ephemeral_test_port();
    let cfg = Config {
        // An hour: long enough that this test's own body cannot race the flush.
        coalesce_window: Duration::from_secs(3600),
        ..Config::default()
            .with_listen_addr(ip)
            .with_port(port)
            .with_insecure_no_key()
    };
    let engine: Replica<u32, u32> = Replica::new_with_transport(
        cfg,
        Arc::new(net.bind(SocketAddr::new(ip, port))),
        Arc::new(ManualClock::new(NodeId::new(1))),
    );

    engine.insert(1, Entry::present(engine.clock_now(), 10));
    engine.insert(1, Entry::present(engine.clock_now(), 20));
    engine.insert(1, Entry::present(engine.clock_now(), 30));

    let pending = engine.coalesce_pending.read();
    assert_eq!(
        pending.len(),
        1,
        "three writes to the same key must collapse to one pending entry, got {pending:?}"
    );
    assert_eq!(
        pending.get(&1).and_then(Entry::value),
        Some(&30),
        "the collapsed entry must carry the last write's value"
    );
}

/// Five writes to five distinct keys, all inside one coalescing window, must flush as exactly one
/// datagram — not five. Probed by binding a plain receiver (no engine, no run loop) on the peer
/// address and counting what actually arrives on the wire, the same technique
/// `tests/pacing.rs::oversized_message_is_dropped_not_sent_empty_or_oversized` uses.
#[tokio::test]
async fn distinct_key_writes_within_the_window_flush_as_one_datagram() {
    let net = InMemoryNetwork::new();
    let port = next_ephemeral_test_port();
    let sender_ip: IpAddr = "127.0.10.2".parse().unwrap();
    let peer_ip: IpAddr = "127.0.10.3".parse().unwrap();
    let window = Duration::from_millis(150);
    let cfg = Config {
        coalesce_window: window,
        ..Config::default()
            .with_listen_addr(sender_ip)
            .with_port(port)
            // Long enough that only the coalesced flush, never the periodic round, can be
            // responsible for what the receiver observes.
            .with_reconcile_interval(Duration::from_secs(3600))
            .with_insecure_no_key()
    };
    let engine: Replica<u32, u32> = Replica::new_with_transport(
        cfg,
        Arc::new(net.bind(SocketAddr::new(sender_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(1))),
    );
    let peer_transport = net.bind(SocketAddr::new(peer_ip, port));
    engine.peers.write().insert(peer_ip, Instant::now());

    for key in 0..5u32 {
        engine.insert(key, Entry::present(engine.clock_now(), key));
    }

    let mut buf = [0u8; 65536];
    let too_early = tokio::time::timeout(window / 2, peer_transport.recv_from(&mut buf)).await;
    assert!(
        too_early.is_err(),
        "5 writes inside the coalescing window must not be sent before it elapses"
    );

    tokio::time::timeout(Duration::from_secs(2), peer_transport.recv_from(&mut buf))
        .await
        .expect("expected the flushed datagram well within 2s of the 150ms window")
        .expect("recv_from failed");
    let second = tokio::time::timeout(
        Duration::from_millis(100),
        peer_transport.recv_from(&mut buf),
    )
    .await;
    assert!(
        second.is_err(),
        "expected exactly one flushed datagram for 5 same-window writes, got a second"
    );
}

/// Load `entries` into engine A purely through `insert` (the propagating, coalescing-aware
/// path — never `just_insert`) with a coalescing window short enough that several flushes happen
/// over the run, and B's `reconcile_interval` starved so only the coalesced pushes, never anti-
/// entropy, can deliver anything (mirrors `immediate_broadcast.rs`'s starving technique). Returns
/// B's final live view once it stops changing or a deadline elapses.
async fn converge_with_coalescing(entries: &[(u32, u32)]) -> BTreeMap<u32, u32> {
    let net = InMemoryNetwork::new();
    let port = 5100u16;
    let a_ip: IpAddr = "127.0.10.4".parse().unwrap();
    let b_ip: IpAddr = "127.0.10.5".parse().unwrap();
    let starved = Duration::from_secs(3600);
    let cfg = |ip: IpAddr, coalesce_window: Duration| Config {
        coalesce_window,
        ..Config::default()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(starved)
            .with_insecure_no_key()
    };
    let a: Replica<u32, u32> = Replica::new_with_transport(
        cfg(a_ip, Duration::from_millis(5)),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(1))),
    );
    let b: Replica<u32, u32> = Replica::new_with_transport(
        cfg(b_ip, Duration::ZERO),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
        Arc::new(ManualClock::new(NodeId::new(2))),
    );
    // Only A is told about B: B must learn purely from receiving A's coalesced flushes.
    a.peers.write().insert(b_ip, Instant::now());

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());
    // Let each engine's unconditional round-0 settle before any write, as in `immediate_broadcast.rs`.
    tokio::time::sleep(Duration::from_millis(50)).await;

    for &(k, v) in entries {
        a.insert(k, Entry::present(a.clock_now(), v));
    }

    let live_view = |eng: &Replica<u32, u32>| -> BTreeMap<u32, u32> {
        eng.map
            .read()
            .iter()
            .filter_map(|(k, e)| e.value().map(|v| (*k, *v)))
            .collect()
    };
    let mut want = BTreeMap::new();
    for &(k, v) in entries {
        want.insert(k, v);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = live_view(&b);
    while Instant::now() < deadline && got != want {
        tokio::time::sleep(Duration::from_millis(20)).await;
        got = live_view(&b);
    }

    ta.abort();
    tb.abort();
    got
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Under `ManualClock`'s per-node monotonic `now()`, later `insert` calls to the same key
    /// always carry the greater stamp, so LWW deterministically resolves to whichever call came
    /// last — a closed-form property independent of how the writes were batched. Coalescing must
    /// not change that: whatever lands inside a window collapses via the same total order
    /// `Entry::merge` uses everywhere else, so the cluster converges to exactly "last write per
    /// key wins", the same as if nothing had been batched at all.
    #[test]
    fn coalesced_writes_converge_to_the_last_write_per_key(
        entries in prop::collection::vec((0u32..8, 0u32..1000), 1..40)
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let got = rt.block_on(converge_with_coalescing(&entries));
        let mut want = BTreeMap::new();
        for &(k, v) in &entries {
            want.insert(k, v);
        }
        prop_assert_eq!(got, want, "B did not converge to A's {} coalesced writes", entries.len());
    }
}
