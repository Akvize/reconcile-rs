// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bincode::{DefaultOptions, Serializer};
use gossip::auth;
use serde::Serialize;

use super::super::Message;
use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::entry::{Entry, State};
use crate::replica::Replica;
use crate::replicated_map::Config;

/// Serialize an `Update`, matching `deadlock_regressions.rs`'s network-ingest helper.
fn update_message_bytes(key: i32, value: Entry<Timestamp, u8>) -> Vec<u8> {
    let message = Message::Update::<i32, Entry<Timestamp, u8>, State<u8>>((key, value));
    let mut buf = vec![gossip::auth::WIRE_VERSION];
    message
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    buf
}

/// A remote `Update` whose stamp exactly equals the locally-held stamp must not be treated as
/// newer — `handle_messages` only re-applies on a *strictly greater* stamp. Equal stamps only
/// arise from an exact re-delivery of the same write (a retried or duplicated datagram); treating
/// them as newer would make every such duplicate an unbounded re-apply. Asserted via the
/// pre-insert hook and the stored value, the only externally observable effects of the internal
/// `>` comparison.
#[tokio::test]
async fn equal_stamp_update_is_not_reapplied() {
    let config = Config::default()
        .with_port(0)
        .with_listen_addr("127.0.0.150".parse().unwrap())
        .with_insecure_no_key();
    let engine = Replica::<i32, u8>::new(config).await.expect("bind failed");

    let stamp = Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(1_000), LogicalCounter::new(0)),
        NodeId::new(0),
    );
    let key = 7;
    engine.just_insert(key, Entry::present(stamp, 1));

    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    *engine.pre_insert.write() = Box::new(move |_: &i32, _: &Entry<Timestamp, u8>| {
        calls2.fetch_add(1, Ordering::SeqCst);
    });

    // Same key, same stamp, a different value: only the stamp comparison decides whether this
    // is re-applied, so the value must not matter.
    let bytes = update_message_bytes(key, Entry::present(stamp, 2));
    let payload = auth::Authenticator::new(None, false)
        .open(&bytes)
        .expect("unauthenticated mode clears any datagram")
        .check_version()
        .expect("update_message_bytes stamps the current wire version");
    let peer: SocketAddr = "127.0.0.151:9".parse().unwrap();
    let payload = payload
        .verify_replay(&engine.replay_filter, peer.ip())
        .expect("unauthenticated mode is exempt from the replay check");
    let mut send_buf = Vec::new();
    engine.handle_messages(payload, peer, &mut send_buf).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an equal-stamp Update must not be re-applied (pre-insert hook must not run)"
    );
    assert_eq!(
        engine.map.read().get(&key).and_then(|v| v.value().copied()),
        Some(1),
        "an equal-stamp Update must not overwrite the locally-held value"
    );
}
