// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use crate::entry::State;
use crate::replica::{send_messages_paced, Message, SendPorts};
use crate::transport::UdpTransport;
use gossip::auth::Authenticator;

type Msg = Message<u64, Vec<u8>, State<u8>>;

/// `n` Update messages with `value_len`-byte values — enough to span several 64 KiB datagrams.
fn bulk_updates(n: u64, value_len: usize) -> Vec<Msg> {
    (0..n)
        .map(|k| Message::Update((k, vec![0u8; value_len])))
        .collect()
}

/// Send `messages` (unauthenticated, to a discard address — the datagrams go nowhere on an
/// unconnected UDP socket) at `rate` and return how long it took.
///
/// Bounded by an outer timeout well above any legitimate pacing delay this module exercises:
/// a broken pacing calculation (e.g. a duration derived from multiplying instead of dividing)
/// produces an astronomically large but finite `Duration`, which `sleep` then waits out
/// literally rather than erroring — an unbounded `.await` here would hang the test for as
/// long as the surrounding harness lets it, instead of failing fast and readably.
async fn time_send(messages: &[Msg], rate: Option<usize>) -> Duration {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let transport = UdpTransport::new(socket);
    let authenticator = Authenticator::new(None, false);
    let sender_counter = gossip::replay::SenderCounter::new();
    let ports = SendPorts {
        transport: &transport,
        authenticator: &authenticator,
        sender_counter: &sender_counter,
    };
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap(); // discard port
    let mut send_buf = Vec::new();
    let start = Instant::now();
    tokio::time::timeout(
        Duration::from_secs(10),
        send_messages_paced(messages, &ports, &peer, &mut send_buf, rate),
    )
    .await
    .expect("send_messages_paced took over 10s — pacing duration math is almost certainly broken");
    start.elapsed()
}

/// `bulk_send_rate` actually meters the transfer: a multi-datagram payload sent at a
/// low rate takes substantially longer than the same payload sent unpaced. Anchored to
/// wall-clock, so we only assert a generous lower bound on the paced run (sleeping can only
/// lengthen it) and an upper bound on the unpaced run — robust to CI scheduler jitter.
#[tokio::test]
async fn bulk_send_rate_meters_the_transfer() {
    // ~265 KiB => ~5 datagrams of 64 KiB, i.e. ~4 inter-datagram pacing points.
    let messages = bulk_updates(256, 1024);

    let unpaced = time_send(&messages, None).await;
    assert!(
        unpaced < Duration::from_millis(200),
        "unpaced send should be near-instant, took {unpaced:?}"
    );

    // 512 KiB/s over ~256 KiB of leading datagrams => ~0.5 s of cumulative sleeps.
    let paced = time_send(&messages, Some(512 * 1024)).await;
    assert!(
        paced >= Duration::from_millis(300),
        "paced send should be metered to ~0.5 s, took {paced:?}"
    );
}

/// A `None` rate is the historical unpaced behaviour, and an explicit `0` is treated as "no
/// pacing" rather than dividing by zero.
#[tokio::test]
async fn zero_or_none_rate_does_not_pace() {
    let messages = bulk_updates(256, 1024);
    assert!(time_send(&messages, None).await < Duration::from_millis(200));
    assert!(time_send(&messages, Some(0)).await < Duration::from_millis(200));
}

/// A nonzero `bulk_send_rate` below the floor is clamped up to it rather than holding the
/// per-peer in-flight mark across an effectively unbounded sleep (#331). Zero and `None`
/// still pass through unpaced.
#[tokio::test]
async fn tiny_bulk_send_rate_is_clamped_to_the_floor() {
    use crate::replica::Replica;
    use crate::replicated_map::{Config, MIN_BULK_SEND_RATE};

    async fn engine(addr: &str, bulk_send_rate: Option<usize>) -> Replica<i32, i32> {
        let config = Config {
            bulk_send_rate,
            ..Config::default()
                .with_listen_addr(addr.parse().unwrap())
                .with_insecure_no_key()
        };
        Replica::new(config).await.expect("bind failed")
    }

    let tiny = engine("127.0.0.80", Some(1)).await;
    assert_eq!(tiny.bulk_send_rate, Some(MIN_BULK_SEND_RATE));

    let none = engine("127.0.0.81", None).await;
    assert_eq!(none.bulk_send_rate, None);

    let zero = engine("127.0.0.82", Some(0)).await;
    assert_eq!(zero.bulk_send_rate, Some(0));

    let above_floor = engine("127.0.0.83", Some(MIN_BULK_SEND_RATE * 2)).await;
    assert_eq!(above_floor.bulk_send_rate, Some(MIN_BULK_SEND_RATE * 2));
}

/// A single message whose own encoding exceeds the datagram budget must be dropped — never
/// sent as a bogus empty datagram (when it was first in the batch) and never sent as an
/// oversized one (otherwise, which a real UDP socket rejects with `EMSGSIZE`). A
/// normal-sized message in the same batch must still go out untouched, whether it comes
/// before or after the oversized one.
#[tokio::test]
async fn oversized_message_is_dropped_not_sent_empty_or_oversized() {
    use crate::transport::{InMemoryNetwork, Transport};

    async fn observe_sends(messages: &[Msg]) -> Vec<usize> {
        let net = InMemoryNetwork::new();
        let sender_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let receiver_addr: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let sender_transport = net.bind(sender_addr);
        let receiver_transport = net.bind(receiver_addr);

        let authenticator = Authenticator::new(None, false);
        let sender_counter = gossip::replay::SenderCounter::new();
        let ports = SendPorts {
            transport: &sender_transport,
            authenticator: &authenticator,
            sender_counter: &sender_counter,
        };
        let mut send_buf = Vec::new();
        send_messages_paced(messages, &ports, &receiver_addr, &mut send_buf, None).await;

        let mut sizes = Vec::new();
        let mut buf = [0u8; 1 << 17];
        while let Ok(Ok((n, _))) = tokio::time::timeout(
            Duration::from_millis(50),
            receiver_transport.recv_from(&mut buf),
        )
        .await
        {
            sizes.push(n);
        }
        sizes
    }

    // One message far bigger than a single 64 KiB datagram, flanked by two ordinary ones.
    let oversized = vec![Message::Update((
        999u64,
        vec![0u8; super::super::BUFFER_SIZE * 2],
    ))];
    let small_before = bulk_updates(1, 16);
    let small_after = bulk_updates(1, 16);

    // Oversized first in the batch (the `last_size == 0` case pre-fix produced an empty
    // datagram).
    let mut messages = oversized.clone();
    messages.extend(small_after.clone());
    let sizes = observe_sends(&messages).await;
    assert_eq!(
        sizes.len(),
        1,
        "expected exactly the one normal-sized datagram, got {sizes:?}"
    );
    assert!(
        sizes[0] < super::super::BUFFER_SIZE,
        "unexpected datagram size {sizes:?}"
    );

    // Oversized in the middle of the batch (pre-fix, this queued the oversized bytes for a
    // doomed EMSGSIZE send attempt).
    let mut messages = small_before;
    messages.extend(oversized);
    let sizes = observe_sends(&messages).await;
    assert_eq!(
        sizes.len(),
        1,
        "expected exactly the one normal-sized datagram, got {sizes:?}"
    );
    assert!(
        sizes[0] < super::super::BUFFER_SIZE,
        "unexpected datagram size {sizes:?}"
    );
}
