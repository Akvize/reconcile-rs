// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::time::Duration;

use crate::replicated_map::{Config, MAX_NETS};

mod config;
mod construction;
mod discovery;
mod lifecycle;
mod membership;
mod peer_cap;
mod persistence;
mod read;
mod write;

/// A config bound to a fresh port on loopback — port `0` is refused, so
/// [`next_ephemeral_test_port`](crate::replica::tests::next_ephemeral_test_port) stands in for it
/// — so persistence tests can construct stores without colliding on a fixed port.
fn ephemeral_config() -> Config {
    Config {
        port: crate::replica::tests::next_ephemeral_test_port(),
        listen_addr: "127.0.0.1".parse().unwrap(),
        nets: [None; MAX_NETS],
        remote_interval: 6,
        remote_fanout: 2,
        cluster_key: None,
        insecure_no_key: true,
        node_id: None,
        encrypt: false,
        reconcile_interval: Duration::from_secs(1),
        bulk_send_rate: Some(super::config::DEFAULT_BULK_SEND_RATE),
        recv_buffer_size: Some(super::config::DEFAULT_SOCKET_BUFFER_SIZE),
        send_buffer_size: Some(super::config::DEFAULT_SOCKET_BUFFER_SIZE),
        freshness_window: gossip::replay::FRESHNESS_WINDOW_DEFAULT,
        max_peers: super::config::DEFAULT_MAX_PEERS,
        max_concurrent_bulk_dumps: super::config::DEFAULT_MAX_CONCURRENT_BULK_DUMPS,
        snapshot_interval: super::persistence::SNAPSHOT_INTERVAL,
        max_clock_drift: crate::clock::MAX_CLOCK_DRIFT,
    }
}
