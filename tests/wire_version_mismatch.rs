// Copyright 2026 Developers of the reconcile-rs project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A peer on a different wire version must be rejected with a distinguishable, counted
//! reason — never confused with a merely malformed datagram. `metrics::set_global_recorder` (not
//! the thread-local `with_local_recorder` `tests/observability.rs` uses) is what lets this
//! capture counters incremented from the `run()` loop's own spawned task, on another thread.
//!
//! Own binary (a plain `tests/*.rs` file), so installing the recorder globally once here cannot
//! collide with another test file's own global install.

#![cfg(all(feature = "internal-testing", feature = "metrics"))]

use std::net::IpAddr;
use std::time::Duration;

use metrics_util::debugging::{DebugValue, DebuggingRecorder};

use reconcile::{replicated_map::Config, ReplicatedMap};

fn config(addr: &str, port: u16) -> Config {
    Config::default()
        .with_port(port)
        .with_listen_addr(addr.parse().unwrap())
}

/// Current value of `reconcile_datagrams_dropped_total{reason=<reason>}` in the process-global
/// recorder installed by [`install_recorder`].
fn dropped_count(snapshotter: &metrics_util::debugging::Snapshotter, reason: &str) -> u64 {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(composite, _unit, _desc, value)| {
            let key = composite.key();
            (key.name() == "reconcile_datagrams_dropped_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == reason))
            .then_some(value)
        })
        .map(|value| match value {
            DebugValue::Counter(v) => v,
            _ => panic!("reconcile_datagrams_dropped_total must be a counter"),
        })
        .unwrap_or(0)
}

/// Installs a [`DebuggingRecorder`] as the process-global metrics recorder, once. Every test in
/// this binary shares it — `dropped_count` reads absolute values, so each test only needs to
/// compare a before/after delta, not assume a fresh recorder.
fn install_recorder() -> metrics_util::debugging::Snapshotter {
    use std::sync::OnceLock;
    static SNAPSHOTTER: OnceLock<metrics_util::debugging::Snapshotter> = OnceLock::new();
    SNAPSHOTTER
        .get_or_init(|| {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            recorder.install().expect("install the debugging recorder");
            snapshotter
        })
        .clone()
}

/// A raw byte, sent directly over UDP with no framing beyond what `Authenticator::Disabled`
/// requires — i.e. exactly what a peer speaking a different wire version, or an attacker, would
/// produce. `version` is the byte a real `seal()` would have written; this test controls it
/// directly instead, since `WIRE_VERSION` is fixed per build and there is no second build to seal
/// a genuinely different version with.
fn raw_datagram(version: u8) -> Vec<u8> {
    // The version byte, then arbitrary bytes standing in for a message body — deliberately not a
    // real `Message`, since `check_version` runs (and must reject) before any attempt to decode
    // one. `Message` is a `pub(crate)` type unreachable from this integration-test crate anyway.
    vec![version, 0xDE, 0xAD, 0xBE, 0xEF]
}

/// A version-mismatched datagram is rejected under its own reason, distinct from "malformed" —
/// the two must not be confused, or an operator alerting on one cannot tell a rolling upgrade
/// apart from an attack.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_wire_versions_are_reported_not_silently_dropped() {
    let snapshotter = install_recorder();

    let target_addr: IpAddr = "127.0.0.210".parse().unwrap();
    let port = 9900u16;
    let store = ReplicatedMap::<i32, i32>::new(config("127.0.0.210", port))
        .await
        .expect("bind failed");
    let task = tokio::spawn(store.clone().run());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let sender = tokio::net::UdpSocket::bind("127.0.0.211:0")
        .await
        .expect("bind sender");
    let target = std::net::SocketAddr::new(target_addr, port);

    let before_version = dropped_count(&snapshotter, "version");
    let before_malformed = dropped_count(&snapshotter, "malformed");

    // (a) A datagram claiming a version this build does not speak.
    let wrong_version = gossip::auth::WIRE_VERSION.wrapping_add(1);
    sender
        .send_to(&raw_datagram(wrong_version), &target)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        dropped_count(&snapshotter, "version"),
        before_version + 1,
        "a wire-version mismatch must be counted under its own distinguishable reason"
    );
    assert_eq!(
        dropped_count(&snapshotter, "malformed"),
        before_malformed,
        "a version mismatch must not also (or instead) be counted as merely malformed"
    );

    // (b) Same junk body, but the *correct* version — proves the rejection above was about the
    // version byte specifically, not the nonsense that follows it: this one clears the version
    // check and fails later, at decode, under a different reason.
    let before_version = dropped_count(&snapshotter, "version");
    let before_malformed = dropped_count(&snapshotter, "malformed");
    sender
        .send_to(&raw_datagram(gossip::auth::WIRE_VERSION), &target)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        dropped_count(&snapshotter, "version"),
        before_version,
        "a correctly-versioned datagram must never be counted as a version mismatch"
    );
    assert_eq!(
        dropped_count(&snapshotter, "malformed"),
        before_malformed + 1,
        "a correctly-versioned but undecodable body must fall through to the malformed reason"
    );

    // Never converges: this is the (already-documented) non-convergence symptom, alongside the
    // diagnostic above rather than in place of it.
    assert!(store.get(&0).is_none());

    task.abort();
}
