//! Tests for the collection-shaped read API on `ReconcileStore`
//! (`len`/`is_empty`/`contains_key`/`for_each`/`for_each_in_range`/`to_vec`/`range_to_vec`/
//! `keys`/`values`), including that tombstoned entries are excluded from every accessor.

use reconcile::{reconcile_store::Config, ReconcileStore};

/// A single isolated store, bound but never `run()`, so nothing is broadcast or GC'd and the reads
/// see exactly what was written locally (tombstones included, until a live read filters them).
async fn isolated_store(addr: &str) -> ReconcileStore<i32, i32> {
    let config = Config::default()
        .with_port(8300)
        .with_listen_addr(addr.parse().unwrap())
        .with_net("127.0.0.1/8".parse().unwrap());
    ReconcileStore::new(config).await.expect("bind failed")
}

#[tokio::test]
async fn empty_store_reads() {
    let store = isolated_store("127.0.0.200").await;

    assert_eq!(store.len(), 0);
    assert!(store.is_empty());
    assert!(!store.contains_key(&1));
    assert!(store.to_vec().is_empty());
    assert!(store.range_to_vec(..).is_empty());
    assert!(store.keys().is_empty());
    assert!(store.values().is_empty());

    let mut seen = 0;
    store.for_each(|_, _| seen += 1);
    assert_eq!(seen, 0);
}

#[tokio::test]
async fn live_reads_and_ranges() {
    let store = isolated_store("127.0.0.201").await;
    for k in 1..=5 {
        store.insert(k, k * 10);
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

    // Callback visits every live entry, in order.
    let mut collected = Vec::new();
    store.for_each(|k, v| collected.push((*k, *v)));
    assert_eq!(collected, store.to_vec());

    // Range bounds: exclusive, inclusive, unbounded-below, unbounded.
    assert_eq!(store.range_to_vec(2..4), vec![(2, 20), (3, 30)]);
    assert_eq!(store.range_to_vec(2..=4), vec![(2, 20), (3, 30), (4, 40)]);
    assert_eq!(store.range_to_vec(..3), vec![(1, 10), (2, 20)]);
    assert_eq!(store.range_to_vec(4..), vec![(4, 40), (5, 50)]);
    assert_eq!(store.range_to_vec(..).len(), 5);

    let mut in_range = Vec::new();
    store.for_each_in_range(2..=4, |k, v| in_range.push((*k, *v)));
    assert_eq!(in_range, vec![(2, 20), (3, 30), (4, 40)]);
}

#[tokio::test]
async fn tombstoned_entries_are_excluded() {
    let store = isolated_store("127.0.0.202").await;
    for k in 1..=3 {
        store.insert(k, k * 10);
    }
    // Delete key 2: it becomes a tombstone in the map (no peers, not GC'd), and must vanish from
    // every live read.
    store.remove(&2);

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
    store.remove(&1);
    store.remove(&3);
    assert_eq!(store.len(), 0);
    assert!(store.is_empty());
    assert!(store.to_vec().is_empty());
}
