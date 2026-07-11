//! Tests for the rounded-out write API on `ReconcileStore`: atomic read-modify-write
//! (`update`/`upsert`/`get_or_insert_with`), bulk/range/predicate deletes
//! (`clear`/`retain`/`delete_range`), and the no-broadcast `load_bulk` seed path. Deletes and
//! updates must reconcile to a peer (not merely mutate locally), so the propagating cases use a
//! two-store cluster.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reconcile::{reconcile_store::Config, ReconcileStore};

async fn wait_until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..300 {
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

fn config(port: u16, addr: &str) -> Config {
    Config::default()
        .with_port(port)
        .with_listen_addr(addr.parse().unwrap())
        .with_net("127.0.0.1/8".parse().unwrap())
}

async fn isolated(port: u16, addr: &str) -> ReconcileStore<i32, i32> {
    ReconcileStore::new(config(port, addr))
        .await
        .expect("bind failed")
}

// --- Atomic read-modify-write (single node is enough for the semantics) -----------------------

#[tokio::test(flavor = "multi_thread")]
async fn update_mutates_present_and_reports_absent() {
    let store = isolated(8310, "127.0.0.210").await;
    store.insert(1, 10);

    assert!(store.update(&1, |v| *v += 5), "update reports the key existed");
    assert_eq!(store.get(&1).as_deref(), Some(&15));

    assert!(
        !store.update(&99, |v| *v += 1),
        "update on an absent key reports false"
    );
    assert!(store.get(&99).is_none(), "update must not create the key");

    // A tombstoned key is treated as absent, and update must not resurrect it.
    store.remove(&1);
    assert!(!store.update(&1, |v| *v += 1));
    assert!(store.get(&1).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_updates_or_inserts() {
    let store = isolated(8311, "127.0.0.211").await;

    // Absent: inserts the default.
    store.upsert(1, 100, |v| *v += 1);
    assert_eq!(store.get(&1).as_deref(), Some(&100));

    // Present: mutates in place, default ignored.
    store.upsert(1, 999, |v| *v += 5);
    assert_eq!(store.get(&1).as_deref(), Some(&105));
}

#[tokio::test(flavor = "multi_thread")]
async fn get_or_insert_with_inserts_only_when_absent() {
    let store = isolated(8312, "127.0.0.212").await;

    assert_eq!(store.get_or_insert_with(&1, || 42), 42);
    assert_eq!(store.get(&1).as_deref(), Some(&42));

    // Present: returns the existing value and never calls the closure.
    let called = AtomicBool::new(false);
    let got = store.get_or_insert_with(&1, || {
        called.store(true, Ordering::SeqCst);
        7
    });
    assert_eq!(got, 42);
    assert!(!called.load(Ordering::SeqCst), "closure must not run when present");
}

// --- Bulk/range/predicate deletes must propagate as tombstones --------------------------------

/// Build two cross-seeded, running stores; store1 loaded with 1..=5, converged to store2.
async fn converged_pair(
    port: u16,
    a1: &str,
    a2: &str,
) -> (
    ReconcileStore<i32, i32>,
    ReconcileStore<i32, i32>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let store1 = ReconcileStore::<i32, i32>::new(config(port, a1))
        .await
        .expect("bind failed")
        .with_seed(a2.parse().unwrap());
    let store2 = ReconcileStore::<i32, i32>::new(config(port, a2))
        .await
        .expect("bind failed")
        .with_seed(a1.parse().unwrap());
    for k in 1..=5 {
        store1.insert(k, k * 10);
    }
    let t1 = tokio::spawn(store1.clone().run());
    let t2 = tokio::spawn(store2.clone().run());
    (store1, store2, t1, t2)
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_range_propagates_and_excludes() {
    let (store1, store2, t1, t2) = converged_pair(8313, "127.0.0.213", "127.0.0.214").await;
    assert_until!(store2.get(&3).as_deref() == Some(&30));

    store1.delete_range(2..4); // deletes 2 and 3 (exclusive end)

    // Excluded locally at once, and the tombstones reconcile to the peer.
    assert!(store1.get(&2).is_none() && store1.get(&3).is_none());
    assert!(store1.get(&1).is_some() && store1.get(&4).is_some());
    assert_until!(store2.get(&2).is_none() && store2.get(&3).is_none());
    assert!(store2.get(&1).as_deref() == Some(&10) && store2.get(&4).as_deref() == Some(&40));

    t1.abort();
    t2.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn retain_propagates_and_excludes() {
    let (store1, store2, t1, t2) = converged_pair(8314, "127.0.0.215", "127.0.0.216").await;
    assert_until!(store2.get(&5).as_deref() == Some(&50));

    store1.retain(|k, _| k % 2 == 0); // keep evens, tombstone odds (1, 3, 5)

    assert!(store1.get(&1).is_none() && store1.get(&3).is_none() && store1.get(&5).is_none());
    assert!(store1.get(&2).is_some() && store1.get(&4).is_some());
    assert_until!(store2.get(&1).is_none() && store2.get(&5).is_none());
    assert!(store2.get(&2).as_deref() == Some(&20) && store2.get(&4).as_deref() == Some(&40));

    t1.abort();
    t2.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_propagates() {
    let (store1, store2, t1, t2) = converged_pair(8315, "127.0.0.217", "127.0.0.218").await;
    assert_until!(store2.get(&1).as_deref() == Some(&10));

    store1.clear();

    for k in 1..=5 {
        assert!(store1.get(&k).is_none());
    }
    assert_until!((1..=5).all(|k| store2.get(&k).is_none()));

    t1.abort();
    t2.abort();
}

// --- load_bulk seeds locally, then converges only via anti-entropy ----------------------------

#[tokio::test(flavor = "multi_thread")]
async fn load_bulk_seeds_locally_then_converges() {
    let store1 = ReconcileStore::<i32, i32>::new(config(8316, "127.0.0.219"))
        .await
        .expect("bind failed")
        .with_seed("127.0.0.220".parse().unwrap());
    let store2 = ReconcileStore::<i32, i32>::new(config(8316, "127.0.0.220"))
        .await
        .expect("bind failed")
        .with_seed("127.0.0.219".parse().unwrap());

    let seed: Vec<(i32, i32)> = (1..=4).map(|k| (k, k * 10)).collect();
    store1.load_bulk(&seed);

    // Applied locally immediately, without any broadcast having been spawned.
    assert_eq!(store1.get(&1).as_deref(), Some(&10));
    assert_eq!(store1.get(&4).as_deref(), Some(&40));

    // Converges to the peer through the periodic anti-entropy round once both run.
    let t1 = tokio::spawn(store1.clone().run());
    let t2 = tokio::spawn(store2.clone().run());
    assert_until!((1..=4).all(|k| store2.get(&k).as_deref() == Some(&(k * 10))));

    t1.abort();
    t2.abort();
}
