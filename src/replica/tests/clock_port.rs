// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::sync::Arc;

use crate::clock::{Hlc, LogicalCounter, ManualClock, NodeId, PhysicalTime, Timestamp};
use crate::replica::Replica;
use crate::replicated_map::Config;

/// The engine mints timestamps only through the injected [`Clock`](crate::clock::Clock) port, so
/// a deterministic adapter makes `clock_now()` fully reproducible — no wall-clock time involved.
/// This is the engine-level testability the port exists to provide.
#[tokio::test]
async fn engine_mints_through_the_injected_clock() {
    let config = Config::default()
        .with_port(8080)
        .with_listen_addr("127.0.0.70".parse().unwrap())
        .with_insecure_no_key();
    let clock = Arc::new(ManualClock::new(NodeId::new(42)));
    let eng: Replica<i32, i32> = Replica::new_with_clock(config, clock)
        .await
        .expect("bind failed");

    assert_eq!(
        eng.clock_now(),
        Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(0), LogicalCounter::new(1)),
            NodeId::new(42)
        )
    );
    assert_eq!(
        eng.clock_now(),
        Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(0), LogicalCounter::new(2)),
            NodeId::new(42)
        )
    );
}
