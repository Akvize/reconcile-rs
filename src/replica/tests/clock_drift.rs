// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::clock::{ClockDrift, Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::replica::Replica;
use crate::replicated_map::Config;
use crate::transport::InMemoryNetwork;

use super::next_ephemeral_test_port;

/// `Config::max_clock_drift` (#292) must actually reach the [`HlcClock`](crate::clock::HlcClock)
/// [`Replica::new`]/[`Replica::with_transport`] construct — not just the hardcoded
/// [`MAX_CLOCK_DRIFT`](crate::clock::MAX_CLOCK_DRIFT) default `HlcClock::new` falls back to on its
/// own. A stamp 500ms in the future sits well inside the default one-hour budget (so a passing
/// test here cannot be satisfied by the default silently winning), but far past a 50ms budget
/// configured explicitly.
#[tokio::test]
async fn config_max_clock_drift_reaches_the_constructed_clock() {
    let net = InMemoryNetwork::new();
    let port = next_ephemeral_test_port();
    let ip: IpAddr = "127.0.9.1".parse().unwrap();
    let cfg = Config::default()
        .with_listen_addr(ip)
        .with_port(port)
        .with_insecure_no_key()
        .with_max_clock_drift(ClockDrift::from_millis(50));
    let replica: Replica<i32, i32> =
        Replica::with_transport(cfg, Arc::new(net.bind(SocketAddr::new(ip, port))));

    let before = replica.clock_now();
    let far_future = Timestamp::new(
        Hlc::new(
            PhysicalTime::from_millis(before.physical().millis() + 500),
            LogicalCounter::new(0),
        ),
        NodeId::new(99),
    );
    replica.clock.observe(far_future);
    let after = replica.clock_now();

    assert!(
        after > before,
        "observe must still advance the clock monotonically: {before:?} -> {after:?}"
    );
    assert!(
        after.physical().millis() < before.physical().millis() + 200,
        "the configured 50ms drift budget did not bound a 500ms-future stamp: \
         before={before:?} after={after:?} far_future={far_future:?}"
    );
}
