// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! End-to-end tests of dynamic discovery through the public `run()` path: decommissioning,
//! tombstone-GC gating, and the wall-time floor that guards against resurrection under DNS
//! flakiness.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reconcile::discovery::{DiscoverFuture, Discovery};
use reconcile::{reconcile_store::Config, ReconcileStore};

mod common;

// Discovery tests use a 200-iteration base (2 s at default speed) because the
// decommission/floor scenarios involve multiple discovery rounds with non-trivial
// sleeps between them.
#[allow(dead_code)]
async fn wait_until<F: FnMut() -> bool>(f: F) -> bool {
    common::wait_until_with_budget(f, 200).await
}

macro_rules! assert_until {
    ( $x:expr ) => {
        assert!(wait_until(|| $x).await, stringify!($x))
    };
}

/// A discovery source whose returned peer set the test mutates at runtime.
#[derive(Clone)]
struct ScriptedDiscovery {
    addrs: Arc<Mutex<Vec<IpAddr>>>,
}

impl ScriptedDiscovery {
    fn new(initial: Vec<IpAddr>) -> Self {
        ScriptedDiscovery {
            addrs: Arc::new(Mutex::new(initial)),
        }
    }
    fn set(&self, addrs: Vec<IpAddr>) {
        *self.addrs.lock().unwrap() = addrs;
    }
}

impl Discovery for ScriptedDiscovery {
    fn discover(&self) -> DiscoverFuture<'_> {
        let addrs = self.addrs.lock().unwrap().clone();
        Box::pin(async move { Ok(addrs) })
    }
}

/// A peer that vanishes from discovery is decommissioned after the grace period, which lets a
/// tombstone it had not acknowledged finally be garbage-collected.
#[tokio::test(flavor = "multi_thread")]
async fn vanished_peer_is_decommissioned_and_tombstone_collected() {
    let port = 8097; // dedicated port isolates this test's random probing from the others
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1: IpAddr = "127.0.0.86".parse().unwrap();
    let addr2: IpAddr = "127.0.0.87".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net);
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net);

    // store1 finds peers through discovery (which initially reports store2 present).
    let discovery = ScriptedDiscovery::new(vec![addr2]);
    let store1 = ReconcileStore::<i32, i32>::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(50))
        .with_discovery(Arc::new(discovery.clone()))
        .with_discovery_interval(Duration::from_millis(20))
        .with_discovery_miss_threshold(3)
        // A short floor so the test completes in reasonable time after the peer vanishes.
        // The floor only delays decommission when tombstone acks are pending; using 150 ms
        // here lets the test exercise the slow path (ack pending → wait floor → GC) quickly.
        .with_discovery_decommission_floor(Duration::from_millis(150));
    let store2 = ReconcileStore::<i32, i32>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1)
        .with_tombstone_timeout(Duration::from_millis(50));

    let task1 = tokio::spawn(store1.clone().run());
    let task2 = tokio::spawn(store2.clone().run());

    // Establish mutual membership by exchanging a value in each direction.
    store1.insert(1, 11);
    assert_until!(store2.get(&1).as_deref() == Some(&11));
    store2.insert(2, 22);
    assert_until!(store1.get(&2).as_deref() == Some(&22));

    // Partition store2 but keep reporting it present in discovery: its tombstone gate holds.
    task2.abort();
    store1.remove(&1);
    assert!(store1.get(&1).is_none());
    let hash_with_tombstone = store1.fingerprint(..);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        store1.fingerprint(..),
        hash_with_tombstone,
        "tombstone collected while the peer was still reported present (resurrection hazard)"
    );

    // The peer now disappears from discovery: after the grace period (miss_threshold rounds) and
    // the decommission floor (150 ms) the peer is decommissioned and the tombstone is GC'd.
    discovery.set(vec![]);
    assert_until!(store1.fingerprint(..) != hash_with_tombstone);

    task1.abort();
}

// ── Wall-time floor tests ─────────────────────────────────────────────────────────────────────
//
// These tests inspect the membership set directly, which requires the `reconcile_internal_testing`
// cfg gate (the same gate used by the peer-cap and replay tests in `tests/service.rs`).

#[cfg(reconcile_internal_testing)]
mod floor {
    use super::*;
    use reconcile::testing::{members_snapshot, peers_contains, tombstone_acks_len};

    /// Helper: build a `Config` for a loopback address + port with an isolated /32 net so random
    /// probing never escapes to other tests' ports.
    fn floor_cfg(addr: IpAddr, port: u16) -> Config {
        let net = format!("{addr}/32").parse().unwrap();
        Config::default()
            .with_port(port)
            .with_listen_addr(addr)
            .with_net(net)
    }

    /// **Resurrection regression test (test 1)**: when member B vanishes from DNS and
    /// `miss_threshold` rounds elapse but the wall-time floor has not, B must NOT be
    /// decommissioned and the tombstone must NOT be GC'd.
    ///
    /// This is the acceptance criterion for the resurrection hazard: the tombstone is kept alive
    /// exactly because B is still a member of the causal-stability set.  If the floor were absent,
    /// `miss_threshold` rounds alone would fire `decommission_peer`, remove B from the GC gate, and
    /// allow the tombstone to be collected — so a returning B could push the deleted value back.
    /// With a 10 s floor (far beyond the test window) decommission is blocked even after
    /// `miss_threshold` rounds when pending tombstone acks exist.
    #[tokio::test(flavor = "multi_thread")]
    async fn floor_prevents_premature_decommission_and_resurrection() {
        let port = 8098u16;
        let addr_a: IpAddr = "127.0.0.88".parse().unwrap();
        let addr_b: IpAddr = "127.0.0.89".parse().unwrap();

        let discovery = ScriptedDiscovery::new(vec![addr_b]);
        // Floor is 10 s — far longer than the test runs, so the floor never elapses here.
        let store_a = ReconcileStore::<i32, i32>::new(floor_cfg(addr_a, port))
            .await
            .expect("bind failed")
            .with_seed(addr_b)
            .with_tombstone_timeout(Duration::from_millis(50))
            .with_discovery(Arc::new(discovery.clone()))
            .with_discovery_interval(Duration::from_millis(20))
            .with_discovery_miss_threshold(2)
            .with_discovery_decommission_floor(Duration::from_secs(10));
        let store_b = ReconcileStore::<i32, i32>::new(floor_cfg(addr_b, port))
            .await
            .expect("bind failed")
            .with_seed(addr_a)
            .with_tombstone_timeout(Duration::from_millis(50));

        let task_a = tokio::spawn(store_a.clone().run());
        let task_b = tokio::spawn(store_b.clone().run());

        // Establish mutual membership: both stores exchange dated datagrams.
        store_a.insert(1, 100);
        assert_until!(store_b.get(&1).as_deref() == Some(&100));
        store_b.insert(2, 200);
        assert_until!(store_a.get(&2).as_deref() == Some(&200));

        // Abort B and delete key 1 on A so a tombstone is outstanding with B's ack pending.
        task_b.abort();
        store_a.remove(&1);
        assert!(store_a.get(&1).is_none());
        let hash_with_tombstone = store_a.fingerprint(..);

        // Confirm discovery has observed B at least once (B is in the peers map) before clearing
        // the resolver, so the `seen_ever` guard inside `discover_periodically` cannot skip it.
        assert_until!(peers_contains(&store_a, addr_b));

        // Remove B from discovery: miss_threshold rounds will elapse but the floor has not.
        discovery.set(vec![]);

        // Wait long enough for miss_threshold rounds to pass (2 × 20 ms + slack).
        tokio::time::sleep(Duration::from_millis(300)).await;

        // B must still be a member (floor not elapsed) and the tombstone must not be GC'd.
        // This is the core of the resurrection guard: as long as B is a member the tombstone
        // cannot be collected, so B cannot push the value back on its return.
        assert!(
            members_snapshot(&store_a).contains(&addr_b),
            "B was prematurely decommissioned before the floor elapsed"
        );
        assert_eq!(
            store_a.fingerprint(..),
            hash_with_tombstone,
            "tombstone was GC'd before the decommission floor elapsed (resurrection hazard)"
        );

        task_a.abort();
    }

    /// **Test 2**: when the floor elapses with a pending tombstone ack, the member IS
    /// decommissioned and the GC slot is released.  Uses a 100 ms floor so the test completes
    /// quickly without sleeping for 10 minutes.
    #[tokio::test(flavor = "multi_thread")]
    async fn floor_elapsed_decommissions_member_with_pending_tombstones() {
        let port = 8099u16;
        let addr_a: IpAddr = "127.0.0.90".parse().unwrap();
        let addr_b: IpAddr = "127.0.0.91".parse().unwrap();

        let discovery = ScriptedDiscovery::new(vec![addr_b]);
        let store_a = ReconcileStore::<i32, i32>::new(floor_cfg(addr_a, port))
            .await
            .expect("bind failed")
            .with_seed(addr_b)
            .with_tombstone_timeout(Duration::from_millis(50))
            .with_discovery(Arc::new(discovery.clone()))
            .with_discovery_interval(Duration::from_millis(20))
            .with_discovery_miss_threshold(2)
            .with_discovery_decommission_floor(Duration::from_millis(100));
        let store_b = ReconcileStore::<i32, i32>::new(floor_cfg(addr_b, port))
            .await
            .expect("bind failed")
            .with_seed(addr_a)
            .with_tombstone_timeout(Duration::from_millis(50));

        let task_a = tokio::spawn(store_a.clone().run());
        let task_b = tokio::spawn(store_b.clone().run());

        // Establish mutual membership.
        store_a.insert(10, 1000);
        assert_until!(store_b.get(&10).as_deref() == Some(&1000));
        store_b.insert(20, 2000);
        assert_until!(store_a.get(&20).as_deref() == Some(&2000));

        // Abort B and create a tombstone on A.
        task_b.abort();
        store_a.remove(&10);
        let hash_with_tombstone = store_a.fingerprint(..);

        // Confirm discovery has observed B at least once before clearing the resolver.
        assert_until!(peers_contains(&store_a, addr_b));

        // Remove B from discovery; floor is 100 ms, so after enough time B is decommissioned.
        discovery.set(vec![]);

        // Wait for decommission: floor (100 ms) + miss_threshold (2 × 20 ms) + slack.
        assert_until!(!members_snapshot(&store_a).contains(&addr_b));

        // Once B is gone the tombstone should be GC'd.
        assert_until!(store_a.fingerprint(..) != hash_with_tombstone);

        task_a.abort();
    }

    /// **Test 3**: a member with NO pending tombstone acks is decommissioned after exactly
    /// `miss_threshold` rounds, with no wall-time floor delay — the existing fast path is
    /// preserved.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_pending_tombstones_decommissions_at_miss_threshold() {
        let port = 8100u16;
        let addr_a: IpAddr = "127.0.0.92".parse().unwrap();
        let addr_b: IpAddr = "127.0.0.93".parse().unwrap();

        let discovery = ScriptedDiscovery::new(vec![addr_b]);
        // Floor is 10 s; it must NOT delay decommission when there are no pending acks.
        let store_a = ReconcileStore::<i32, i32>::new(floor_cfg(addr_a, port))
            .await
            .expect("bind failed")
            .with_seed(addr_b)
            .with_discovery(Arc::new(discovery.clone()))
            .with_discovery_interval(Duration::from_millis(20))
            .with_discovery_miss_threshold(2)
            .with_discovery_decommission_floor(Duration::from_secs(10));
        let store_b = ReconcileStore::<i32, i32>::new(floor_cfg(addr_b, port))
            .await
            .expect("bind failed")
            .with_seed(addr_a);

        let task_a = tokio::spawn(store_a.clone().run());
        let task_b = tokio::spawn(store_b.clone().run());

        // Establish mutual membership (no tombstones involved).
        store_a.insert(100, 1);
        assert_until!(store_b.get(&100).as_deref() == Some(&1));
        store_b.insert(200, 2);
        assert_until!(store_a.get(&200).as_deref() == Some(&2));

        // Confirm discovery has observed B at least once (so the `seen_ever` guard inside
        // `discover_periodically` will not skip B when it goes absent) before clearing the
        // resolver. Without this wait the clear can race ahead of the first discovery round and
        // B would never enter `seen_ever`, causing the decommission logic to skip it forever.
        assert_until!(peers_contains(&store_a, addr_b));

        // Abort B and remove it from discovery; no tombstones are outstanding.
        task_b.abort();
        discovery.set(vec![]);

        // B should be decommissioned promptly (miss_threshold rounds, no floor delay).
        assert_until!(!members_snapshot(&store_a).contains(&addr_b));

        task_a.abort();
    }

    /// **Test 4**: reappearance resets the absence clock.  Scenario: B vanishes, then reappears
    /// before the floor, then vanishes again.  The floor is measured from the *second* vanish.
    #[tokio::test(flavor = "multi_thread")]
    async fn reappearance_resets_absence_clock() {
        let port = 8101u16;
        let addr_a: IpAddr = "127.0.0.94".parse().unwrap();
        let addr_b: IpAddr = "127.0.0.95".parse().unwrap();

        let discovery = ScriptedDiscovery::new(vec![addr_b]);
        // 200 ms floor; short enough to test elapsed in the second-absence window.
        let store_a = ReconcileStore::<i32, i32>::new(floor_cfg(addr_a, port))
            .await
            .expect("bind failed")
            .with_seed(addr_b)
            .with_tombstone_timeout(Duration::from_millis(50))
            .with_discovery(Arc::new(discovery.clone()))
            .with_discovery_interval(Duration::from_millis(20))
            .with_discovery_miss_threshold(2)
            .with_discovery_decommission_floor(Duration::from_millis(200));
        let store_b = ReconcileStore::<i32, i32>::new(floor_cfg(addr_b, port))
            .await
            .expect("bind failed")
            .with_seed(addr_a)
            .with_tombstone_timeout(Duration::from_millis(50));

        let task_a = tokio::spawn(store_a.clone().run());
        let task_b = tokio::spawn(store_b.clone().run());

        // Establish mutual membership and leave a tombstone pending.
        store_a.insert(1000, 42);
        assert_until!(store_b.get(&1000).as_deref() == Some(&42));
        store_b.insert(2000, 84);
        assert_until!(store_a.get(&2000).as_deref() == Some(&84));

        task_b.abort();
        store_a.remove(&1000);
        let hash_with_tombstone = store_a.fingerprint(..);

        // Confirm discovery has observed B at least once before clearing the resolver so the
        // `seen_ever` guard inside `discover_periodically` tracks B correctly.
        assert_until!(peers_contains(&store_a, addr_b));

        // First absence: vanish for a bit but reappear before the floor.
        discovery.set(vec![]);
        // Less than the 200 ms floor; absence clock must not yet have triggered decommission.
        tokio::time::sleep(Duration::from_millis(80)).await;
        // Reappear: absence clock must reset.
        discovery.set(vec![addr_b]);
        tokio::time::sleep(Duration::from_millis(60)).await;

        // B must still be a member (the first-absence clock was reset on reappearance).
        assert!(
            members_snapshot(&store_a).contains(&addr_b),
            "B was decommissioned during or just after the first absence window"
        );
        assert_eq!(
            store_a.fingerprint(..),
            hash_with_tombstone,
            "tombstone GC'd during the first absence window"
        );

        // Second absence: remove B again; floor is 200 ms.
        discovery.set(vec![]);

        // Wait for the floor measured from the second vanish to elapse.
        assert_until!(!members_snapshot(&store_a).contains(&addr_b));
        assert_until!(store_a.fingerprint(..) != hash_with_tombstone);

        task_a.abort();
    }

    /// **Test 5 — zero-ack deletion**: the floor must hold for a tombstone that has received no
    /// acknowledgment at all.
    ///
    /// A tombstone nobody has acked yet has no `tombstone_acks` entry, so a pending-acks
    /// predicate that iterates `tombstone_acks` cannot see it and reports "nothing pending" —
    /// triggering the fast-path decommission while `is_tombstone_stable` is still false, letting
    /// GC collect the tombstone and opening the resurrection window.  The predicate iterates the
    /// map itself (the authoritative domain), so a fresh tombstone with zero acks is pending.
    ///
    /// Construction: B's run loop is aborted and **awaited to full termination before the
    /// deletion is issued**, so the tombstone cannot reach B and zero acks is guaranteed by
    /// ordering (asserted below), not by racing the abort.  A's config deliberately has NO
    /// probe net: with the usual self-/32 net, A random-probes its own address, reconciles
    /// with itself, and acks its own tombstone — which would break the zero-ack invariant
    /// this test exists to exercise.  The `peers_contains` wait ensures discovery observed B
    /// beforehand (the `seen_ever` guard admits B to the decommission path), and B stays in
    /// `members` from the pre-deletion exchange.
    #[tokio::test(flavor = "multi_thread")]
    async fn zero_ack_deletion_floor_holds() {
        let port = 8102u16;
        let addr_a: IpAddr = "127.0.0.96".parse().unwrap();
        let addr_b: IpAddr = "127.0.0.97".parse().unwrap();

        let discovery = ScriptedDiscovery::new(vec![addr_b]);
        // Floor is 10 s — far longer than the test window, so it must never elapse here.
        // No `.with_net(...)`: seeded/discovered peers are contacted every round regardless;
        // only the self-probing has to go.
        let store_a = ReconcileStore::<i32, i32>::new(
            Config::default().with_port(port).with_listen_addr(addr_a),
        )
        .await
        .expect("bind failed")
        .with_seed(addr_b)
        .with_tombstone_timeout(Duration::from_millis(50))
        .with_discovery(Arc::new(discovery.clone()))
        .with_discovery_interval(Duration::from_millis(20))
        .with_discovery_miss_threshold(2)
        .with_discovery_decommission_floor(Duration::from_secs(10));
        let store_b = ReconcileStore::<i32, i32>::new(floor_cfg(addr_b, port))
            .await
            .expect("bind failed")
            .with_seed(addr_a)
            .with_tombstone_timeout(Duration::from_millis(50));

        let task_a = tokio::spawn(store_a.clone().run());
        let task_b = tokio::spawn(store_b.clone().run());

        // Establish mutual membership so B enters the causal-stability gate.
        store_a.insert(42, 1);
        assert_until!(store_b.get(&42).as_deref() == Some(&1));
        store_b.insert(99, 2);
        assert_until!(store_a.get(&99).as_deref() == Some(&2));

        // Wait until B is in A's membership set (the causal-stability gate that blocks tombstone
        // GC). This requires a dated datagram from B to have been processed by A, which the data
        // exchange above triggers. We wait explicitly rather than relying on implicit timing.
        assert_until!(members_snapshot(&store_a).contains(&addr_b));

        // Wait until discovery has seeded B in the peers map, confirming that at least one
        // discovery round has run and B will enter `seen_ever` inside `discover_periodically`.
        assert_until!(peers_contains(&store_a, addr_b));

        // Terminate B completely BEFORE the deletion exists: abort the run loop and await the
        // join handle, so B can never receive the tombstone, never ack it, and never send
        // another dated datagram that would re-assert its membership.
        task_b.abort();
        let _ = task_b.await;
        drop(store_b);

        store_a.remove(&42);
        let hash_with_tombstone = store_a.fingerprint(..);
        discovery.set(vec![]);

        // Allow miss_threshold rounds to pass (2 × 20 ms + slack) but well within the 10 s floor.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Pin the construction: the tombstone for key 42 must have received no acks. If this
        // ever fires, the ordering guarantee above broke and the test no longer exercises the
        // zero-ack path it exists for.
        assert_eq!(
            tombstone_acks_len(&store_a),
            0,
            "construction broken: an ack arrived for the tombstone after B was terminated"
        );

        // Core assertions: with zero acks, a predicate blind to un-acked tombstones would have
        // taken the fast path and decommissioned B at miss_threshold (~40 ms); the floor must
        // instead hold B's membership and keep the tombstone un-GC'd.
        assert!(
            members_snapshot(&store_a).contains(&addr_b),
            "B was prematurely decommissioned despite a zero-ack pending tombstone \
             (resurrection hazard)"
        );
        assert_eq!(
            store_a.fingerprint(..),
            hash_with_tombstone,
            "tombstone GC'd despite zero acks from B (resurrection hazard)"
        );

        task_a.abort();
    }
}
