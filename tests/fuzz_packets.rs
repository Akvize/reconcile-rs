// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Malformed-packet fuzzing of the receive loop (issue #113).
//!
//! A single malformed datagram from any host must never panic or kill the
//! receive task — otherwise an unauthenticated remote could DoS the whole
//! cluster. We run a store *without* a cluster key so that arbitrary bytes reach
//! the bincode deserialization path (`handle_messages`), bombard it with random
//! and deliberately-corrupt datagrams, and assert that the receive task stays
//! alive and the local state is untouched.

use std::time::Duration;

use rand::{Rng, SeedableRng};
use tokio::net::UdpSocket;

use reconcile::{reconcile_store::Config, ReconcileStore};

/// Generate a batch of adversarial payloads: pure random noise of varied
/// lengths (including empty and larger-than-buffer), plus a few structured
/// "almost valid" shapes (truncated length prefixes, huge declared lengths)
/// that tend to trip naive bincode decoders.
fn malformed_payloads(seed: u64) -> Vec<Vec<u8>> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut payloads = Vec::new();

    // Random noise across a wide range of sizes (0 .. a few KB to exceed the
    // receive buffer and exercise the oversized-datagram branch too).
    for _ in 0..200 {
        let len = rng.gen_range(0..4096);
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf[..]);
        payloads.push(buf);
    }

    // All-zero and all-one buffers of various sizes.
    for len in [0usize, 1, 7, 64, 1024, 4096] {
        payloads.push(vec![0u8; len]);
        payloads.push(vec![0xFFu8; len]);
    }

    // Corrupt bincode-ish framings: a small leading byte (a plausible enum
    // variant / sequence tag) followed by truncated or oversized content.
    for tag in 0u8..=4 {
        payloads.push(vec![tag]);
        payloads.push(vec![tag, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let mut huge_len = vec![tag];
        huge_len.extend_from_slice(&u64::MAX.to_le_bytes());
        payloads.push(huge_len);
    }

    payloads
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_datagrams_do_not_panic_or_corrupt_state() {
    let port = 8085;
    let victim_addr = "127.0.0.70";
    let config = Config::default()
        .with_port(port)
        .with_listen_addr(victim_addr.parse().unwrap());
    // No cluster key: arbitrary bytes are *not* dropped at the auth gate and
    // reach the deserializer, which is exactly the path we want to fuzz.
    let store = ReconcileStore::<i32, String>::new(config).await;
    store.just_insert(0, "legit".to_string());

    let task = tokio::spawn(store.clone().run());

    let attacker = UdpSocket::bind("127.0.0.71:0").await.unwrap();
    let target = format!("{victim_addr}:{port}");

    for payload in malformed_payloads(0xDEAD_BEEF) {
        // Sending may legitimately fail for over-MTU buffers on some platforms;
        // those are not the property under test, so ignore send errors.
        let _ = attacker.send_to(&payload, &target).await;
        // Yield occasionally so the receive loop interleaves with the sender.
        tokio::task::yield_now().await;
    }

    // Give the victim time to (attempt to) process everything.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The receive task must still be running (a panic would have finished it).
    assert!(
        !task.is_finished(),
        "receive task died on a malformed datagram"
    );
    // The legitimate value must be untouched by any garbage datagram.
    assert_eq!(store.get(&0).as_deref(), Some(&"legit".to_string()));

    task.abort();
}
