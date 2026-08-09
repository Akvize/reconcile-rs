// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Tests for the collection-shaped read API on `ReplicatedMap`
//! (`len`/`is_empty`/`contains_key`/`for_each`/`for_each_in_range`/`to_vec`/`range_to_vec`/
//! `keys`/`values`/`first_key_value`/`last_key_value`), including that tombstoned entries are
//! excluded from every accessor.

use std::sync::Arc;

use reconcile::{replicated_map::Config, InMemoryNetwork, ReplicatedMap};

/// A single isolated store over an in-memory transport, so nothing is broadcast or GC'd and the
/// reads see exactly what was written locally (tombstones included, until a live read filters
/// them).
fn isolated_store(addr: &str) -> ReplicatedMap<i32, i32> {
    let network = InMemoryNetwork::new();
    let transport = Arc::new(network.bind(format!("{addr}:8300").parse().unwrap()));
    ReplicatedMap::new_with_transport(Config::default(), transport)
}

#[test]
fn empty_store_reads() {
    let store = isolated_store("127.0.0.1");

    assert_eq!(store.len(), 0);
    assert!(store.is_empty());
    assert!(!store.contains_key(&1));
    assert!(store.to_vec().is_empty());
    assert!(store.range_to_vec(..).is_empty());
    assert!(store.keys().is_empty());
    assert!(store.values().is_empty());
    assert_eq!(store.first_key_value(), None);
    assert_eq!(store.last_key_value(), None);

    let mut seen = 0;
    store.for_each(|_, _| seen += 1);
    assert_eq!(seen, 0);
}

#[test]
fn live_reads_and_ranges() {
    let store = isolated_store("127.0.0.2");
    for k in 1..=5 {
        store.just_insert(k, k * 10);
    }

    assert_eq!(store.len(), 5);
    assert!(!store.is_empty());
    assert!(store.contains_key(&3));
    assert!(!store.contains_key(&99));

    // Ordered owned snapshots.
    assert_eq!(
        store.to_vec(),
        vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)]
    );
    assert_eq!(store.keys(), vec![1, 2, 3, 4, 5]);
    assert_eq!(store.values(), vec![10, 20, 30, 40, 50]);
    assert_eq!(store.first_key_value(), Some((1, 10)));
    assert_eq!(store.last_key_value(), Some((5, 50)));

    // Callback visits every live entry, in order.
    let mut collected = Vec::new();
    store.for_each(|k, v| collected.push((*k, *v)));
    assert_eq!(collected, store.to_vec());

    // Range bounds: exclusive, inclusive, unbounded-below, unbounded-above.
    assert_eq!(store.range_to_vec(2..4), vec![(2, 20), (3, 30)]);
    assert_eq!(store.range_to_vec(2..=4), vec![(2, 20), (3, 30), (4, 40)]);
    assert_eq!(store.range_to_vec(..3), vec![(1, 10), (2, 20)]);
    assert_eq!(store.range_to_vec(4..), vec![(4, 40), (5, 50)]);
    assert_eq!(store.range_to_vec(..).len(), 5);

    let mut in_range = Vec::new();
    store.for_each_in_range(2..=4, |k, v| in_range.push((*k, *v)));
    assert_eq!(in_range, vec![(2, 20), (3, 30), (4, 40)]);
}

#[test]
fn tombstoned_entries_are_excluded() {
    let store = isolated_store("127.0.0.3");
    for k in 1..=3 {
        store.just_insert(k, k * 10);
    }
    // Delete key 2: it becomes a tombstone in the map (no peers, not GC'd), and must vanish from
    // every live read.
    store.just_remove(&2);

    assert_eq!(store.len(), 2);
    assert!(!store.is_empty());
    assert!(!store.contains_key(&2));
    assert!(store.contains_key(&1));

    assert_eq!(store.to_vec(), vec![(1, 10), (3, 30)]);
    assert_eq!(store.keys(), vec![1, 3]);
    assert_eq!(store.values(), vec![10, 30]);
    assert_eq!(store.range_to_vec(..), vec![(1, 10), (3, 30)]);
    // A range that spans only the tombstoned key yields nothing.
    assert!(store.range_to_vec(2..3).is_empty());

    let mut collected = Vec::new();
    store.for_each(|k, v| collected.push((*k, *v)));
    assert_eq!(collected, vec![(1, 10), (3, 30)]);

    // Deleting every remaining key leaves the store empty of live entries (only tombstones remain).
    store.just_remove(&1);
    store.just_remove(&3);
    assert_eq!(store.len(), 0);
    assert!(store.is_empty());
    assert!(store.to_vec().is_empty());
}

/// `first_key_value`/`last_key_value` must skip past a tombstone sitting exactly at the extremal
/// (smallest/largest) raw key — the case that distinguishes them from a plain "smallest/largest raw
/// key" read.
#[test]
fn first_and_last_key_value_skip_boundary_tombstones() {
    let store = isolated_store("127.0.0.4");
    for k in 1..=5 {
        store.just_insert(k, k * 10);
    }
    assert_eq!(store.first_key_value(), Some((1, 10)));
    assert_eq!(store.last_key_value(), Some((5, 50)));

    // Tombstone both extremes: the live min/max must move inward to 2 and 4.
    store.just_remove(&1);
    store.just_remove(&5);
    assert_eq!(store.first_key_value(), Some((2, 20)));
    assert_eq!(store.last_key_value(), Some((4, 40)));

    // Tombstone everything but one middle key: it is both the min and the max.
    store.just_remove(&2);
    store.just_remove(&4);
    assert_eq!(store.first_key_value(), Some((3, 30)));
    assert_eq!(store.last_key_value(), Some((3, 30)));

    // No live entries left: both read `None`, not a stale/raw tombstoned key.
    store.just_remove(&3);
    assert_eq!(store.first_key_value(), None);
    assert_eq!(store.last_key_value(), None);
}
