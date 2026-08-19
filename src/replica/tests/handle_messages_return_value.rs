// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! `run()` only registers a sender in `peers`/`members` when [`handle_messages`] returns
//! `true` (`spoke_dated`) — a value-only sender (a read replica) must never gate tombstone GC.
//! Exercised by calling `handle_messages` directly with each message shape, rather than
//! through the network loop: the loop's own convergence-style tests seed membership by other
//! means too, so they do not reliably fail when this return value alone is wrong.

use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::entry::{Entry, State};
use crate::replica::Replica;
use crate::replicated_map::Config;
use bincode::{DefaultOptions, Serializer};
use gossip::auth;
use serde::Serialize;
use std::net::SocketAddr;

use super::super::Message;

/// Serialize a single message the same way a peer's datagram would arrive — the leading
/// wire-version byte plus the encoded message, no authentication.
fn message_bytes(message: &Message<i32, Entry<Timestamp, u8>, State<u8>>) -> Vec<u8> {
    let mut buf = vec![gossip::auth::WIRE_VERSION];
    message
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    buf
}

async fn feed(
    engine: &Replica<i32, u8>,
    message: &Message<i32, Entry<Timestamp, u8>, State<u8>>,
) -> bool {
    let bytes = message_bytes(message);
    let payload = auth::Authenticator::new(None, false)
        .open(&bytes)
        .expect("unauthenticated mode clears any datagram")
        .check_version()
        .expect("message_bytes stamps the current wire version");
    let peer: SocketAddr = "127.0.0.60:9".parse().unwrap();
    let payload = payload
        .verify_replay(&engine.replay_filter, peer.ip())
        .expect("unauthenticated mode is exempt from the replay check");
    let mut send_buf = Vec::new();
    engine.handle_messages(payload, peer, &mut send_buf).await
}

fn future_stamp() -> Timestamp {
    Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(u64::MAX), LogicalCounter::new(0)),
        NodeId::new(0),
    )
}

#[tokio::test]
async fn a_dated_update_reports_true() {
    let config = Config::default()
        .with_port(0)
        .with_listen_addr("127.0.0.60".parse().unwrap())
        .with_insecure_no_key();
    let engine = Replica::<i32, u8>::new(config).await.expect("bind failed");
    let message = Message::Update((1, Entry::present(future_stamp(), 7)));
    assert!(
        feed(&engine, &message).await,
        "a dated Update must report spoke_dated = true, or run() will never grant its \
         sender peers/members membership"
    );
}

#[tokio::test]
async fn a_value_only_update_reports_false() {
    let config = Config::default()
        .with_port(0)
        .with_listen_addr("127.0.0.61".parse().unwrap())
        .with_insecure_no_key();
    let engine = Replica::<i32, u8>::new(config).await.expect("bind failed");
    let message: Message<i32, Entry<Timestamp, u8>, State<u8>> =
        Message::ValueUpdate((1, State::Present(7)));
    assert!(
        !feed(&engine, &message).await,
        "a value-only ValueUpdate must report spoke_dated = false — a read replica must never \
         gate tombstone GC by being granted peers/members membership"
    );
}
