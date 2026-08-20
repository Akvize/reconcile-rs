// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! #292: `run`'s shutdown/final-flush contract, and the introspection accessors
//! (`sync_state`/`peers`/`members`/`local_addr`) that answer "is this node actually
//! synchronizing" instead of just "is the process up".

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::transport::InMemoryNetwork;
use crate::{FileSnapshot, ReplicatedMap};

use super::ephemeral_config;

async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
}

/// `local_addr` reports the transport's real bound address, matching what was configured.
#[tokio::test]
async fn local_addr_matches_the_configured_bind_address() {
    let config = ephemeral_config();
    let expected = SocketAddr::new(config.listen_addr, config.port);
    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");
    assert_eq!(
        store
            .local_addr()
            .expect("transport must report its address"),
        expected
    );
}

/// `sync_state` starts with no rounds completed and advances as the engine actually runs —
/// a mutant that no-ops the round counter or the `last_round_at` write would leave this at its
/// initial `0`/`None` forever.
#[tokio::test(flavor = "multi_thread")]
async fn sync_state_advances_as_the_engine_runs() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    let initial = store.sync_state();
    assert_eq!(initial.rounds, 0);
    assert!(initial.last_round_at.is_none());

    let task = tokio::spawn(store.clone().run(CancellationToken::new()));
    assert!(
        wait_until(|| store.sync_state().rounds > 0 && store.sync_state().last_round_at.is_some())
            .await,
        "sync_state never observed a completed reconciliation round"
    );
    task.abort();
}

/// `peers`/`members` reflect a real converged pair, not a fixed literal — each only contains the
/// other node's address once a genuine authenticated datagram has been exchanged.
#[tokio::test(flavor = "multi_thread")]
async fn peers_and_members_reflect_a_converged_pair() {
    let net = InMemoryNetwork::new();
    let port = crate::replica::tests::next_ephemeral_test_port();
    let a_ip: IpAddr = "127.0.10.1".parse().unwrap();
    let b_ip: IpAddr = "127.0.10.2".parse().unwrap();
    let cfg = |ip: IpAddr| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_reconcile_interval(Duration::from_millis(20))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    let a = a.with_seed(b_ip);
    let b = b.with_seed(a_ip);

    // `with_seed` above already registers the peer for gossip routing; membership is earned only
    // through a real accepted datagram, so it must still be empty at this point.
    assert!(a.members().is_empty());

    let ta = tokio::spawn(a.clone().run(CancellationToken::new()));
    let tb = tokio::spawn(b.clone().run(CancellationToken::new()));

    assert!(
        wait_until(|| a.peers().contains(&b_ip) && a.members().contains(&b_ip)).await,
        "A never registered B as a peer/member"
    );
    assert!(
        wait_until(|| b.peers().contains(&a_ip) && b.members().contains(&a_ip)).await,
        "B never registered A as a peer/member"
    );

    ta.abort();
    tb.abort();
}

/// `run` returns once `shutdown` fires (rather than looping forever) and flushes a final
/// snapshot first — the snapshot must actually be durable, not just reported as `Ok`.
#[tokio::test(flavor = "multi_thread")]
async fn run_observes_cancellation_and_flushes_a_durable_final_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shutdown.bin");
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.insert(7, 70);

    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(store.clone().run(shutdown.clone()));
    assert!(
        wait_until(|| store.sync_state().rounds > 0).await,
        "run() never completed a round before shutdown"
    );
    shutdown.cancel();

    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("run() did not return after the shutdown signal fired")
        .expect("run() task panicked");
    outcome
        .final_snapshot
        .expect("the final snapshot on shutdown should succeed");

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert_eq!(
        restarted.get(&7).as_deref(),
        Some(&70),
        "run()'s final snapshot on shutdown was not actually durable"
    );
}

/// `snapshot_now` flushes immediately: it must not need the periodic background task to be
/// running at all, unlike the periodic snapshot loop.
#[tokio::test]
async fn snapshot_now_flushes_without_the_periodic_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forced.bin");
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.just_insert(3, 30);

    assert!(store.sync_state().last_snapshot_at.is_none());
    store
        .snapshot_now()
        .expect("forced snapshot should succeed");
    assert!(
        store.sync_state().last_snapshot_at.is_some(),
        "snapshot_now must record last_snapshot_at on success"
    );

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert_eq!(
        restarted.get(&3).as_deref(),
        Some(&30),
        "snapshot_now's write was not actually durable"
    );
}
