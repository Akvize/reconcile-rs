// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::SocketAddr;

use bincode::{DefaultOptions, Serializer};
use serde::Serialize;

use super::super::Message;
use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::entry::{Entry, State};
use crate::replica::{version_hash, Replica};
use crate::replicated_map::Config;
use gossip::auth;

type Tombstoned = Entry<Timestamp, i32>;

async fn engine(addr: &str) -> Replica<i32, i32> {
    // Use a distinct port per module to avoid bind conflicts between parallel test runs.
    let config = Config::default()
        .with_port(0)
        .with_listen_addr(addr.parse().unwrap())
        .with_insecure_no_key();
    Replica::new(config).await.expect("bind failed")
}

fn ack_bytes(key: i32, version: u64) -> Vec<u8> {
    let msg = Message::Ack::<i32, Tombstoned, State<i32>>((key, version));
    let mut buf = vec![gossip::auth::WIRE_VERSION];
    msg.serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    buf
}

/// An ack for a key that does not exist locally must not create any entry in
/// `tombstone_acks`. Without the fix, `or_default()` allocates on every ack for
/// an arbitrary key, enabling unbounded growth.
#[tokio::test]
async fn ack_for_unknown_key_does_not_grow_tombstone_acks() {
    let eng = engine("127.0.0.93").await;
    let peer: SocketAddr = "127.0.0.94:9000".parse().unwrap();

    let bytes = ack_bytes(42, 999);
    let payload = auth::Authenticator::new(None, false)
        .open(&bytes)
        .expect("unauthenticated open")
        .check_version()
        .expect("ack_bytes stamps the current wire version");
    let payload = payload
        .verify_replay(&eng.replay_filter, peer.ip())
        .expect("unauthenticated mode is exempt from the replay check");
    let mut send_buf = Vec::new();
    eng.handle_messages(payload, peer, &mut send_buf).await;

    assert_eq!(
        eng.tombstone_acks_len(),
        0,
        "ack for unknown key must not insert into tombstone_acks"
    );
}

/// An ack for a key that exists locally but is a *live* (non-tombstone) value must
/// likewise be dropped.
#[tokio::test]
async fn ack_for_live_key_does_not_grow_tombstone_acks() {
    let eng = engine("127.0.0.95").await;
    let key = 10;
    eng.just_insert(
        key,
        Entry::present(
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            ),
            42,
        ),
    );

    let peer: SocketAddr = "127.0.0.96:9000".parse().unwrap();
    let bytes = ack_bytes(key, 123);
    let payload = auth::Authenticator::new(None, false)
        .open(&bytes)
        .expect("unauthenticated open")
        .check_version()
        .expect("ack_bytes stamps the current wire version");
    let payload = payload
        .verify_replay(&eng.replay_filter, peer.ip())
        .expect("unauthenticated mode is exempt from the replay check");
    let mut send_buf = Vec::new();
    eng.handle_messages(payload, peer, &mut send_buf).await;

    assert_eq!(
        eng.tombstone_acks_len(),
        0,
        "ack for a live (non-tombstone) key must not insert into tombstone_acks"
    );
}

/// An ack for a key that IS a local tombstone must be recorded normally; the
/// decommission test verifies tombstone GC still completes.
#[tokio::test]
async fn ack_for_local_tombstone_is_recorded() {
    let eng = engine("127.0.0.97").await;
    let key = 20;
    let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
        NodeId::new(0),
    ));
    let version = version_hash(&tombstone);
    eng.just_insert(key, tombstone);

    let peer: SocketAddr = "127.0.0.98:9000".parse().unwrap();
    let bytes = ack_bytes(key, version);
    let payload = auth::Authenticator::new(None, false)
        .open(&bytes)
        .expect("unauthenticated open")
        .check_version()
        .expect("ack_bytes stamps the current wire version");
    let payload = payload
        .verify_replay(&eng.replay_filter, peer.ip())
        .expect("unauthenticated mode is exempt from the replay check");
    let mut send_buf = Vec::new();
    eng.handle_messages(payload, peer, &mut send_buf).await;

    assert_eq!(
        eng.tombstone_acks_len(),
        1,
        "ack for a local tombstone must be recorded in tombstone_acks"
    );

    // Causal-stability GC still completes: with one member who has acked and no
    // other members, the tombstone is stable and bookkeeping is cleared on forget.
    eng.members.write().insert(peer.ip());
    assert!(
        eng.is_tombstone_stable(&key, version),
        "tombstone should be stable after the only member has acked"
    );
    eng.forget_tombstone(&key);
    assert_eq!(
        eng.tombstone_acks_len(),
        0,
        "forget_tombstone must clear tombstone_acks for the key"
    );
}
