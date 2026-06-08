// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! End-to-end test of dynamic discovery through the public `run()` path: a peer that disappears
//! from the discovery source is decommissioned after the grace period, releasing the causal
//! stability gate that was holding back a tombstone's garbage collection.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reconcile::discovery::{DiscoverFuture, Discovery};
use reconcile::{reconcile_store::Config, ReconcileStore};

async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if f() {
            return true;
        }
    }
    false
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
        .with_seed(addr2)
        .with_tombstone_timeout(Duration::from_millis(50))
        .with_discovery(Arc::new(discovery.clone()))
        .with_discovery_interval(Duration::from_millis(20))
        .with_discovery_miss_threshold(3);
    let store2 = ReconcileStore::<i32, i32>::new(cfg2)
        .await
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

    // The peer now disappears from discovery: after the grace period it is decommissioned and the
    // tombstone becomes causally stable, so GC proceeds.
    discovery.set(vec![]);
    assert_until!(store1.fingerprint(..) != hash_with_tombstone);

    task1.abort();
}
