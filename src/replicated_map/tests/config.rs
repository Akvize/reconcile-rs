// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::time::Duration;

use crate::{replicated_map::Config, ReplicatedMap};

/// #325: a `Config` with neither a cluster key nor the explicit insecure opt-in must refuse to
/// build at all, rather than silently running unauthenticated — the whole point of the guard.
#[tokio::test]
#[should_panic(expected = "Config::cluster_key is None")]
async fn missing_key_and_no_insecure_opt_in_panics_at_construction() {
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("OS should hand out an ephemeral port")
        .local_addr()
        .expect("a bound socket reports its own address")
        .port();
    let config = Config::default().with_port(port);
    let _ = ReplicatedMap::<i32, i32>::new(config).await;
}

/// The metering/buffer-size defaults are pinned to their documented values (32 MiB/s, 1 MiB
/// floor, 8 MiB) — an accidental `*` → `+`/`/` typo in the byte-count arithmetic would silently
/// shrink these by orders of magnitude without a type error to catch it.
#[test]
fn byte_rate_defaults_match_their_documented_values() {
    assert_eq!(
        super::super::config::DEFAULT_BULK_SEND_RATE,
        32 * 1024 * 1024
    );
    assert_eq!(super::super::MIN_BULK_SEND_RATE, 1024 * 1024);
    assert_eq!(
        super::super::config::DEFAULT_SOCKET_BUFFER_SIZE,
        8 * 1024 * 1024
    );
}

/// Each `with_*` builder actually sets its field on `self`; a mutant collapsing any of them to
/// `Default::default()` would silently discard both `self` and the argument.
#[test]
fn config_builders_actually_set_their_field() {
    use ipnet::IpNet;

    let net_a: IpNet = "10.0.0.0/8".parse().unwrap();
    let net_b: IpNet = "10.1.0.0/16".parse().unwrap();
    let cfg = Config::default().with_nets(&[net_a, net_b]);
    assert_eq!(cfg.nets[0], Some(net_a));
    assert_eq!(cfg.nets[1], Some(net_b));

    let cfg = Config::default().with_bulk_send_rate(777_216);
    assert_eq!(cfg.bulk_send_rate, Some(777_216));

    let cfg = Config::default().with_recv_buffer_size(12_345);
    assert_eq!(cfg.recv_buffer_size, Some(12_345));

    let cfg = Config::default().with_send_buffer_size(54_321);
    assert_eq!(cfg.send_buffer_size, Some(54_321));

    let cfg = Config::default().with_freshness_window(Duration::from_secs(42));
    assert_eq!(cfg.freshness_window, Duration::from_secs(42));
}
