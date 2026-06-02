// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Interop between a dated [`ReconcileStore`] and a dateless [`ReconcileMirror`] (issue #128).
//!
//! These cover the two properties the lightweight-mirror design must guarantee:
//! 1. **Convergence**: the mirror receives the dated store's current values (timestamps dropped),
//!    mirrors deletions as tombstones, and its value-only fingerprint matches the dated store's
//!    value-only projection.
//! 2. **#109-safety**: a read-only mirror is never counted as a causal-stability member, so it
//!    cannot block the dated store's tombstone garbage collection.

use std::time::Duration;

use reconcile::{reconcile_store::Config, ReconcileMirror, ReconcileStore};

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

/// A dated store and a dateless mirror converge: the mirror ends up with every live value (no
/// timestamps), reflects a deletion as a tombstone, and its value-only fingerprint equals the dated
/// store's value-only projection fingerprint.
#[tokio::test(flavor = "multi_thread")]
async fn mirror_converges_with_dated_store() {
    let port = 8086;
    let peer_net = "127.0.0.1/8".parse().unwrap();
    let dated_addr = "127.0.0.90".parse().unwrap();
    let mirror_addr = "127.0.0.91".parse().unwrap();

    let dated = ReconcileStore::<String, String>::new(
        Config::default()
            .with_port(port)
            .with_listen_addr(dated_addr)
            .with_peer_net(peer_net),
    )
    .await;
    // Seed the mirror with the dated store's address: the mirror *drives* reconciliation over the
    // value-only channel, so it just needs to know where to send.
    let mirror = ReconcileMirror::<String, String>::new(
        Config::default()
            .with_port(port)
            .with_listen_addr(mirror_addr)
            .with_peer_net(peer_net),
    )
    .await
    .with_seed(dated_addr);

    // Populate the dated store with live values and one key we will later delete.
    for i in 0..50 {
        dated.insert(format!("k{i:02}"), format!("v{i:02}"));
    }
    dated.insert("doomed".to_string(), "to be deleted".to_string());

    let dated_task = tokio::spawn(dated.clone().run());
    let mirror_task = tokio::spawn(mirror.clone().run());

    // The mirror receives the latest values, with timestamps dropped.
    assert_until!(mirror.get(&"k00".to_string()).as_deref() == Some(&"v00".to_string()));
    assert_until!(mirror.get(&"k49".to_string()).as_deref() == Some(&"v49".to_string()));
    assert_until!(
        mirror.get(&"doomed".to_string()).as_deref() == Some(&"to be deleted".to_string())
    );

    // The value-only fingerprints converge: the mirror's tree hashes identically to the dated
    // store's value-only projection, even though the mirror never stored a single timestamp.
    assert_until!(mirror.fingerprint(..) == dated.value_fingerprint(..));
    assert_eq!(mirror.len(), 51);

    // A fresh write on the dated store propagates to the mirror.
    dated.insert("late".to_string(), "arrival".to_string());
    assert_until!(mirror.get(&"late".to_string()).as_deref() == Some(&"arrival".to_string()));

    // A deletion on the dated store is mirrored as a tombstone: the value disappears from `get`.
    dated.remove(&"doomed".to_string());
    assert_until!(mirror.get(&"doomed".to_string()).is_none());

    // ...and the value-only fingerprints reconverge after the deletion.
    assert_until!(mirror.fingerprint(..) == dated.value_fingerprint(..));

    dated_task.abort();
    mirror_task.abort();
}

/// A read-only mirror must never join the dated store's #109 membership set, otherwise it would
/// hold back tombstone garbage collection forever (it never acknowledges tombstones). With only a
/// mirror talking to it, the dated store must still collect an expired tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn mirror_does_not_block_tombstone_gc() {
    let port = 8087;
    let peer_net = "127.0.0.1/8".parse().unwrap();
    let dated_addr = "127.0.0.92".parse().unwrap();
    let mirror_addr = "127.0.0.93".parse().unwrap();

    // Aggressive tombstone expiry so GC would fire quickly *if* it is not gated.
    let dated = ReconcileStore::<i32, i32>::new(
        Config::default()
            .with_port(port)
            .with_listen_addr(dated_addr)
            .with_peer_net(peer_net),
    )
    .await
    .with_tombstone_timeout(Duration::from_millis(50));
    let mirror = ReconcileMirror::<i32, i32>::new(
        Config::default()
            .with_port(port)
            .with_listen_addr(mirror_addr)
            .with_peer_net(peer_net),
    )
    .await
    .with_seed(dated_addr);

    dated.insert(1, 11);
    dated.insert(2, 22);

    let dated_task = tokio::spawn(dated.clone().run());
    let mirror_task = tokio::spawn(mirror.clone().run());

    // The mirror syncs (and thereby contacts the dated store, the moment it could wrongly become a
    // member).
    assert_until!(mirror.get(&1).as_deref() == Some(&11));
    assert_until!(mirror.get(&2).as_deref() == Some(&22));

    // Delete key 1 on the dated store, then wait well past the tombstone timeout and the GC scan
    // period. Because the only node that ever contacted the dated store is a value-only mirror
    // (which is not a member), the tombstone is causally stable and must be collected.
    dated.remove(&1);
    let with_tombstone = dated.fingerprint(..);
    assert_until!(dated.fingerprint(..) != with_tombstone);
    // The live value for key 2 is untouched by GC.
    assert_eq!(dated.get(&2).as_deref(), Some(&22));
    // NOTE: we deliberately do *not* assert the deletion reaches the mirror here. With this
    // pathologically short tombstone timeout the dated store collects the tombstone before the
    // mirror's periodic value-only diff observes it, so the mirror may retain the pre-deletion
    // value — the documented consequence of GC outrunning mirror propagation. Reliable deletion
    // propagation (with a normal timeout) is covered by `mirror_converges_with_dated_store`.

    dated_task.abort();
    mirror_task.abort();
}
