//! Regression test for the `TimeoutWheel` same-instant collision bug.
//!
//! When `remove_bulk` is called with ≥2 keys in the same millisecond their tombstone entries
//! all share the same `physical`-derived `DateTime<Utc>`.  Before the fix only one of them
//! would survive in the wheel, the rest being silently overwritten.  After the fix all
//! tombstones must be individually tracked and, once the timeout elapses on a single-node
//! store (empty membership ⇒ `is_tombstone_stable` returns `true`), all of them must be
//! garbage-collected.
//!
//! Observable invariant tested here: after the tombstone timeout + GC interval, the store's
//! fingerprint must return to `Fingerprint::ZERO` (empty store), proving that every tombstone
//! was actually GC'd and not just "logically absent".

use std::net::IpAddr;
use std::time::Duration;

use reconcile::{replicated_map::Config, Fingerprint, ReplicatedMap};

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

/// A single-node store isolated from the other tests, whose `run()` loop leaves
/// `clear_expired_tombstones` a scheduling window.
///
/// `probe_net` differs from `listen_addr`, so the local net falls back to the host route and the
/// one probe target answers nothing, blocking `recv_from` for a full interval. `port` must be
/// non-zero (port 0 gives EINVAL on send). No peers, so any expired tombstone is stable at once.
async fn isolated_store(
    listen_addr: IpAddr,
    probe_net: &str,
    port: u16,
    tombstone_timeout: Duration,
) -> ReplicatedMap<i32, i32> {
    ReplicatedMap::new(
        Config::default()
            .with_listen_addr(listen_addr)
            .with_net(probe_net.parse().unwrap())
            .with_port(port),
    )
    .await
    .expect("bind failed")
    .with_tombstone_timeout(tombstone_timeout)
}

/// `remove_bulk` of N keys in one millisecond must produce N individually tracked tombstones,
/// all of which expire and are collected — a wheel keyed on the instant alone would keep one.
#[tokio::test(flavor = "multi_thread")]
async fn remove_bulk_same_millisecond_all_gc() {
    // Very short timeout so GC fires quickly.
    let timeout = Duration::from_millis(5);
    let store = isolated_store("127.0.2.1".parse().unwrap(), "127.0.3.0/32", 19879, timeout).await;

    let keys: Vec<i32> = (0..8).collect();
    for &k in &keys {
        store.insert(k, k * 10);
    }
    for &k in &keys {
        assert_eq!(store.get(&k).as_deref(), Some(&(k * 10)));
    }

    let live_fp = store.fingerprint(..);
    assert_ne!(live_fp, Fingerprint::ZERO);

    // A same-millisecond collision cannot be forced from outside the crate; one `remove_bulk` is
    // the likeliest way to get one.
    store.remove_bulk(&keys);

    for &k in &keys {
        assert!(store.get(&k).is_none(), "key {k} should be tombstoned");
    }

    let tombstone_fp = store.fingerprint(..);
    assert_ne!(
        tombstone_fp,
        Fingerprint::ZERO,
        "fingerprint must be non-zero while tombstones are still tracked"
    );

    // `run()` drives `clear_expired_tombstones()` on a 1-second GC loop; the 5 ms tombstone
    // timeout means they're already expired by the first sweep.
    let store2 = store.clone();
    let _task = tokio::spawn(store2.run());

    assert_until!(store.fingerprint(..) == Fingerprint::ZERO);
}

/// Sanity check: a single `remove` (no bulk collision possible) still GC's correctly.
/// This guards against regressions to the non-collision path.
#[tokio::test(flavor = "multi_thread")]
async fn single_remove_gc() {
    let timeout = Duration::from_millis(5);
    let store = isolated_store("127.0.2.2".parse().unwrap(), "127.0.3.1/32", 19880, timeout).await;

    store.insert(99, 990);
    assert_eq!(store.get(&99).as_deref(), Some(&990));
    store.remove(&99);
    assert!(store.get(&99).is_none());

    let store2 = store.clone();
    let _task = tokio::spawn(store2.run());

    assert_until!(store.fingerprint(..) == Fingerprint::ZERO);
}
