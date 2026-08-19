// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::entry::{Entry, State};
use crate::replica::Replica;
use crate::replicated_map::Config;

use super::super::{Message, MAX_MESSAGES_PER_DATAGRAM};

async fn engine(addr: &str) -> Replica<i32, i32> {
    let config = Config::default()
        .with_port(crate::replica::tests::next_ephemeral_test_port())
        .with_listen_addr(addr.parse().unwrap())
        .with_insecure_no_key();
    Replica::new(config).await.expect("bind failed")
}

/// `resend_held_tombstone_acks`' return value is discarded by its only caller
/// (`start_reconciliation`) and otherwise feeds only a metrics counter — so a wrong count is
/// invisible short of decoding the wire output back out, which is what this asserts: the
/// returned count must equal the number of `Ack` messages actually appended to `send_buf`.
#[tokio::test]
async fn returned_count_matches_acks_actually_appended() {
    let eng = engine("127.0.0.160").await;
    let n: i32 = 5;
    for key in 0..n {
        eng.just_insert(
            key,
            Entry::tombstone(Timestamp::new(
                Hlc::new(
                    PhysicalTime::from_millis(key as u64 + 1),
                    LogicalCounter::new(0),
                ),
                NodeId::new(0),
            )),
        );
    }

    let mut send_buf = Vec::new();
    let appended = eng.resend_held_tombstone_acks(&mut send_buf, 0);

    assert_eq!(
        appended, n as usize,
        "every held tombstone must be reported as appended when well under the byte budget"
    );

    let decoded: Vec<Message<i32, Entry<Timestamp, i32>, State<i32>>> =
        gossip::bincode::decode_stream(&send_buf, MAX_MESSAGES_PER_DATAGRAM)
            .expect("resend_held_tombstone_acks writes valid Message encodings");
    let acks = decoded
        .iter()
        .filter(|m| matches!(m, Message::Ack(_)))
        .count();
    assert_eq!(
        acks, appended,
        "the returned count must equal the number of Ack messages actually written to send_buf"
    );
}
