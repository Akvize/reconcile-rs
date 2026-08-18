// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::time::Duration;

use crate::clock::NodeId;
use crate::{replicated_map::Config, ReplicatedMap};

use super::ephemeral_config;

#[tokio::test]
async fn tombstones_expiration() {
    // A dedicated port and /32 net keep a concurrent test's discovery from injecting here.
    let config = Config::default()
        .with_port(8090)
        .with_listen_addr("127.0.0.45".parse().unwrap())
        .with_net("127.0.0.45/32".parse().unwrap())
        .with_insecure_no_key();
    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed")
        .with_tombstone_timeout(Duration::from_millis(1));

    // No `run()`: its periodic GC would race these assertions.

    // `remove` inserts a tombstone rather than deleting the key outright.
    store.remove(&0);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(store.tombstones.expired(), vec![0]);
    assert_eq!(store.tombstones.remove(&0), Some(0));
    assert_eq!(store.tombstones.remove(&0), None);
}

/// The instant derived from a peer-controlled tombstone stamp must be bounded, and the
/// stored stamp must come through byte-identical.
mod tombstone_expiry_bound {
    use super::*;
    use crate::clock::{Hlc, LogicalCounter, PhysicalTime};
    use crate::entry::Entry;
    use crate::replicated_map::write::TOMBSTONE_STAMP_DRIFT_BUDGET;
    use chrono::Utc;

    /// Plant a tombstone carrying exactly `physical_ms` through the hook, and return the
    /// instant the wheel recorded for it.
    async fn plant(physical_ms: u64) -> (ReplicatedMap<i32, i32>, chrono::DateTime<Utc>) {
        let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
            .await
            .expect("bind failed");
        let stamp = crate::clock::Timestamp::new(
            Hlc::new(
                PhysicalTime::from_millis(physical_ms),
                LogicalCounter::new(7),
            ),
            NodeId::new(0xBEEF),
        );
        store
            .engine
            .just_insert_bulk(&[(1, Entry::tombstone(stamp))]);

        // The stored stamp must be untouched.
        assert_eq!(
            store.engine.map.read().get(&1).unwrap().stamp,
            stamp,
            "the stored stamp must be exactly as received — only the expiry instant is bounded"
        );

        let when = store
            .tombstones
            .instant_of(&1)
            .expect("tombstone was not tracked");
        (store, when)
    }

    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn cap_ms() -> i64 {
        now_ms() + TOMBSTONE_STAMP_DRIFT_BUDGET.millis() as i64
    }

    /// Regime 1 — an ordinary stamp is used verbatim, exactly as before the bound existed.
    /// This is what keeps honest replicas agreeing on when a tombstone ages out.
    #[tokio::test]
    async fn a_normal_stamp_is_used_verbatim() {
        let physical = now_ms() as u64 - 1_000;
        let (_store, when) = plant(physical).await;
        assert_eq!(when.timestamp_millis(), physical as i64);
    }

    /// Regime 2: far future, inside chrono's range. Must land on the cap — converting it
    /// exactly would date the tombstone past every plausible expiry.
    #[tokio::test]
    async fn a_far_future_representable_stamp_is_capped() {
        // ~10 000 years ahead: inside chrono's ceiling, so a lossless conversion is the hazard.
        let physical = now_ms() as u64 + 10_000 * 365 * 24 * 3_600_000;
        let (_store, when) = plant(physical).await;
        assert!(
            when.timestamp_millis() <= cap_ms(),
            "instant {when} escaped the cap"
        );
        assert!(
            when.timestamp_millis() >= now_ms() - 1_000,
            "a capped instant must stay in the near future, not fall into the past"
        );
    }

    /// Regime 3: above `i64::MAX`. Must land on the same cap, never a pre-1970 date.
    #[tokio::test]
    async fn a_stamp_above_i64_max_is_bounded_not_wrapped() {
        let (_store, when) = plant(u64::MAX).await;
        assert!(
            when.timestamp_millis() > 0,
            "stamp wrapped to a pre-epoch instant: {when}"
        );
        assert!(
            when.timestamp_millis() <= cap_ms(),
            "instant {when} escaped the cap"
        );
    }

    /// A hostile tombstone's expiry deadline must be a finite operator-controlled horizon
    /// (`now + budget + timeout`), not a date the peer picked. Bounding makes expiry
    /// reachable, not immediate — hence asserting on the deadline.
    #[tokio::test]
    async fn a_capped_tombstone_has_a_finite_expiry_horizon() {
        let timeout = Duration::from_secs(60);
        let physical = now_ms() as u64 + 10_000 * 365 * 24 * 3_600_000;
        let (_store, when) = plant(physical).await;

        let deadline = when.timestamp_millis() + timeout.as_millis() as i64;
        assert!(
            deadline <= cap_ms() + timeout.as_millis() as i64,
            "expiry deadline {deadline} is beyond now + budget + timeout"
        );
        // For contrast, the deadline an unbounded conversion would produce.
        let unbounded = physical as i64 + timeout.as_millis() as i64;
        assert!(
            unbounded > deadline + 100 * 365 * 24 * 3_600_000,
            "the unbounded deadline should be astronomically later than the bounded one"
        );
    }
}

/// `just_insert_bulk` must actually insert every pair, not silently no-op.
#[tokio::test]
async fn just_insert_bulk_actually_inserts_every_pair() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();
    store.just_insert_bulk(&[(1, 10), (2, 20), (3, 30)]);
    assert_eq!(store.get(&1).as_deref(), Some(&10));
    assert_eq!(store.get(&2).as_deref(), Some(&20));
    assert_eq!(store.get(&3).as_deref(), Some(&30));
}

/// `just_remove_bulk` must actually remove every key, not silently no-op.
#[tokio::test]
async fn just_remove_bulk_actually_removes_every_key() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();
    store.just_insert_bulk(&[(1, 10), (2, 20)]);
    store.just_remove_bulk(&[1, 2]);
    assert_eq!(store.get(&1).as_deref(), None);
    assert_eq!(store.get(&2).as_deref(), None);
}

/// `set_tombstone_timeout` must actually retune the wheel at runtime, not silently no-op.
#[tokio::test]
async fn set_tombstone_timeout_actually_retunes_the_wheel() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap()
        .with_tombstone_timeout(Duration::from_secs(3600)); // won't expire on its own
    store.remove(&0);
    assert!(
        store.tombstones.expired().is_empty(),
        "must not be expired yet under the long initial timeout"
    );

    store.set_tombstone_timeout(Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        store.tombstones.expired(),
        vec![0],
        "retuning the timeout down must make the tombstone expire promptly"
    );
}
