//! Regression test for the `TimeoutWheel` same-instant collision bug.
//!
//! When `remove_bulk` is called with ≥2 keys in the same millisecond their tombstone entries
//! all share the same `wall_ms`-derived `DateTime<Utc>`.  Before the fix only one of them
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

use reconcile::{reconcile_store::Config, Fingerprint, ReconcileStore};

/// Poll `f` up to 300 times, sleeping 10 ms between tries (~3 seconds total).
/// Returns `true` as soon as `f` becomes true, `false` if it never does.
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

/// Build a single-node store that is isolated from all other tests and whose `run()` loop
/// does not get stuck in a self-send tight loop.
///
/// Design choices:
///  * `listen_addr` is the address the socket binds to.
///  * `probe_net` is configured as the declared network.  It is deliberately DIFFERENT from
///    `listen_addr` so that `derive_local_net` cannot find `listen_addr` inside `probe_net`
///    and falls back to the `listen_addr/32` host route.  The only probe target is the
///    single address of `probe_net`; since nothing listens there, UDP fire-and-forget
///    succeeds but `recv_from` blocks for the full `reconcile_interval` (≈1 s) before
///    timing out — giving `clear_expired_tombstones` its scheduling window each cycle.
///  * A non-zero `port` is required; port 0 produces EINVAL when the engine sends to
///    `(probe_target, self.port)`.
///  * No peers are seeded, so `members` stays empty and `is_tombstone_stable` returns
///    `true` immediately — any expired tombstone is GC'd in the first sweep.
async fn isolated_store(
    listen_addr: IpAddr,
    probe_net: &str,
    port: u16,
    tombstone_timeout: Duration,
) -> ReconcileStore<i32, i32> {
    ReconcileStore::new(
        Config::default()
            .with_listen_addr(listen_addr)
            .with_net(probe_net.parse().unwrap())
            .with_port(port),
    )
    .await
    .expect("bind failed")
    .with_tombstone_timeout(tombstone_timeout)
}

/// Core regression: `remove_bulk` of N keys in the same millisecond must produce N
/// individually tracked tombstones, all of which expire and are GC'd.
///
/// Before the fix, the `TimeoutWheel` keyed on `DateTime<Utc>` alone, so keys sharing the
/// same millisecond timestamp would overwrite each other; only one tombstone would be yielded
/// by `expired()` and eventually GC'd, leaving the rest retained forever (fingerprint ≠ ZERO).
#[tokio::test(flavor = "multi_thread")]
async fn remove_bulk_same_millisecond_all_gc() {
    // Very short timeout so GC fires quickly.
    let timeout = Duration::from_millis(5);
    // Listen on 127.0.2.1, probe 127.0.3.0/32 (no listener there).
    // The engine sends reconciliation frames to 127.0.3.0 which silently drops them,
    // and recv_from waits for reconcile_interval (1 s) before retrying — giving
    // clear_expired_tombstones its scheduling window.
    let store = isolated_store("127.0.2.1".parse().unwrap(), "127.0.3.0/32", 19879, timeout).await;

    // Insert several values.
    let keys: Vec<i32> = (0..8).collect();
    for &k in &keys {
        store.insert(k, k * 10);
    }
    // Confirm they are present.
    for &k in &keys {
        assert_eq!(store.get(&k).as_deref(), Some(&(k * 10)));
    }

    // Capture the live fingerprint (non-zero, all keys present).
    let live_fp = store.fingerprint(..);
    assert_ne!(live_fp, Fingerprint::ZERO);

    // Delete all keys in one bulk call.  The HLC's physical component is derived from
    // `Utc::now()` in milliseconds; any two calls within the same millisecond share the
    // same `wall_ms`.  We cannot *force* a collision from outside the crate, but calling
    // `remove_bulk` atomically is the most likely scenario and exercises the pre-insert hook
    // code path that converts each tombstone's `wall_ms` to a `DateTime<Utc>` wheel instant.
    store.remove_bulk(&keys);

    // All keys should now be absent (tombstoned).
    for &k in &keys {
        assert!(store.get(&k).is_none(), "key {k} should be tombstoned");
    }

    // Fingerprint is non-zero: tombstones are still present in the tree.
    let tombstone_fp = store.fingerprint(..);
    assert_ne!(
        tombstone_fp,
        Fingerprint::ZERO,
        "fingerprint must be non-zero while tombstones are still tracked"
    );

    // Spawn `run()` which drives `clear_expired_tombstones()` on a 1-second GC loop.
    // The tombstone timeout is 5 ms so by the time the first GC sweep runs the tombstones
    // are already expired and will be collected.
    // On a single-node store with no peers, `is_tombstone_stable` returns `true` immediately
    // (empty membership set), so every expired tombstone is removed in the first sweep.
    let store2 = store.clone();
    let _task = tokio::spawn(store2.run());

    // Wait up to ~3 seconds for all tombstones to be GC'd.
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
