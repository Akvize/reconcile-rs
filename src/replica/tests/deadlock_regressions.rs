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
use crate::{replicated_map::Config, ReplicatedMap};
use bincode::{DefaultOptions, Serializer};
use gossip::auth;
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use super::super::Message;

#[tokio::test(flavor = "multi_thread")]
async fn pre_insert_hook_can_call_insert_again_without_deadlock() {
    let config = Config::default()
        .with_port(8080)
        .with_listen_addr("127.0.0.44".parse().unwrap())
        .with_insecure_no_key();
    let svc = ReplicatedMap::new(config).await.expect("bind failed");
    svc.insert_bulk(&[(1, 10_u8)]);

    let flag = Arc::new(AtomicBool::new(false));
    let flag2 = flag.clone();

    let hook_svc = svc.clone();
    // The hook itself calls `just_insert` on the same store, re-entering the pre-insert
    // path; guard against re-entering more than once so the hook cannot recurse forever.
    let once = Arc::new(AtomicBool::new(false));
    let guard = once.clone();
    svc.set_pre_insert(move |&k, v| {
        if !guard.swap(true, Ordering::SeqCst) {
            let _ = hook_svc.just_insert(k + 100, v.value().copied().unwrap_or_default() + 100);
        }
        flag2.store(true, Ordering::SeqCst);
    });

    let _ = svc.just_insert(42, 99);
    assert!(
        flag.load(Ordering::SeqCst),
        "The pre-insert hook never ran to completion (likely deadlocked)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_pre_insert_replaces_previous_hook() {
    let config = Config::default()
        .with_port(8080)
        .with_listen_addr("127.0.0.45".parse().unwrap())
        .with_insecure_no_key();
    let svc = ReplicatedMap::new(config).await.expect("bind failed");

    let first_ran = Arc::new(AtomicBool::new(false));
    let second_ran = Arc::new(AtomicBool::new(false));

    let first_ran2 = first_ran.clone();
    svc.set_pre_insert(move |_, _| {
        first_ran2.store(true, Ordering::SeqCst);
    });
    let second_ran2 = second_ran.clone();
    svc.set_pre_insert(move |_, _| {
        second_ran2.store(true, Ordering::SeqCst);
    });

    let _ = svc.just_insert(1, 1_u8);

    assert!(
        !first_ran.load(Ordering::SeqCst),
        "the first pre-insert hook ran after being replaced"
    );
    assert!(
        second_ran.load(Ordering::SeqCst),
        "the second pre-insert hook never ran"
    );
}

/// Serialize an `Update` so it can be fed straight into the engine's network ingest path
/// (`handle_messages`), exactly as a peer's datagram would arrive once the (disabled)
/// authentication gate has been cleared — including the leading wire-version byte every
/// datagram carries regardless of authentication mode.
fn update_message_bytes(key: i32, value: Entry<Timestamp, u8>) -> Vec<u8> {
    let message = Message::Update::<i32, Entry<Timestamp, u8>, State<u8>>((key, value));
    let mut buf = vec![gossip::auth::WIRE_VERSION];
    message
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .unwrap();
    buf
}

/// [`handle_messages`] must run the pre-insert hook outside the map write lock, or a hook
/// that re-inserts deadlocks the receive loop.
///
/// The failure mode is a hang, so the scenario runs on its own thread and the body waits with
/// `recv_timeout`.
#[test]
fn pre_insert_hook_can_call_insert_again_from_network_path_without_deadlock() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let reinserted = rt.block_on(async {
            let config = Config::default()
                .with_port(8083)
                .with_listen_addr("127.0.0.50".parse().unwrap())
                .with_insecure_no_key();
            let engine = Replica::<i32, u8>::new(config).await.expect("bind failed");

            // The same re-entrant hook as the direct-path test, registered on the engine whose
            // map `handle_messages` writes to: it calls back into `just_insert` on that engine.
            // Were `handle_messages` still running the hook under `map.write()`, this inner
            // insert would re-enter the write lock and deadlock.
            let hook_engine = engine.clone();
            let once = Arc::new(AtomicBool::new(false));
            let guard = once.clone();
            *engine.pre_insert.write() = Box::new(move |&k: &i32, v: &Entry<Timestamp, u8>| {
                if !guard.swap(true, Ordering::SeqCst) {
                    let inner = Entry::present(
                        Timestamp::new(
                            Hlc::new(PhysicalTime::from_millis(u64::MAX), LogicalCounter::new(1)),
                            NodeId::new(0),
                        ),
                        v.value().copied().unwrap_or_default() + 100,
                    );
                    let _ = hook_engine.just_insert(k + 100, inner);
                }
            });

            // Feed an `Update` (future timestamp, so it is integrated) through the real network
            // ingest path. No cluster key, so `open` clears the bytes unchanged into a `Payload`.
            let bytes = update_message_bytes(
                42,
                Entry::present(
                    Timestamp::new(
                        Hlc::new(PhysicalTime::from_millis(u64::MAX), LogicalCounter::new(0)),
                        NodeId::new(0),
                    ),
                    99,
                ),
            );
            let payload = auth::Authenticator::new(None, false)
                .open(&bytes)
                .expect("unauthenticated mode clears any datagram")
                .check_version()
                .expect("update_message_bytes stamps the current wire version");
            let peer: SocketAddr = "127.0.0.51:8083".parse().unwrap();
            let payload = payload
                .verify_replay(&engine.replay_filter, peer.ip())
                .expect("unauthenticated mode is exempt from the replay check");
            let mut send_buf = Vec::new();
            engine.handle_messages(payload, peer, &mut send_buf).await;

            // Read back the value the re-entrant hook inserted from the network path. The read
            // guard is dropped explicitly so it does not outlive `engine` at the block's end.
            let map_guard = engine.map.read();
            let reinserted = map_guard.get(&142).and_then(|v| v.value().copied());
            drop(map_guard);
            reinserted
        });
        let _ = tx.send(reinserted);
    });

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(reinserted) => assert_eq!(
            reinserted,
            Some(199),
            "the re-entrant insert from the network-path hook did not take effect"
        ),
        Err(_) => panic!(
            "the network-path pre-insert hook deadlocked (it ran under the map write lock, so \
             its re-entrant insert could not re-acquire the lock)"
        ),
    }
}
