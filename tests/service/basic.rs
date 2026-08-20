// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Core LWW convergence: bulk sync, single-key read/write/delete, causal ordering, and
//! concurrent-write tie-breaking. No auth, encryption, topology, or tombstone-GC concerns.

use rand::{
    distributions::{Alphanumeric, DistString},
    Rng, SeedableRng,
};

use tokio_util::sync::CancellationToken;

use reconcile::{clock::NodeId, replicated_map::Config, Fingerprint, ReplicatedMap};

use crate::support::assert_until;

#[tokio::test(flavor = "multi_thread")]
async fn test() {
    let port = 8080;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.44".parse().unwrap();
    let addr2 = "127.0.0.45".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_insecure_no_key();

    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let key_values: [(String, String); 1000] = core::array::from_fn(|_| {
        let key: String = Alphanumeric.sample_string(&mut rng, 100);
        let value: String = Alphanumeric.sample_string(&mut rng, 100);
        (key, value)
    });

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    store1.insert_bulk(&key_values);
    let start_fingerprint = store1.fingerprint(..);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    // Check the initial state *before* spawning the run loops: store1's `insert_bulk` already
    // spawned a background broadcast to its seeded peer (store2), so once store2 starts
    // receiving these asserts would race with reconciliation.
    assert_eq!(store2.fingerprint(..), Fingerprint::ZERO);
    assert_eq!(store1.fingerprint(..), start_fingerprint);
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));

    assert_until!(store2.fingerprint(..) == start_fingerprint);

    assert_eq!(store1.fingerprint(..), start_fingerprint);

    let key = "42".to_string();
    let value = "Hello, World!".to_string();
    store2.insert(key.clone(), value.clone());
    assert_until!(store1.get(&key).as_deref() == Some(&value));

    store1.remove(&key);
    assert_until!(store2.get(&key).is_none());

    // A causally later write must win. Causality is established by waiting for the first value
    // to propagate — not by wall-clock order, which for concurrent writes means nothing.
    let key = "42".to_string();
    for i in 0..20 {
        // Unique values per iteration so each `assert_until` observes *this* write, not a value
        // left over from a previous iteration.
        let first = format!("first-{i}");
        let second = format!("second-{i}");
        if rng.gen() {
            // store1 writes, store2 observes it, then store2 overwrites: store2's value wins.
            store1.insert(key.clone(), first.clone());
            assert_until!(store2.get(&key).as_deref() == Some(&first));
            store2.insert(key.clone(), second.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&second));
            assert_until!(store2.get(&key).as_deref() == Some(&second));
        } else if rng.gen() {
            // Symmetric: store2 writes first, store1 observes, then store1 wins.
            store2.insert(key.clone(), first.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&first));
            store1.insert(key.clone(), second.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&second));
            assert_until!(store2.get(&key).as_deref() == Some(&second));
        } else if rng.gen() {
            // value then tombstone: the deletion observes the value and wins.
            store1.insert(key.clone(), first.clone());
            assert_until!(store2.get(&key).as_deref() == Some(&first));
            store2.remove(&key);
            assert_until!(store1.get(&key).is_none());
            assert_until!(store2.get(&key).is_none());
        } else {
            // tombstone then value: the insert observes the tombstone and wins.
            store1.insert(key.clone(), first.clone());
            assert_until!(store2.get(&key).as_deref() == Some(&first));
            store1.remove(&key);
            assert_until!(store2.get(&key).is_none());
            store2.insert(key.clone(), second.clone());
            assert_until!(store1.get(&key).as_deref() == Some(&second));
            assert_until!(store2.get(&key).as_deref() == Some(&second));
        }
    }

    // A newer value can overwrite a tombstone.
    let key = "43".to_string();
    let value1 = "Hello, World!".to_string();
    let value2 = "Goodbye!".to_string();
    store1.insert(key.clone(), value1.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&value1));
    store2.remove(&key);
    assert_until!(store1.get(&key).is_none());
    store1.insert(key.clone(), value2.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&value2));

    task2.abort();
    task1.abort();
}

/// An in-place `get_mut` mutation must be re-stamped and broadcast, so peers converge to the
/// edited value exactly as for an `insert`.
#[tokio::test(flavor = "multi_thread")]
async fn get_mut_edit_propagates_to_peers() {
    let port = 8089;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.100".parse().unwrap();
    let addr2 = "127.0.0.101".parse().unwrap();
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_insecure_no_key();

    let store1 = ReplicatedMap::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    let store2 = ReplicatedMap::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

    let key = "k".to_string();
    let before = "before".to_string();
    let after = "after".to_string();

    store1.insert(key.clone(), before.clone());
    assert_until!(store2.get(&key).as_deref() == Some(&before));

    store1.get_mut(&key, |v| {
        if let Some(v) = v {
            *v = after.clone();
        }
    });
    assert_eq!(store1.get(&key).as_deref(), Some(&after));

    // The crux: the in-place edit must reach store2. This fails before the fix, because `get_mut`
    // did not re-stamp the timestamp or broadcast the change.
    assert_until!(store2.get(&key).as_deref() == Some(&after));

    task1.abort();
    task2.abort();
}

/// Two replicas writing different values to one key concurrently must converge on one value with
/// matching fingerprints.
///
/// A non-commutative tie-break would leave each keeping its own, and since the timestamp is part
/// of the fingerprint, re-exchanging the pair forever. The assertions below time out if so.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_converge() {
    let port = 8083;
    let net = "127.0.0.1/8".parse().unwrap();
    let addr1 = "127.0.0.80".parse().unwrap();
    let addr2 = "127.0.0.81".parse().unwrap();
    // Fixed, distinct node ids give a deterministic conflict winner (the higher id).
    let cfg1 = Config::default()
        .with_port(port)
        .with_listen_addr(addr1)
        .with_net(net)
        .with_node_id(NodeId::new(1))
        .with_insecure_no_key();
    let cfg2 = Config::default()
        .with_port(port)
        .with_listen_addr(addr2)
        .with_net(net)
        .with_node_id(NodeId::new(2))
        .with_insecure_no_key();

    let store1 = ReplicatedMap::<String, String>::new(cfg1)
        .await
        .expect("bind failed")
        .with_seed(addr2);
    let store2 = ReplicatedMap::<String, String>::new(cfg2)
        .await
        .expect("bind failed")
        .with_seed(addr1);
    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

    // Hammer the same key from both nodes with different values, back to back, so that some
    // writes race closely in time.
    let key = "contended".to_string();
    for i in 0..50 {
        store1.insert(key.clone(), format!("from-1-{i}"));
        store2.insert(key.clone(), format!("from-2-{i}"));
    }

    // Both replicas must converge: identical fingerprints over the whole range, and the same
    // value for the contended key. (A surviving divergence/livelock would never equalize.)
    assert_until!(store1.fingerprint(..) == store2.fingerprint(..));
    let v1 = store1.get(&key).map(|g| g.clone());
    let v2 = store2.get(&key).map(|g| g.clone());
    assert_eq!(
        v1, v2,
        "replicas disagree on the contended key: {v1:?} vs {v2:?}"
    );
    assert!(v1.is_some(), "the contended key vanished entirely");

    task1.abort();
    task2.abort();
}
