// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::replicated_map::Config;
use crate::FingerprintTreeMap;
use rsos::Fingerprint;

fn ephemeral_config() -> Config {
    // A fresh port per call — Config::port must be nonzero — on the loopback default
    // network.
    Config::default()
        .with_port(crate::replica::tests::next_ephemeral_test_port())
        .with_insecure_no_key()
}

/// `get` returns the live value, and absent keys are `None`.
#[tokio::test]
async fn get_returns_integrated_value() {
    let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
        .await
        .expect("bind failed");
    assert!(read_replica.get(&1).is_none());
    read_replica.integrate(vec![(1, State::Present("hello".to_string()))]);
    assert_eq!(read_replica.get(&1).as_deref(), Some(&"hello".to_string()));
    assert!(read_replica.contains_key(&1));
    assert_eq!(read_replica.len(), 1);
}

/// `get_cloned` mirrors `get`, but owns rather than borrows: present for an integrated key,
/// `None` for one that was never integrated.
#[tokio::test]
async fn get_cloned_returns_an_owned_copy_or_none_for_a_missing_key() {
    let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
        .await
        .expect("bind failed");
    assert_eq!(read_replica.get_cloned(&1), None);
    read_replica.integrate(vec![(1, State::Present("hello".to_string()))]);
    assert_eq!(read_replica.get_cloned(&1), Some("hello".to_string()));
    assert_eq!(read_replica.get_cloned(&2), None);
}

/// A replicated tombstone (`State::Tombstone`) hides the value but is still a stored entry.
#[tokio::test]
async fn replicates_tombstones() {
    let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
        .await
        .expect("bind failed");
    read_replica.integrate(vec![(1, State::Present("v".to_string()))]);
    assert_eq!(read_replica.get(&1).as_deref(), Some(&"v".to_string()));

    // A later tombstone overwrites it: the value disappears from `get`, and it no longer counts
    // as a live entry (a read replica has no timestamp and trusts the authoritative peer).
    read_replica.integrate(vec![(1, State::Tombstone)]);
    assert!(read_replica.get(&1).is_none());
    assert!(!read_replica.contains_key(&1));
    assert_eq!(read_replica.len(), 0, "the tombstone is not a live entry");
    // The tombstone itself is still retained internally (the tree keeps it until the dated peer
    // observes it acknowledged and moves on) — `len` deliberately doesn't surface that raw size.
    assert_eq!(
        read_replica.tree.read().len(),
        1,
        "the tombstone is retained as a tree entry"
    );
}

/// The collection-shaped read API (`for_each`/`for_each_in_range`/`to_vec`/`range_to_vec`/
/// `keys`/`values`) mirrors [`ReplicatedMap`](crate::ReplicatedMap)'s: live entries only, in key
/// order, tombstones excluded.
#[tokio::test]
async fn collection_reads_exclude_tombstones() {
    let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    read_replica.integrate(vec![
        (1, State::Present(10)),
        (2, State::Present(20)),
        (3, State::Tombstone),
        (4, State::Present(40)),
    ]);

    assert_eq!(read_replica.to_vec(), vec![(1, 10), (2, 20), (4, 40)]);
    assert_eq!(read_replica.keys(), vec![1, 2, 4]);
    assert_eq!(read_replica.values(), vec![10, 20, 40]);
    assert_eq!(read_replica.range_to_vec(2..=3), vec![(2, 20)]);
    assert!(read_replica.range_to_vec(3..3).is_empty());

    let mut collected = Vec::new();
    read_replica.for_each(|k, v| collected.push((*k, *v)));
    assert_eq!(collected, read_replica.to_vec());

    let mut in_range = Vec::new();
    read_replica.for_each_in_range(2.., |k, v| in_range.push((*k, *v)));
    assert_eq!(in_range, vec![(2, 20), (4, 40)]);
}

/// `first_key_value`/`last_key_value` skip a tombstone sitting at the extremal raw key,
/// mirroring [`ReplicatedMap`](crate::ReplicatedMap)'s.
#[tokio::test]
async fn first_and_last_key_value_skip_boundary_tombstones() {
    let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    assert_eq!(read_replica.first_key_value(), None);
    assert_eq!(read_replica.last_key_value(), None);

    read_replica.integrate(vec![
        (1, State::Tombstone),
        (2, State::Present(20)),
        (3, State::Present(30)),
        (4, State::Present(40)),
        (5, State::Tombstone),
    ]);
    assert_eq!(read_replica.first_key_value(), Some((2, 20)));
    assert_eq!(read_replica.last_key_value(), Some((4, 40)));

    read_replica.integrate(vec![(2, State::Tombstone), (4, State::Tombstone)]);
    assert_eq!(read_replica.first_key_value(), Some((3, 30)));
    assert_eq!(read_replica.last_key_value(), Some((3, 30)));

    read_replica.integrate(vec![(3, State::Tombstone)]);
    assert_eq!(read_replica.first_key_value(), None);
    assert_eq!(read_replica.last_key_value(), None);
}

/// The on-update hook fires for every integrated value, including tombstones.
#[tokio::test]
async fn on_update_hook_fires() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    read_replica.set_on_update(move |_, _| {
        count2.fetch_add(1, Ordering::SeqCst);
    });
    read_replica.integrate(vec![(1, State::Present(10)), (2, State::Tombstone)]);
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

/// A second `set_on_update` call replaces the first hook rather than adding to it.
#[tokio::test]
async fn set_on_update_replaces_previous_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    let first_count = Arc::new(AtomicUsize::new(0));
    let second_count = Arc::new(AtomicUsize::new(0));

    let first_count2 = first_count.clone();
    read_replica.set_on_update(move |_, _| {
        first_count2.fetch_add(1, Ordering::SeqCst);
    });
    let second_count2 = second_count.clone();
    read_replica.set_on_update(move |_, _| {
        second_count2.fetch_add(1, Ordering::SeqCst);
    });

    read_replica.integrate(vec![(1, State::Present(10))]);

    assert_eq!(first_count.load(Ordering::SeqCst), 0);
    assert_eq!(second_count.load(Ordering::SeqCst), 1);
}

/// The read replica's value-only fingerprint matches an independently-built tree of the same
/// logical content — i.e. timestamps genuinely play no part in the hash.
#[tokio::test]
async fn value_fingerprint_is_timestamp_independent() {
    let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
        .await
        .expect("bind failed");
    read_replica.integrate(vec![
        (1, State::Present("a".to_string())),
        (2, State::Tombstone),
    ]);

    let mut reference: FingerprintTreeMap<i32, State<String>> = FingerprintTreeMap::new();
    reference.insert(1, State::Present("a".to_string()));
    reference.insert(2, State::Tombstone);

    assert_eq!(
        read_replica.value_fingerprint(..),
        reference.aggregate(..).fingerprint()
    );
}

/// #294: the deprecated `fingerprint` alias must actually forward to `value_fingerprint`, not
/// just compile — a mutant that no-ops it and returns a default `Fingerprint` would pass any
/// test that never compares its result to the real one.
#[test]
#[allow(deprecated)]
fn deprecated_fingerprint_alias_matches_value_fingerprint() {
    let read_replica = ReadReplicaMap::<i32, String>::new_with_transport(
        ephemeral_config(),
        Arc::new(crate::transport::InMemoryNetwork::new().bind("127.0.5.1:1".parse().unwrap())),
    );
    read_replica.integrate(vec![(1, State::Present("a".to_string()))]);
    assert_eq!(
        read_replica.fingerprint(..),
        read_replica.value_fingerprint(..)
    );
    assert_ne!(read_replica.fingerprint(..), Fingerprint::default());
}

/// #294: `start_reconciliation` (the public, buffer-owning wrapper) must actually send a
/// value-only comparison round to the seeded peer — a mutant that no-ops its body would leave
/// the peer's socket silent forever, which this test's `recv_from` would then time out on. No
/// `run()` loop is spawned on either side, so nothing but this call can produce the datagram.
#[tokio::test]
async fn start_reconciliation_wrapper_actually_transmits() {
    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = crate::replica::tests::next_ephemeral_test_port();
    let read_replica_addr: IpAddr = "127.0.5.2".parse().unwrap();
    let peer_addr: IpAddr = "127.0.5.3".parse().unwrap();
    let peer_transport = net.bind(SocketAddr::new(peer_addr, port));

    let read_replica = ReadReplicaMap::<i32, String>::new_with_transport(
        ephemeral_config()
            .with_port(port)
            .with_listen_addr(read_replica_addr),
        Arc::new(net.bind(SocketAddr::new(read_replica_addr, port))),
    )
    .with_seed(peer_addr);

    read_replica.start_reconciliation().await;

    let mut buf = [0u8; 65536];
    let (size, from) =
        tokio::time::timeout(Duration::from_secs(5), peer_transport.recv_from(&mut buf))
            .await
            .expect("start_reconciliation never sent anything to the seeded peer")
            .expect("recv_from failed");
    assert!(size > 0, "the datagram sent to the peer was empty");
    assert_eq!(from.ip(), read_replica_addr);
}

/// A live value and its `State` projection hash identically only via the value-only basis:
/// per-entry, the dateless read replica saves the whole `Timestamp` (the point of the dateless
/// read replica).
#[test]
fn value_only_is_smaller_per_entry() {
    let dated = std::mem::size_of::<Entry<Timestamp, u64>>();
    let light = std::mem::size_of::<State<u64>>();
    assert!(
        light < dated,
        "value-only entry ({light} B) should be smaller than dated entry ({dated} B)"
    );
}
