// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::replicated_map::Config;

/// Default budget is 4, matching DEFAULT_MAX_CONCURRENT_BULK_DUMPS.
#[test]
fn default_budget_is_four() {
    assert_eq!(Config::default().max_concurrent_bulk_dumps, 4);
}

/// `with_max_concurrent_bulk_dumps` overrides the value.
#[test]
fn builder_sets_budget() {
    let cfg = Config::default().with_max_concurrent_bulk_dumps(1);
    assert_eq!(cfg.max_concurrent_bulk_dumps, 1);
}

/// With a budget of 1, claiming a second slot before the first is released must fail.
/// After the first slot is dropped its count returns to zero and a fresh claim succeeds.
#[tokio::test]
async fn budget_guard_limits_and_releases_slots() {
    use crate::replica::Replica;

    let config = Config::default()
        .with_port(crate::replica::tests::next_ephemeral_test_port())
        .with_listen_addr("127.0.0.99".parse().unwrap())
        .with_max_concurrent_bulk_dumps(1)
        .with_insecure_no_key();
    let eng = Replica::<i32, i32>::new(config).await.expect("bind failed");

    let peer_a: std::net::SocketAddr = "127.0.0.100:9001".parse().unwrap();
    let peer_b: std::net::SocketAddr = "127.0.0.101:9001".parse().unwrap();

    // First claim succeeds.
    let slot_a = eng.try_claim_dump_slot(peer_a);
    assert!(slot_a.is_some(), "first slot must be available");
    assert_eq!(eng.bulk_dumps_in_flight_count(), 1);

    // Second claim (different peer) is rejected — budget exhausted.
    let slot_b = eng.try_claim_dump_slot(peer_b);
    assert!(slot_b.is_none(), "second slot must be rejected at budget 1");
    assert_eq!(eng.bulk_dumps_in_flight_count(), 1);

    // Releasing the first slot (drop) frees it for the next caller.
    drop(slot_a);
    assert_eq!(eng.bulk_dumps_in_flight_count(), 0);

    // Now peer_b's retry succeeds.
    let slot_b_retry = eng.try_claim_dump_slot(peer_b);
    assert!(
        slot_b_retry.is_some(),
        "slot must be available after release"
    );
    drop(slot_b_retry);
}
