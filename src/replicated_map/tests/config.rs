// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::time::Duration;

use crate::{
    replicated_map::{Config, ConfigError, MAX_NETS},
    ReplicatedMap,
};

use super::ephemeral_config;

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

/// `ConfigError::TooManyNets`'s `Display` text is user-facing — assert its actual content, not
/// merely that formatting it doesn't panic.
#[test]
fn config_error_too_many_nets_display_names_the_limit() {
    assert_eq!(
        ConfigError::TooManyNets.to_string(),
        format!("at most {MAX_NETS} networks are supported")
    );
}

/// `Config`'s hand-written `Debug` impl exists to redact `cluster_key` — assert it actually
/// hides the key material and still reports `Some`/`None` correctly either way.
#[test]
fn config_debug_redacts_cluster_key_but_not_its_presence() {
    let key_bytes = [0xABu8; 32];
    let with_key = ephemeral_config().with_cluster_key(gossip::auth::ClusterKey::new(key_bytes));
    let debug = format!("{with_key:?}");
    assert!(
        debug.contains("<redacted>"),
        "expected the key material to be redacted: {debug}"
    );
    assert!(
        !debug.contains("ab, ab, ab") && !debug.contains("171, 171, 171"),
        "raw key bytes must not appear in Debug output: {debug}"
    );

    let without_key = ephemeral_config();
    let debug = format!("{without_key:?}");
    assert!(
        debug.contains("cluster_key: None"),
        "expected an explicit None, got: {debug}"
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

    let cfg = Config::default().with_snapshot_interval(Duration::from_secs(99));
    assert_eq!(cfg.snapshot_interval, Duration::from_secs(99));

    let drift = crate::clock::ClockDrift::from_millis(123);
    let cfg = Config::default().with_max_clock_drift(drift);
    assert_eq!(cfg.max_clock_drift, drift);

    let cfg = Config::default().with_coalesce_window(Duration::from_millis(7));
    assert_eq!(cfg.coalesce_window, Duration::from_millis(7));
}

/// The new #292 fields default to the documented values: [`SNAPSHOT_INTERVAL`] (5 s) and
/// [`MAX_CLOCK_DRIFT`](crate::clock::MAX_CLOCK_DRIFT) (1 h) — not e.g. `Duration::ZERO`, which
/// would make every write pay a snapshot or silently disable the drift clamp.
///
/// [`SNAPSHOT_INTERVAL`]: super::super::persistence::SNAPSHOT_INTERVAL
#[test]
fn snapshot_interval_and_max_clock_drift_default_correctly() {
    let cfg = Config::default();
    assert_eq!(
        cfg.snapshot_interval,
        super::super::persistence::SNAPSHOT_INTERVAL
    );
    assert_eq!(cfg.max_clock_drift, crate::clock::MAX_CLOCK_DRIFT);
}
