// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::time::Duration;

use bincode::{DefaultOptions, Serializer};
use chrono::Utc;
use serde::Serialize;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use super::super::Message;
use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
use crate::entry::{Entry, State};
use crate::{replicated_map::Config, ReplicatedMap};
use gossip::auth;

/// Serialize the F3 attack payload: an `Update` with a far-future timestamp that, if merged,
/// would win against every legitimate write forever.
fn forged_update() -> Vec<u8> {
    let far_future = Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(u64::MAX), LogicalCounter::new(0)),
        NodeId::new(0),
    );
    let message = Message::Update::<i32, Entry<Timestamp, String>, State<String>>((
        0,
        Entry::present(far_future, "evil".to_string()),
    ));
    let mut buf = Vec::new();
    message
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    buf
}

/// A node with a cluster key must drop forged datagrams (no tag, or wrong key) before
/// deserialization, so an attacker cannot poison it via last-write-wins.
#[tokio::test(flavor = "multi_thread")]
async fn forged_datagram_is_ignored() {
    let key = [0x42u8; auth::KEY_LEN];
    let port = 8082;
    let victim_addr = "127.0.0.48";
    let config = Config::default()
        .with_port(port)
        .with_listen_addr(victim_addr.parse().unwrap())
        .with_cluster_key(auth::ClusterKey::new(key));
    let store = ReplicatedMap::<i32, String>::new(config)
        .await
        .expect("bind failed");
    store.just_insert(0, "legit".to_string());
    let task = tokio::spawn(store.clone().run(CancellationToken::new()));

    let attacker = UdpSocket::bind("127.0.0.49:0").await.unwrap();
    let target = format!("{victim_addr}:{port}");
    let forged = forged_update();

    // (a) forged update sent WITHOUT any authentication tag
    attacker.send_to(&forged, &target).await.unwrap();
    // (b) forged update sealed with the WRONG key
    let wrong_key_sealed =
        auth::Authenticator::new(Some(auth::ClusterKey::new([0x99u8; auth::KEY_LEN])), false).seal(
            gossip::replay::Seq::new(1),
            gossip::replay::Stamp::new(Utc::now().timestamp_millis().max(0) as u64),
            &forged,
        );
    attacker.send_to(&wrong_key_sealed, &target).await.unwrap();

    // give the victim time to (not) process the forged datagrams
    tokio::time::sleep(Duration::from_millis(200)).await;

    // the legitimate value must be untouched
    assert_eq!(store.get(&0).as_deref(), Some(&"legit".to_string()));

    task.abort();
}
