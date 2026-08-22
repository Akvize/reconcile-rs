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

use tokio_util::sync::CancellationToken;

use crate::{
    replicated_map::{ConfigError, MAX_NETS},
    ReplicatedMap,
};

use super::ephemeral_config;

/// `set_nets` enforces the same [`MAX_NETS`] cap `Config::with_net`/`try_with_net` do at
/// construction time, just at runtime — both the accepting and the rejecting side need coverage.
#[tokio::test]
async fn set_nets_enforces_max_nets_at_runtime() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();

    let within_cap: Vec<_> = (0..MAX_NETS)
        .map(|i| format!("127.0.0.0/{}", 8 + (i % 24)).parse().unwrap())
        .collect();
    store
        .set_nets(&within_cap)
        .expect("exactly MAX_NETS networks should be accepted");

    let over_cap: Vec<_> = (0..=MAX_NETS)
        .map(|i| format!("127.0.0.0/{}", 8 + (i % 24)).parse().unwrap())
        .collect();
    assert_eq!(
        store.set_nets(&over_cap),
        Err(ConfigError::TooManyNets),
        "MAX_NETS + 1 networks should be rejected"
    );
}

/// `ReplicatedMap::start_reconciliation` must actually drive a round through the engine, not
/// silently no-op: with the *automatic* background trigger disabled (an hour-long
/// `reconcile_interval`), the only way two peers can converge here is by this method being
/// called explicitly, proving the wrapper reaches the real engine call.
#[tokio::test]
async fn start_reconciliation_actually_drives_a_round() {
    use std::net::SocketAddr;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5101u16;
    let a_ip: IpAddr = "127.0.3.5".parse().unwrap();
    let b_ip: IpAddr = "127.0.3.6".parse().unwrap();
    let cfg = |ip: IpAddr| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_secs(3600))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    // Inserted before either peer is known, so the live broadcast on `insert` reaches nobody —
    // convergence below can only come from the round-based comparison `start_reconciliation`
    // drives, not from the immediate push every `insert` also performs.
    a.insert(99, 42);

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    // `run()` fires an unconditional round-0 comparison the instant it starts, independent of
    // `start_reconciliation` ever being called explicitly again — seed the peers only *after*
    // that has already happened with nobody to reach, or it alone would converge this test
    // regardless of whether the wrapper under test does anything at all. B never learns of A,
    // so B can never independently initiate either.
    tokio::time::sleep(Duration::from_millis(150)).await;
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());

    let mut converged = false;
    for _ in 0..300 {
        if b.get(&99).as_deref() == Some(&42) {
            converged = true;
            break;
        }
        a.start_reconciliation().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    task_a.abort();
    task_b.abort();
    assert!(
        converged,
        "explicit start_reconciliation calls never converged the peer"
    );
}

/// `peers_map_len` must reflect the engine's actual peer count, not a fixed literal.
#[tokio::test]
async fn peers_map_len_reflects_the_engine_peers_map() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();
    assert_eq!(store.peers_map_len(), 0);
    store
        .engine
        .peers
        .write()
        .insert("127.0.0.222".parse().unwrap(), std::time::Instant::now());
    assert_eq!(store.peers_map_len(), 1);
}

/// `tombstone_acks_len` must reflect the engine's actual tracked-key count, not a fixed literal.
#[tokio::test]
async fn tombstone_acks_len_reflects_the_engine_tombstone_acks_map() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();
    assert_eq!(store.tombstone_acks_len(), 0);
    store
        .engine
        .tombstone_acks
        .write()
        .insert(1, std::collections::HashMap::new());
    assert_eq!(store.tombstone_acks_len(), 1);
}

/// `replay_filter_len` must reflect the engine's actual per-peer replay-filter count: 0 before
/// any authenticated traffic, and registering the sender after a real authenticated exchange.
#[tokio::test]
async fn replay_filter_len_reflects_the_engine_replay_filter() {
    use std::net::SocketAddr;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5104u16;
    let a_ip: IpAddr = "127.0.5.5".parse().unwrap();
    let b_ip: IpAddr = "127.0.5.6".parse().unwrap();
    let key = gossip::auth::ClusterKey::new([7u8; 32]);
    let cfg = |ip: IpAddr| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_cluster_key(key.clone())
            .with_reconcile_interval(Duration::from_millis(5))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());
    b.engine
        .peers
        .write()
        .insert(a_ip, std::time::Instant::now());
    assert_eq!(b.replay_filter_len(), 0);
    a.insert(1, 1);

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    let mut seen = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.replay_filter_len() >= 1 {
            seen = true;
            break;
        }
    }
    task_a.abort();
    task_b.abort();
    assert!(
        seen,
        "receiving an authenticated datagram from A must register A in B's replay filter"
    );
}

/// `bulk_dumps_in_flight_count` must reflect a real paced cold-sync dump actually in progress.
#[tokio::test]
async fn bulk_dumps_in_flight_count_reflects_a_dump_actually_in_progress() {
    use std::net::SocketAddr;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5105u16;
    let a_ip: IpAddr = "127.0.6.5".parse().unwrap();
    let b_ip: IpAddr = "127.0.6.6".parse().unwrap();
    let cfg = |ip: IpAddr| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_millis(20))
            // Slow enough that a few hundred KiB of cold-sync payload stays in flight for a
            // window this test can reliably observe.
            .with_bulk_send_rate(super::super::MIN_BULK_SEND_RATE)
    };
    let a = ReplicatedMap::<i32, Vec<u8>>::new_with_transport(
        cfg(a_ip),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, Vec<u8>>::new_with_transport(
        cfg(b_ip),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());
    b.engine
        .peers
        .write()
        .insert(a_ip, std::time::Instant::now());
    // ~8000 * 200B = ~1.6MB at the 1 MiB/s floor => a couple seconds of paced transfer, a wide
    // enough window to reliably poll mid-flight even under heavy test-suite contention.
    let payload = vec![0u8; 200];
    let entries: Vec<(i32, Vec<u8>)> = (0..8000).map(|k| (k, payload.clone())).collect();
    a.just_insert_bulk(&entries);
    assert_eq!(a.bulk_dumps_in_flight_count(), 0);

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    let mut seen_in_flight = false;
    for _ in 0..400 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if a.bulk_dumps_in_flight_count() >= 1 {
            seen_in_flight = true;
            break;
        }
    }
    task_a.abort();
    task_b.abort();
    assert!(
        seen_in_flight,
        "sending a cold-sync bulk dump must register as in-flight"
    );
}

/// `set_remote_interval` must actually retune the engine's cross-network cadence: with no nets
/// declared every peer is remote by default, so this is the sole gate on contact here.
#[tokio::test]
async fn set_remote_interval_actually_retunes_the_cross_network_cadence() {
    use std::net::SocketAddr;

    use ipnet::IpNet;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5102u16;
    // With no net declared, `Replica` falls back to a flat `127.0.0.1/8` "historical loopback
    // cluster" where every loopback peer is local (contacted every round, bypassing
    // `remote_interval` entirely) — declaring A's own net is what actually makes B remote from
    // A's perspective. B is deliberately left on that flat-loopback fallback rather than also
    // declaring a narrow net for it: the default `RandomProbe` speculatively probes one random
    // address per declared net every round, unthrottled by `remote_interval`/`remote_fanout` and
    // answered unconditionally by whoever it reaches (a responder's own throttle only gates
    // outbound-*initiated* targets, never inbound requests) — a narrow declared net for B would
    // give B's own probe a real, if small, chance of finding A and leaking the value through a
    // wholly different path than the one under test. Sampling out of all 16M `127.0.0.1/8`
    // addresses instead makes that chance negligible.
    let net_a: IpNet = "127.1.0.0/24".parse().unwrap();
    let a_ip: IpAddr = "127.1.0.5".parse().unwrap();
    let b_ip: IpAddr = "127.2.0.5".parse().unwrap();
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        ephemeral_config()
            .with_listen_addr(a_ip)
            .with_port(port)
            .with_net(net_a)
            .with_reconcile_interval(Duration::from_millis(5)),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        ephemeral_config()
            .with_listen_addr(b_ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_millis(5)),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    // Inserted before either peer is known, so the live broadcast on `insert` reaches nobody —
    // only the round-based comparison can deliver it, which is what `remote_interval` gates.
    a.insert(7, 42);

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    // `run()` fires round 0 synchronously, before `reconcile_interval` is ever consulted — with
    // no peer known yet, that round reaches nobody. Only *after* letting several rounds tick
    // past (advancing the round counter well past 0, which `round % remote_interval == 0`
    // would otherwise trivially satisfy) do we introduce the peer and the starved interval.
    // B never learns of A as a peer (only A -> B is seeded): B must never independently pull
    // from A, so the only way A's data can reach B is A pushing on its own initiated round —
    // which is exactly what `remote_interval` gates.
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());
    a.set_remote_interval(100_000); // effectively never

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        b.get(&7).is_none(),
        "remote_interval=100000 must starve cross-network contact"
    );

    a.set_remote_interval(1); // every round

    let mut converged = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.get(&7).as_deref() == Some(&42) {
            converged = true;
            break;
        }
    }
    task_a.abort();
    task_b.abort();
    assert!(
        converged,
        "retuning remote_interval down must let cross-network contact resume"
    );
}

/// `set_remote_fanout` must actually retune the engine's cross-network sample size; the interval
/// is held fixed at 1 throughout so it can never be the blocker here.
#[tokio::test]
async fn set_remote_fanout_actually_retunes_the_cross_network_sample_size() {
    use std::net::SocketAddr;

    use ipnet::IpNet;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5106u16;
    // With no net declared, `Replica` falls back to a flat `127.0.0.1/8` "historical loopback
    // cluster" where every loopback peer is local (contacted every round, bypassing
    // `remote_fanout` entirely) — declaring A's own net is what actually makes B remote from A's
    // perspective. B is deliberately left on that flat-loopback fallback rather than also
    // declaring a narrow net for it: the default `RandomProbe` speculatively probes one random
    // address per declared net every round, unthrottled by `remote_interval`/`remote_fanout` and
    // answered unconditionally by whoever it reaches (a responder's own throttle only gates
    // outbound-*initiated* targets, never inbound requests) — a narrow declared net for B would
    // give B's own probe a real, if small, chance of finding A and leaking the value through a
    // wholly different path than the one under test. Sampling out of all 16M `127.0.0.1/8`
    // addresses instead makes that chance negligible.
    let net_a: IpNet = "127.1.1.0/24".parse().unwrap();
    let a_ip: IpAddr = "127.1.1.5".parse().unwrap();
    let b_ip: IpAddr = "127.2.1.5".parse().unwrap();
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        ephemeral_config()
            .with_listen_addr(a_ip)
            .with_port(port)
            .with_net(net_a)
            .with_reconcile_interval(Duration::from_millis(5)),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        ephemeral_config()
            .with_listen_addr(b_ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_millis(5)),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    // Inserted before either peer is known, so the live broadcast on `insert` reaches nobody —
    // only the round-based comparison can deliver it, which is what `remote_fanout` gates. B
    // never learns of A as a peer (only A -> B is seeded): B must never independently pull from
    // A, so the only way A's data can reach B is A pushing on its own initiated round.
    a.insert(7, 42);
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());
    a.set_remote_interval(1); // never the blocker here
    a.set_remote_fanout(0);

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        b.get(&7).is_none(),
        "remote_fanout=0 must starve cross-network contact"
    );

    a.set_remote_fanout(1);

    let mut converged = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.get(&7).as_deref() == Some(&42) {
            converged = true;
            break;
        }
    }
    task_a.abort();
    task_b.abort();
    assert!(
        converged,
        "retuning remote_fanout up must let cross-network contact resume"
    );
}

/// `set_reconcile_interval` must actually retune the round cadence at runtime.
#[tokio::test]
async fn set_reconcile_interval_actually_retunes_the_round_cadence() {
    use std::net::SocketAddr;

    use ipnet::IpNet;

    use crate::transport::InMemoryNetwork;

    let net_fabric = InMemoryNetwork::new();
    let port = 5103u16;
    let shared_net: IpNet = "127.0.4.0/24".parse().unwrap();
    let a_ip: IpAddr = "127.0.4.5".parse().unwrap();
    let b_ip: IpAddr = "127.0.4.6".parse().unwrap();
    let cfg = |ip: IpAddr| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_net(shared_net)
            // Deliberately far longer than this test's timeout: convergence below can only
            // happen if the runtime `set_reconcile_interval` call actually overrides this
            // before the loop's first real wait, proving the setter is not a no-op.
            .with_reconcile_interval(Duration::from_secs(3600))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip),
        Arc::new(net_fabric.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip),
        Arc::new(net_fabric.bind(SocketAddr::new(b_ip, port))),
    );
    // Retuned before `run()` ever starts, so it is already in effect the first time the round
    // loop consults it (right after the unconditional round-0 call `run()` always makes, which
    // never honors `reconcile_interval` at all — retuning *after* that first wait has already
    // begun would not unstick it, since the interval is only re-read at the top of each
    // iteration).
    a.set_reconcile_interval(Duration::from_millis(5));
    a.insert(7, 42);

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    // A only learns of B after round 0 (fires unconditionally and instantly on `run()` entry)
    // has already happened with no peer to reach; B never learns of A at all, so B can never
    // independently pull — the only path is A pushing on its own retuned cadence.
    tokio::time::sleep(Duration::from_millis(150)).await;
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());

    let mut converged = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.get(&7).as_deref() == Some(&42) {
            converged = true;
            break;
        }
    }
    task_a.abort();
    task_b.abort();
    assert!(
        converged,
        "retuning reconcile_interval before the loop's first wait must make it converge \
         quickly, not wait out the original 3600s interval"
    );
}

/// `set_coalesce_window` must actually retune the running engine's coalescing window, not be a
/// no-op setter. Starves the reconciliation fallback (`reconcile_interval` far beyond this
/// test's deadline, B never told about A) exactly like `set_reconcile_interval`'s own test
/// above, so B observing nothing within the window is attributable only to a real, in-effect
/// window on A's write path.
#[tokio::test]
async fn set_coalesce_window_actually_delays_the_broadcast() {
    use std::net::SocketAddr;

    use crate::transport::InMemoryNetwork;

    let net_fabric = InMemoryNetwork::new();
    let port = 5105u16;
    let a_ip: IpAddr = "127.0.4.7".parse().unwrap();
    let b_ip: IpAddr = "127.0.4.8".parse().unwrap();
    let cfg = |ip: IpAddr| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_secs(3600))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip),
        Arc::new(net_fabric.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip),
        Arc::new(net_fabric.bind(SocketAddr::new(b_ip, port))),
    );

    let task_a = tokio::spawn(a.clone().run(CancellationToken::new()));
    let task_b = tokio::spawn(b.clone().run(CancellationToken::new()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.engine
        .peers
        .write()
        .insert(b_ip, std::time::Instant::now());

    // Far longer than this test's deadline: only a real (non-no-op) setter suppresses the
    // otherwise-immediate broadcast for this long.
    a.set_coalesce_window(Duration::from_secs(3600));
    a.insert(7, 42);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let not_yet_delivered = b.get(&7).is_none();

    task_a.abort();
    task_b.abort();
    assert!(
        not_yet_delivered,
        "set_coalesce_window must actually delay the broadcast — B observed A's write \
         immediately, as if the window were still the zero default"
    );
}
