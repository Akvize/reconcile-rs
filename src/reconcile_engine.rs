// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReconcileEngine`], the inner layer of the [`ReconcileStore`](crate::reconcile_store::ReconcileStore)
//! that handles communication between instances at the network level.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::diff::HashRangeQueryable;
use crate::diff::{Diffable, HashSegment};
use crate::gen_ip::gen_ip;
use crate::reconcilable::{MaybeTombstone, Reconcilable};
use crate::reconcile_store::Config;
use crate::HRTree;

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);
const PEER_EXPIRATION: Duration = Duration::from_secs(60);

const MAX_SENDTO_RETRIES: u32 = 4;

type PreInsertCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &V)>;

/// Compute a deterministic, cross-node version token for a value.
///
/// Used to acknowledge a *specific* version of a tombstone across replicas: a peer
/// acknowledges the exact value it holds, so a stale acknowledgment for an older
/// tombstone cannot authorize GC of a newer one. [`DefaultHasher::new`] uses fixed
/// keys, so the same value hashes identically on every node.
pub(crate) fn version_hash<V: Hash>(value: &V) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// The internal reconciliation engine at the network level.
/// This struct does not handle removals, which are managed by the external layer.
/// For more information, see [`ReconcileStore`](crate::reconcile_store::ReconcileStore).
pub(crate) struct ReconcileEngine<K, V> {
    pub(crate) map: Arc<RwLock<HRTree<K, V>>>,
    port: u16,
    socket: Arc<UdpSocket>,
    peer_net: IpNet,
    rng: Arc<RwLock<StdRng>>,
    pub(crate) peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    pub(crate) pre_insert: Arc<RwLock<PreInsertCallback<K, V>>>,
    /// Monotonic set of every peer this node has ever exchanged messages with.
    ///
    /// Unlike [`peers`](Self::peers), entries are never expired automatically: a tombstone
    /// must be acknowledged by every member (or the member explicitly decommissioned) before
    /// it can be garbage-collected. This is what gates tombstone GC on *causal stability* and
    /// prevents a peer that is partitioned for longer than the peer-expiration window from
    /// resurrecting a deleted value on its return.
    pub(crate) members: Arc<RwLock<HashSet<IpAddr>>>,
    /// Per-tombstone acknowledgments: `key -> (peer -> version token of the tombstone it holds)`.
    pub(crate) tombstone_acks: Arc<RwLock<HashMap<K, HashMap<IpAddr, u64>>>>,
}

impl<K, V> Clone for ReconcileEngine<K, V> {
    fn clone(&self) -> Self {
        ReconcileEngine {
            map: self.map.clone(),
            port: self.port,
            socket: self.socket.clone(),
            peer_net: self.peer_net,
            rng: self.rng.clone(),
            peers: self.peers.clone(),
            pre_insert: self.pre_insert.clone(),
            members: self.members.clone(),
            tombstone_acks: self.tombstone_acks.clone(),
        }
    }
}

/// Represent an atomic message for the reconciliation protocol.
#[derive(Clone, Debug, Deserialize, Serialize)]
enum Message<K: Serialize, V: Serialize> {
    /// Provides information about a set of keys that allows checking
    /// whether there are differences between the two instances over this set
    ComparisonItem(HashSegment<K>),
    /// Provides an individual key-value pair when the protocol
    /// has identified that it differs on the two instances
    Update((K, V)),
    /// Acknowledges that the sender now holds the tombstone for the given key with the
    /// given version token (see [`version_hash`]). Enables causal-stability-gated
    /// tombstone garbage collection on the receiver.
    Ack((K, u64)),
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone
            + DeserializeOwned
            + Hash
            + MaybeTombstone
            + PartialEq
            + Reconcilable
            + Send
            + Serialize
            + Sync
            + 'static,
    > ReconcileEngine<K, V>
{
    pub async fn new(config: Config) -> Self {
        let socket = UdpSocket::bind(SocketAddr::new(config.listen_addr, config.port))
            .await
            .unwrap();
        debug!("Listening on: {}", socket.local_addr().unwrap());
        let map = HRTree::<K, V>::new();
        ReconcileEngine {
            map: Arc::new(RwLock::new(map)),
            port: config.port,
            socket: Arc::new(socket),
            peer_net: config.peer_net,
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            pre_insert: Arc::new(RwLock::new(Box::new(|_, _| {}))),
            members: Arc::new(RwLock::new(HashSet::new())),
            tombstone_acks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> u64 {
        self.map.read().hash(&range)
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        // 1) Call the pre-insert hook *before* taking the map lock
        (self.pre_insert.read())(&key, &value);

        // 2) Now acquire write lock and insert
        let mut guard = self.map.write();
        guard.insert(key.clone(), value.clone())
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.just_insert(key.clone(), value.clone());
        let peers = self.get_peers();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let message = Message::Update::<K, V>((key, value));
            let messages = vec![message];
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(&messages, Arc::clone(&socket), &peer, &mut send_buf).await;
            }
        });
        ret
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        // 1) First run all pre-insert hooks outside the map lock
        for (key, value) in key_values {
            (self.pre_insert.read())(key, value);
        }
        // 2) Then acquire the write lock once and insert them
        let mut guard = self.map.write();
        for (key, value) in key_values {
            guard.insert(key.clone(), value.clone());
        }
    }

    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.just_insert_bulk(key_values);
        let peers = self.get_peers();
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, V>(kv.clone()))
            .collect();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        tokio::spawn(async move {
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(&messages, Arc::clone(&socket), &peer, &mut send_buf).await;
            }
        });
    }

    pub async fn run(self) {
        // extra byte that easily detect when the buffer is too small
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        let recv_timeout = ACTIVITY_TIMEOUT;
        // start the protocol at the beginning
        self.start_reconciliation(&mut send_buf).await;
        // infinite loop
        loop {
            match timeout(recv_timeout, self.socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    // timeout
                    debug!("no recent activity; initiating diff protocol");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => {
                    // network error
                    warn!("network error in recv_from: {err}");
                }
                Ok(Ok((size, peer))) => {
                    // received datagram
                    if peer.port() != self.port {
                        warn!(
                            "received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    self.handle_messages(&recv_buf, (size, peer), &mut send_buf)
                        .await;
                    let now = Instant::now();
                    let addr = peer.ip();
                    self.peers.write().insert(addr, now);
                    // Record the peer as a permanent member so that its acknowledgment is
                    // required before any tombstone can be garbage-collected (causal stability).
                    self.members.write().insert(addr);
                }
            }
        }
    }

    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let segments = {
            let guard = self.map.read();
            guard.start_diff()
        };
        send_buf.clear();
        for segment in segments {
            Message::ComparisonItem::<K, V>(segment)
                .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                .unwrap();
        }
        let mut peers = self.get_peers();
        // select a random address out of the peer network
        // NOTE: the random address might not correspond to a real peer, so we do not add it to the
        // list of known peers, just to our local copies of the addresses; if a peer exists at this
        // address, they will eventually send us a message in return, and we will add them to the
        // list of known peer
        let addr = gen_ip(&mut *self.rng.write(), self.peer_net);
        peers.push(addr);
        // initiate the reconciliation protocol with all the known peers, and a random one
        for peer in peers {
            trace!("start_diff {} bytes to {peer}", send_buf.len());
            send_to_retry(&self.socket, send_buf, (peer, self.port))
                .await
                .unwrap();
        }
    }

    async fn handle_messages(
        &self,
        recv_buf: &[u8],
        (size, peer): (usize, SocketAddr),
        send_buf: &mut Vec<u8>,
    ) {
        if size == recv_buf.len() {
            warn!("Buffer too small for message, discarded");
            return;
        }
        trace!("received {} bytes from {peer}", size);
        let mut in_comparison = Vec::new();
        let mut updates = Vec::new();
        let mut acks: Vec<(K, u64)> = Vec::new();
        let mut deserializer = Deserializer::from_slice(&recv_buf[..size], DefaultOptions::new());
        // read messages in buffer
        loop {
            match Message::deserialize(&mut deserializer) {
                Err(ref kind) => {
                    if let bincode::ErrorKind::Io(err) = kind.as_ref() {
                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                            break;
                        }
                    }
                    panic!("failed to deserialize message: {:?}", kind);
                }
                Ok(Message::ComparisonItem(segment)) => in_comparison.push(segment),
                Ok(Message::Update(update)) => updates.push(update),
                Ok(Message::Ack(ack)) => acks.push(ack),
            }
        }
        // record tombstone acknowledgments received from the peer
        if !acks.is_empty() {
            let peer_ip = peer.ip();
            // Reciprocate acks for tombstones we also hold, so that the deletion originator
            // (which never receives the tombstone as an inbound update, hence is never acked
            // by the peer) can still reach causal stability. The `already`-guard bounds this
            // to at most one extra round-trip and prevents an infinite ack ping-pong.
            let mut reciprocal_acks = Vec::new();
            {
                let map_guard = self.map.read();
                let mut guard = self.tombstone_acks.write();
                for (key, version) in acks {
                    let entry = guard.entry(key.clone()).or_default();
                    let already = entry.get(&peer_ip) == Some(&version);
                    entry.insert(peer_ip, version);
                    if !already {
                        let holds_same_tombstone = map_guard
                            .get(&key)
                            .is_some_and(|v| v.is_tombstone() && version_hash(v) == version);
                        if holds_same_tombstone {
                            reciprocal_acks.push(Message::Ack::<K, V>((key, version)));
                        }
                    }
                }
            }
            if !reciprocal_acks.is_empty() {
                send_messages_to(&reciprocal_acks, Arc::clone(&self.socket), &peer, send_buf).await;
            }
        }
        // handle messages
        if !in_comparison.is_empty() {
            debug!("received {} segments", in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.map.read();
                guard.diff_round(in_comparison, &mut out_comparison, &mut differences);
            }
            let mut messages = Vec::new();
            if !out_comparison.is_empty() {
                debug!("returning {} segments", out_comparison.len());
                trace!("segments: {out_comparison:?}");
                for segment in out_comparison {
                    messages.push(Message::ComparisonItem::<K, V>(segment))
                }
            }
            if !differences.is_empty() {
                debug!("returning {} diff_ranges", differences.len());
                trace!("diff_ranges: {differences:?}");
                let guard = self.map.read();
                for range in differences {
                    for update in guard.get_range(&range).map(|(k, v)| (k.clone(), v.clone())) {
                        messages.push(Message::Update(update));
                    }
                }
            }
            if !messages.is_empty() {
                send_messages_to(&messages, Arc::clone(&self.socket), &peer, send_buf).await;
            }
        }
        if !updates.is_empty() {
            debug!("received {} updates", updates.len());
            // Tombstones we now hold as a result of these updates, to be acknowledged back to
            // the peer so it can eventually garbage-collect them once causally stable.
            let mut acks_to_send = Vec::new();
            {
                let mut guard = self.map.write();
                for (k, remote_v) in updates {
                    let tombstone_version = match guard.get(&k) {
                        Some(local_v) => {
                            let merged_v = local_v.reconcile(&remote_v);
                            if merged_v != *local_v {
                                (self.pre_insert.read())(&k, &merged_v);
                                let version =
                                    merged_v.is_tombstone().then(|| version_hash(&merged_v));
                                guard.insert(k.clone(), merged_v);
                                version
                            } else {
                                // We already hold an equal-or-newer value; still acknowledge it
                                // if it is the same tombstone, so the peer learns we have it.
                                local_v.is_tombstone().then(|| version_hash(local_v))
                            }
                        }
                        None => {
                            (self.pre_insert.read())(&k, &remote_v);
                            let version = remote_v.is_tombstone().then(|| version_hash(&remote_v));
                            guard.insert(k.clone(), remote_v);
                            version
                        }
                    };
                    if let Some(version) = tombstone_version {
                        acks_to_send.push(Message::Ack::<K, V>((k, version)));
                    }
                }
            }
            if !acks_to_send.is_empty() {
                send_messages_to(&acks_to_send, Arc::clone(&self.socket), &peer, send_buf).await;
            }
        }
    }

    /// Returns `true` if the tombstone for `key` with the given version token has been
    /// acknowledged by every member (causal stability) and is therefore safe to
    /// garbage-collect.
    ///
    /// When no other replica is known, there is nothing that could resurrect the value, so
    /// GC is allowed.
    pub(crate) fn is_tombstone_stable(&self, key: &K, version: u64) -> bool {
        let members = self.members.read();
        if members.is_empty() {
            return true;
        }
        let acks = self.tombstone_acks.read();
        let Some(key_acks) = acks.get(key) else {
            return false;
        };
        members
            .iter()
            .all(|peer| key_acks.get(peer) == Some(&version))
    }

    /// Drop the acknowledgment bookkeeping for a key once its tombstone has been collected.
    pub(crate) fn forget_tombstone(&self, key: &K) {
        self.tombstone_acks.write().remove(key);
    }

    /// Permanently remove a peer from the membership set so that tombstones are no longer held
    /// back waiting for its acknowledgment. Use when a replica is decommissioned or will never
    /// return. Also clears any acknowledgments it had recorded.
    pub(crate) fn decommission_peer(&self, peer: IpAddr) {
        self.members.write().remove(&peer);
        for key_acks in self.tombstone_acks.write().values_mut() {
            key_acks.remove(&peer);
        }
    }
}

async fn send_to_retry<A: ToSocketAddrs>(
    socket: &UdpSocket,
    buf: &[u8],
    target: A,
) -> std::io::Result<usize> {
    let mut res = Ok(0);
    for _ in 0..MAX_SENDTO_RETRIES {
        res = socket.send_to(buf, &target).await;
        if res.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    res
}

async fn send_messages_to<K: Serialize, V: Serialize>(
    messages: &[Message<K, V>],
    socket: Arc<UdpSocket>,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
) {
    debug!("sending {} messages to {peer}", messages.len());
    send_buf.clear();
    for message in messages {
        let last_size = send_buf.len();
        message
            .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
            .unwrap();
        if send_buf.len() > BUFFER_SIZE {
            trace!("sending {} bytes to {peer}", last_size);
            send_to_retry(&socket, &send_buf[..last_size], &peer)
                .await
                .unwrap();
            trace!("sent {} bytes to {peer}", last_size);
            send_buf.drain(..last_size);
        }
    }
    trace!("sending last {} bytes to {peer}", send_buf.len());
    send_to_retry(&socket, send_buf, &peer).await.unwrap();
    trace!("sent last {} bytes to {peer}", send_buf.len());
}

#[cfg(test)]
mod deadlock_regressions {

    use crate::{reconcile_store::Config, ReconcileStore};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_insert_hook_can_call_insert_again_without_deadlock() {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr("127.0.0.44".parse().unwrap());
        // let tree = HRTree::from_iter(vec![(1, 10), (2, 20)]);
        let svc = ReconcileStore::new(config).await;
        svc.insert_bulk(&vec![(1, 10_u8)]);

        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();

        let hook_svc = svc.clone();
        // Install a pre-insert hook that itself calls insert on the same Store
        let once = Arc::new(AtomicBool::new(false));
        let guard = once.clone();
        svc.add_pre_insert(move |&k, &v| {
            // inner insert should no longer deadlock
            if !guard.swap(true, Ordering::SeqCst) {
                let _ = hook_svc.just_insert(k + 100, v.1.unwrap_or_default() + 100);
                // TODO Check vs Utc::now()
            }
            flag2.store(true, Ordering::SeqCst);
        });

        // Trigger our hook via a normal insert
        let _ = svc.just_insert(42, 99);
        assert!(
            flag.load(Ordering::SeqCst),
            "The pre-insert hook never ran to completion (likely deadlocked)"
        );
    }
}

#[cfg(test)]
mod causal_stability {
    use std::net::IpAddr;

    use chrono::{DateTime, Utc};

    use crate::reconcile_engine::{version_hash, ReconcileEngine};
    use crate::reconcile_store::Config;

    type Tombstoned = (DateTime<Utc>, Option<i32>);

    async fn engine(addr: &str) -> ReconcileEngine<i32, Tombstoned> {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr(addr.parse().unwrap());
        ReconcileEngine::new(config).await
    }

    #[tokio::test]
    async fn tombstone_not_stable_until_all_members_ack() {
        let eng = engine("127.0.0.60").await;
        let peer_a: IpAddr = "127.0.0.61".parse().unwrap();
        let peer_b: IpAddr = "127.0.0.62".parse().unwrap();
        let key = 7;
        let tombstone: Tombstoned = (Utc::now(), None);
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

    #[tokio::test]
    async fn decommission_releases_a_silent_peer() {
        let eng = engine("127.0.0.63").await;
        let live: IpAddr = "127.0.0.64".parse().unwrap();
        let gone: IpAddr = "127.0.0.65".parse().unwrap();
        let key = 9;
        let tombstone: Tombstoned = (Utc::now(), None);
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
}
