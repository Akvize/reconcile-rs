#[cfg(test)]
mod deadlock_regressions {

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
                                Hlc::new(
                                    PhysicalTime::from_millis(u64::MAX),
                                    LogicalCounter::new(1),
                                ),
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
}

#[cfg(test)]
mod auth_attack {
    use std::time::Duration;

    use bincode::{DefaultOptions, Serializer};
    use chrono::Utc;
    use serde::Serialize;
    use tokio::net::UdpSocket;

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
        let task = tokio::spawn(store.clone().run());

        let attacker = UdpSocket::bind("127.0.0.49:0").await.unwrap();
        let target = format!("{victim_addr}:{port}");
        let forged = forged_update();

        // (a) forged update sent WITHOUT any authentication tag
        attacker.send_to(&forged, &target).await.unwrap();
        // (b) forged update sealed with the WRONG key
        let wrong_key_sealed =
            auth::Authenticator::new(Some(auth::ClusterKey::new([0x99u8; auth::KEY_LEN])), false)
                .seal(
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
}

#[cfg(test)]
mod causal_stability {
    use std::net::IpAddr;

    use bincode::{DefaultOptions, Deserializer};
    use serde::Deserialize;

    use crate::clock::{Hlc, LogicalCounter, NodeId, PhysicalTime, Timestamp};
    use crate::entry::{Entry, State};
    use crate::replica::{version_hash, Message, Replica};
    use crate::replicated_map::Config;

    type Tombstoned = Entry<Timestamp, i32>;

    async fn engine(addr: &str) -> Replica<i32, i32> {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr(addr.parse().unwrap())
            .with_insecure_no_key();
        Replica::new(config).await.expect("bind failed")
    }

    #[tokio::test]
    async fn tombstone_not_stable_until_all_members_ack() {
        let eng = engine("127.0.0.60").await;
        let peer_a: IpAddr = "127.0.0.61".parse().unwrap();
        let peer_b: IpAddr = "127.0.0.62".parse().unwrap();
        let key = 7;
        let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        let version = version_hash(&tombstone);

        // No member known yet: nothing could resurrect the value, so GC is allowed.
        assert!(eng.is_tombstone_stable(&key, version));

        // Two members known, neither has acknowledged: not stable.
        eng.members.write().insert(peer_a);
        eng.members.write().insert(peer_b);
        assert!(!eng.is_tombstone_stable(&key, version));

        // Only one member acknowledges: still not stable.
        eng.tombstone_acks
            .write()
            .entry(key)
            .or_default()
            .insert(peer_a, version);
        assert!(!eng.is_tombstone_stable(&key, version));

        // A stale acknowledgment (wrong version) from the other member does not count.
        eng.tombstone_acks
            .write()
            .entry(key)
            .or_default()
            .insert(peer_b, version.wrapping_add(1));
        assert!(!eng.is_tombstone_stable(&key, version));

        // The correct acknowledgment from every member makes it stable.
        eng.tombstone_acks
            .write()
            .entry(key)
            .or_default()
            .insert(peer_b, version);
        assert!(eng.is_tombstone_stable(&key, version));
    }

    /// Every reconciliation round must resend an `Ack` for each tombstone the node currently
    /// holds, so the causal-stability ack matrix keeps converging at three or more nodes (where a
    /// held tombstone is otherwise never re-advertised once two replicas agree on it).
    /// Deterministic, socket-free: insert a tombstone, run one round, and inspect the datagram.
    #[tokio::test]
    async fn reconciliation_round_resends_acks_for_held_tombstones() {
        let eng = engine("127.0.0.66").await;
        let live_key = 1;
        let tombstone_key = 2;
        // A live value's ack must NOT be resent; a tombstone's must be.
        eng.just_insert(
            live_key,
            Entry::present(
                Timestamp::new(
                    Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                    NodeId::new(0),
                ),
                11,
            ),
        );
        let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(2), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        let expected_version = version_hash(&tombstone);
        eng.just_insert(tombstone_key, tombstone);

        let mut buf = Vec::new();
        eng.start_reconciliation(&mut buf).await;

        // Decode every message in the datagram and collect the acks.
        let mut acks: Vec<(i32, u64)> = Vec::new();
        let mut de = Deserializer::from_slice(&buf, DefaultOptions::new());
        while let Ok(msg) = Message::<i32, Tombstoned, State<i32>>::deserialize(&mut de) {
            if let Message::Ack(ack) = msg {
                acks.push(ack);
            }
        }

        assert!(
            acks.contains(&(tombstone_key, expected_version)),
            "the round must resend the ack for the held tombstone (key {tombstone_key}); got \
             {acks:?}"
        );
        assert!(
            !acks.iter().any(|(k, _)| *k == live_key),
            "a live value's ack must never be resent; got {acks:?}"
        );
    }

    #[tokio::test]
    async fn decommission_releases_a_silent_peer() {
        let eng = engine("127.0.0.63").await;
        let live: IpAddr = "127.0.0.64".parse().unwrap();
        let gone: IpAddr = "127.0.0.65".parse().unwrap();
        let key = 9;
        let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        let version = version_hash(&tombstone);

        eng.members.write().insert(live);
        eng.members.write().insert(gone);
        eng.tombstone_acks
            .write()
            .entry(key)
            .or_default()
            .insert(live, version);
        // `gone` never acknowledges: not stable.
        assert!(!eng.is_tombstone_stable(&key, version));

        // Decommissioning the silent peer makes the tombstone stable.
        eng.decommission_peer(gone);
        assert!(eng.is_tombstone_stable(&key, version));

        // Forgetting the tombstone clears its bookkeeping.
        eng.forget_tombstone(&key);
        assert!(eng.tombstone_acks.read().get(&key).is_none());
    }

    /// A tombstone with zero acknowledgments — the state a deletion is in the instant it happens —
    /// must read as pending. This is the exact case a predicate walking `tombstone_acks` (rather than
    /// `live_tombstones`) gets wrong, because no entry exists there until the first ack arrives.
    #[tokio::test]
    async fn fresh_tombstone_with_zero_acks_is_pending() {
        let eng = engine("127.0.0.67").await;
        let peer: IpAddr = "127.0.0.68".parse().unwrap();
        let key = 3;
        eng.just_insert(
            key,
            Entry::tombstone(Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                NodeId::new(0),
            )),
        );

        assert!(
            eng.tombstone_acks.read().get(&key).is_none(),
            "test setup: no ack should exist yet"
        );
        assert!(
            eng.has_pending_tombstone_acks(peer),
            "a freshly deleted, zero-ack tombstone must count as pending"
        );
    }

    #[tokio::test]
    async fn no_local_tombstones_is_never_pending() {
        let eng = engine("127.0.0.69").await;
        let peer: IpAddr = "127.0.0.70".parse().unwrap();
        eng.just_insert(
            1,
            Entry::present(
                Timestamp::new(
                    Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
                    NodeId::new(0),
                ),
                42,
            ),
        );
        assert!(!eng.has_pending_tombstone_acks(peer));
    }

    #[tokio::test]
    async fn acked_tombstone_is_not_pending_but_stale_or_missing_ack_is() {
        let eng = engine("127.0.0.71").await;
        let acked: IpAddr = "127.0.0.72".parse().unwrap();
        let stale: IpAddr = "127.0.0.73".parse().unwrap();
        let silent: IpAddr = "127.0.0.74".parse().unwrap();
        let key = 5;
        let tombstone: Tombstoned = Entry::tombstone(Timestamp::new(
            Hlc::new(PhysicalTime::from_millis(1), LogicalCounter::new(0)),
            NodeId::new(0),
        ));
        let version = version_hash(&tombstone);
        eng.just_insert(key, tombstone);

        eng.tombstone_acks
            .write()
            .entry(key)
            .or_default()
            .insert(acked, version);
        eng.tombstone_acks
            .write()
            .entry(key)
            .or_default()
            .insert(stale, version.wrapping_add(1));

        assert!(!eng.has_pending_tombstone_acks(acked));
        assert!(eng.has_pending_tombstone_acks(stale));
        assert!(eng.has_pending_tombstone_acks(silent));
    }
}

#[cfg(test)]
mod clock_port {
    use std::sync::Arc;

    use crate::clock::{Hlc, LogicalCounter, ManualClock, NodeId, PhysicalTime, Timestamp};
    use crate::replica::Replica;
    use crate::replicated_map::Config;

    /// The engine mints timestamps only through the injected [`Clock`](crate::clock::Clock) port, so
    /// a deterministic adapter makes `clock_now()` fully reproducible — no wall-clock time involved.
    /// This is the engine-level testability the port exists to provide.
    #[tokio::test]
    async fn engine_mints_through_the_injected_clock() {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr("127.0.0.70".parse().unwrap())
            .with_insecure_no_key();
        let clock = Arc::new(ManualClock::new(NodeId::new(42)));
        let eng: Replica<i32, i32> = Replica::new_with_clock(config, clock)
            .await
            .expect("bind failed");

        assert_eq!(
            eng.clock_now(),
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(0), LogicalCounter::new(1)),
                NodeId::new(42)
            )
        );
        assert_eq!(
            eng.clock_now(),
            Timestamp::new(
                Hlc::new(PhysicalTime::from_millis(0), LogicalCounter::new(2)),
                NodeId::new(42)
            )
        );
    }
}

#[cfg(test)]
mod pacing {
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
        send_messages_paced(messages, &ports, &peer, &mut send_buf, rate).await;
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
                    .with_port(crate::replica::next_ephemeral_test_port())
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
}

#[cfg(test)]
mod socket_buffers {
    use socket2::SockRef;

    use crate::transport::UdpTransport;

    async fn bound(addr: &str, recv: Option<usize>, send: Option<usize>) -> UdpTransport {
        UdpTransport::bind(addr.parse().unwrap(), recv, send)
            .await
            .expect("bind failed")
    }

    /// The receive-buffer knob must size the socket. Asserted as monotonicity, not an absolute
    /// count: the kernel clamps `SO_RCVBUF` to a per-host `net.core.rmem_max`.
    #[tokio::test]
    async fn recv_buffer_size_is_configurable() {
        let big = bound("127.0.0.90:0", Some(4 * 1024 * 1024), None).await;
        // An explicit, tiny request — well below any plausible rmem_max, so it is honoured as-is.
        let small = bound("127.0.0.91:0", Some(8 * 1024), None).await;

        let big_buf = SockRef::from(big.socket()).recv_buffer_size().unwrap();
        let small_buf = SockRef::from(small.socket()).recv_buffer_size().unwrap();

        assert!(
            big_buf > small_buf,
            "the multi-MiB request ({big_buf} B) should exceed an explicitly tiny buffer \
             ({small_buf} B)"
        );
    }

    /// `None` opts out of the tuning entirely, leaving the inherited OS default: the call path does
    /// not panic and the socket has a positive receive buffer.
    #[tokio::test]
    async fn recv_buffer_size_none_leaves_os_default() {
        let t = bound("127.0.0.92:0", None, None).await;
        let buf = SockRef::from(t.socket()).recv_buffer_size().unwrap();
        assert!(buf > 0, "a socket always has a positive receive buffer");
    }
}

#[cfg(test)]
mod tombstone_ack_bounds {
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
            .with_port(crate::replica::next_ephemeral_test_port())
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
}

#[cfg(test)]
mod dump_budget {
    use crate::replicated_map::Config;

    /// Default budget is 4, matching DEFAULT_MAX_CONCURRENT_BULK_DUMPS.
    #[test]
    fn default_budget_is_four() {
        assert_eq!(Config::default().max_concurrent_bulk_dumps, 4);
    }

    /// `with_max_concurrent_bulk_dumps` overrides the value.
    #[test]
    fn builder_sets_budget() {
        let cfg = Config::default().with_max_concurrent_bulk_dumps(1);
        assert_eq!(cfg.max_concurrent_bulk_dumps, 1);
    }

    /// With a budget of 1, claiming a second slot before the first is released must fail.
    /// After the first slot is dropped its count returns to zero and a fresh claim succeeds.
    #[tokio::test]
    async fn budget_guard_limits_and_releases_slots() {
        use crate::replica::Replica;

        let config = Config::default()
            .with_port(crate::replica::next_ephemeral_test_port())
            .with_listen_addr("127.0.0.99".parse().unwrap())
            .with_max_concurrent_bulk_dumps(1)
            .with_insecure_no_key();
        let eng = Replica::<i32, i32>::new(config).await.expect("bind failed");

        let peer_a: std::net::SocketAddr = "127.0.0.100:9001".parse().unwrap();
        let peer_b: std::net::SocketAddr = "127.0.0.101:9001".parse().unwrap();

        // First claim succeeds.
        let slot_a = eng.try_claim_dump_slot(peer_a);
        assert!(slot_a.is_some(), "first slot must be available");
        assert_eq!(eng.bulk_dumps_in_flight_count(), 1);

        // Second claim (different peer) is rejected — budget exhausted.
        let slot_b = eng.try_claim_dump_slot(peer_b);
        assert!(slot_b.is_none(), "second slot must be rejected at budget 1");
        assert_eq!(eng.bulk_dumps_in_flight_count(), 1);

        // Releasing the first slot (drop) frees it for the next caller.
        drop(slot_a);
        assert_eq!(eng.bulk_dumps_in_flight_count(), 0);

        // Now peer_b's retry succeeds.
        let slot_b_retry = eng.try_claim_dump_slot(peer_b);
        assert!(
            slot_b_retry.is_some(),
            "slot must be available after release"
        );
        drop(slot_b_retry);
    }
}

#[cfg(test)]
mod in_memory_convergence {
    //! Deterministic convergence over the [`InMemoryTransport`](crate::transport::InMemoryTransport):
    //! two engines exchange datagrams entirely in-process — no real sockets — so the anti-entropy
    //! protocol is exercised without any network flakiness. The engine reaches its I/O only through
    //! the `Transport` port, which is what makes this substitution possible.
    use std::collections::BTreeMap;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use proptest::prelude::*;

    use crate::clock::{ManualClock, NodeId};
    use crate::entry::Entry;
    use crate::replica::Replica;
    use crate::replicated_map::Config;
    use crate::transport::InMemoryNetwork;

    /// Live (value-only) view of an engine's map, for comparing convergence regardless of stamps.
    fn live_view(eng: &Replica<u32, u32>) -> BTreeMap<u32, u32> {
        eng.map
            .read()
            .iter()
            .filter_map(|(k, e)| e.value().map(|v| (*k, *v)))
            .collect()
    }

    /// Load `entries` into engine A and drive both engines over an in-memory network until B's live
    /// view matches A's (or a short deadline elapses). Returns whether they converged.
    async fn converges(entries: &[(u32, u32)]) -> bool {
        let net = InMemoryNetwork::new();
        let port = 5000u16;
        let a_ip: IpAddr = "127.0.0.2".parse().unwrap();
        let b_ip: IpAddr = "127.0.0.3".parse().unwrap();
        let cfg = |ip: IpAddr| {
            Config::default()
                .with_listen_addr(ip)
                .with_port(port)
                .with_reconcile_interval(Duration::from_millis(5))
                .with_insecure_no_key()
        };
        let a: Replica<u32, u32> = Replica::new_with_transport(
            cfg(a_ip),
            Arc::new(net.bind(SocketAddr::new(a_ip, port))),
            Arc::new(ManualClock::new(NodeId::new(1))),
        );
        let b: Replica<u32, u32> = Replica::new_with_transport(
            cfg(b_ip),
            Arc::new(net.bind(SocketAddr::new(b_ip, port))),
            Arc::new(ManualClock::new(NodeId::new(2))),
        );
        // Seed each as the other's known gossip peer (no real discovery over the in-memory fabric).
        a.peers.write().insert(b_ip, Instant::now());
        b.peers.write().insert(a_ip, Instant::now());
        // Load A only; B must learn every entry purely through anti-entropy.
        for (k, v) in entries {
            a.just_insert(*k, Entry::present(a.clock_now(), *v));
        }
        let want = live_view(&a);

        let ta = tokio::spawn(a.clone().run());
        let tb = tokio::spawn(b.clone().run());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut converged = false;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if live_view(&b) == want {
                converged = true;
                break;
            }
        }
        ta.abort();
        tb.abort();
        converged
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(12))]

        /// For any small map, a cold replica converges to it over the in-memory transport.
        #[test]
        fn cold_replica_converges_over_in_memory_transport(
            entries in prop::collection::vec((0u32..64, 0u32..1000), 0..24)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let ok = rt.block_on(converges(&entries));
            prop_assert!(ok, "B did not converge to A's {} entries over the in-memory transport", entries.len());
        }
    }
}
