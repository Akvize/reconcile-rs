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

use crate::auth;
use crate::diff::HashRangeQueryable;
use crate::diff::{Diffable, HashSegment};
use crate::fingerprint::Fingerprint;
use crate::gen_ip::gen_ip;
use crate::hlc::{Hlc, HlcClock, Timestamped};
use crate::reconcilable::{MaybeTombstone, Projectable, Reconcilable};
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
pub(crate) struct ReconcileEngine<K, V: Projectable> {
    pub(crate) map: Arc<RwLock<HRTree<K, V>>>,
    /// Value-only **projection** of [`map`](Self::map), kept in sync at every map mutation.
    ///
    /// Its range fingerprints are timestamp-less by construction (see
    /// [`ValueOnly`](crate::reconcilable::ValueOnly)), which is what lets a dateless mirror
    /// converge with this dated store over the existing range-diff protocol. It is read-only state
    /// for the dated↔dated path and never touches the #109 causal-stability bookkeeping.
    pub(crate) projection: Arc<RwLock<HRTree<K, V::Projected>>>,
    port: u16,
    socket: Arc<UdpSocket>,
    peer_net: IpNet,
    rng: Arc<RwLock<StdRng>>,
    pub(crate) peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    pub(crate) pre_insert: Arc<RwLock<PreInsertCallback<K, V>>>,
    /// Per-datagram authentication policy; carries the cluster key when enabled.
    authenticator: auth::Authenticator,
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
    /// This node's Hybrid Logical Clock, shared across all clones so that every write and
    /// every received timestamp advances the same clock.
    clock: Arc<HlcClock>,
}

impl<K, V: Projectable> Clone for ReconcileEngine<K, V> {
    fn clone(&self) -> Self {
        ReconcileEngine {
            map: self.map.clone(),
            projection: self.projection.clone(),
            port: self.port,
            socket: self.socket.clone(),
            peer_net: self.peer_net,
            rng: self.rng.clone(),
            peers: self.peers.clone(),
            pre_insert: self.pre_insert.clone(),
            authenticator: self.authenticator.clone(),
            members: self.members.clone(),
            tombstone_acks: self.tombstone_acks.clone(),
            clock: self.clock.clone(),
        }
    }
}

/// Represent an atomic message for the reconciliation protocol.
///
/// The three original variants form the **dated** channel (`ComparisonItem` / `Update` / `Ack`),
/// exchanged between full dated stores exactly as before — their bincode tags (0, 1, 2) and wire
/// bytes are unchanged. The two trailing variants form the additive **value-only** channel used by
/// lightweight dateless mirrors (issue #128): they are tagged 3 and 4, so a node that does not
/// understand them simply fails to deserialize and drops the datagram (the receive loop is hardened
/// against that), keeping the protocol backward compatible on a single port.
///
/// `V` is the dated value carried by `Update`; `P` is its timestamp-less
/// [`Projected`](crate::reconcilable::Projectable::Projected) form carried by `ValueUpdate`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum Message<K: Serialize, V: Serialize, P: Serialize> {
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
    /// Like [`ComparisonItem`](Message::ComparisonItem) but on the **value-only basis**: the
    /// fingerprints were computed over timestamp-less values. A dated store answers these by
    /// diffing against its value-only *projection* tree, never its dated map.
    ValueComparisonItem(HashSegment<K>),
    /// An individual timestamp-less update sent to a dateless mirror once the value-only diff has
    /// identified a difference. Carries the projected payload only (no [`Hlc`]).
    ValueUpdate((K, P)),
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone
            + DeserializeOwned
            + Hash
            + MaybeTombstone
            + PartialEq
            + Projectable
            + Reconcilable
            + Send
            + Serialize
            + Sync
            + Timestamped
            + 'static,
    > ReconcileEngine<K, V>
{
    pub async fn new(config: Config) -> Self {
        let socket = UdpSocket::bind(SocketAddr::new(config.listen_addr, config.port))
            .await
            .unwrap();
        debug!("Listening on: {}", socket.local_addr().unwrap());
        let authenticator = auth::Authenticator::new(config.cluster_key);
        if authenticator.is_enabled() {
            debug!("per-datagram MAC authentication ENABLED");
        } else {
            warn!(
                "SECURITY: no cluster key set — UDP reconciliation is UNAUTHENTICATED. Any host \
                 that can send UDP to this port can forge updates and poison the cluster via \
                 last-write-wins. Set Config::with_cluster_key on every node, or restrict the \
                 network to a trusted underlay. See issue #108 / REVIEW.md F3."
            );
        }
        let map = HRTree::<K, V>::new();
        let projection = HRTree::<K, V::Projected>::new();
        let node_id = config.node_id.unwrap_or_else(rand::random);
        ReconcileEngine {
            map: Arc::new(RwLock::new(map)),
            projection: Arc::new(RwLock::new(projection)),
            port: config.port,
            socket: Arc::new(socket),
            peer_net: config.peer_net,
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            pre_insert: Arc::new(RwLock::new(Box::new(|_, _| {}))),
            authenticator,
            members: Arc::new(RwLock::new(HashSet::new())),
            tombstone_acks: Arc::new(RwLock::new(HashMap::new())),
            clock: Arc::new(HlcClock::new(node_id)),
        }
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.map.read().hash(&range)
    }

    /// Fingerprint of the value-only [`projection`](Self::projection) over a range.
    ///
    /// This is the timestamp-less counterpart of [`fingerprint`](Self::fingerprint); a dateless
    /// mirror that has converged with this store computes the same value over the same range.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.projection.read().hash(&range)
    }

    /// Insert into the dated `map` **and** mirror the value-only projection, under a consistent
    /// lock order (`map` then `projection`) shared by every mutation path so the two trees never
    /// deadlock against each other. The caller already holds the `map` write guard.
    fn map_insert(&self, guard: &mut HRTree<K, V>, key: K, value: V) -> Option<V> {
        self.projection.write().insert(key.clone(), value.project());
        guard.insert(key, value)
    }

    /// Remove a key from the dated `map` and its value-only projection (the GC removal path).
    pub(crate) fn gc_remove(&self, key: &K) -> Option<V> {
        let mut guard = self.map.write();
        self.projection.write().remove(key);
        guard.remove(key)
    }

    /// Mint a fresh Hybrid Logical Clock timestamp for a local write.
    pub fn clock_now(&self) -> Hlc {
        self.clock.now()
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        // 1) Call the pre-insert hook *before* taking the map lock
        (self.pre_insert.read())(&key, &value);

        // 2) Now acquire write lock and insert (mirroring the value-only projection)
        let mut guard = self.map.write();
        self.map_insert(&mut guard, key, value)
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.just_insert(key.clone(), value.clone());
        let peers = self.get_peers();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        let authenticator = self.authenticator.clone();
        tokio::spawn(async move {
            let message = Message::Update::<K, V, V::Projected>((key, value));
            let messages = vec![message];
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(
                    &messages,
                    Arc::clone(&socket),
                    &authenticator,
                    &peer,
                    &mut send_buf,
                )
                .await;
            }
        });
        ret
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        // 1) First run all pre-insert hooks outside the map lock
        for (key, value) in key_values {
            (self.pre_insert.read())(key, value);
        }
        // 2) Then acquire the write lock once and insert them (mirroring the projection)
        let mut guard = self.map.write();
        for (key, value) in key_values {
            self.map_insert(&mut guard, key.clone(), value.clone());
        }
    }

    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.just_insert_bulk(key_values);
        let peers = self.get_peers();
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, V, V::Projected>(kv.clone()))
            .collect();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        let authenticator = self.authenticator.clone();
        tokio::spawn(async move {
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(
                    &messages,
                    Arc::clone(&socket),
                    &authenticator,
                    &peer,
                    &mut send_buf,
                )
                .await;
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
                    if size == recv_buf.len() {
                        warn!("Buffer too small for message, discarded");
                    } else {
                        // Authenticate the datagram *before* any deserialization. Only a cleared
                        // `Payload` can reach `handle_messages`; a missing or invalid tag is
                        // dropped silently (trace-only, to avoid attacker-driven log flooding).
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                let spoke_dated =
                                    self.handle_messages(payload, peer, &mut send_buf).await;
                                // Only record senders whose datagram was accepted. With
                                // authentication enabled this stops a spoofed/unauthenticated host
                                // from being registered as a peer (and receiving DB dumps) or as a
                                // permanent member (which would block tombstone GC forever, since
                                // causal stability requires an ack from every member). In
                                // unauthenticated mode `open` always succeeds, so this is unchanged.
                                //
                                // Crucially, a sender that spoke *only* the value-only channel is a
                                // dateless read-only mirror: it never acknowledges tombstones, so it
                                // must NOT join the #109 membership set or it would block GC
                                // forever (the very regression #128 must avoid). Such a mirror is
                                // served reactively off `peer` and is not tracked as a peer/member.
                                if spoke_dated {
                                    let addr = peer.ip();
                                    self.peers.write().insert(addr, Instant::now());
                                    self.members.write().insert(addr);
                                }
                            }
                            None => trace!("dropped datagram from {peer}: missing or invalid MAC"),
                        }
                    }
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
            Message::ComparisonItem::<K, V, V::Projected>(segment)
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
            send_to_retry(
                &self.socket,
                &self.authenticator,
                send_buf,
                (peer, self.port),
            )
            .await
            .unwrap();
        }
    }

    /// Handle the messages contained in an already-authenticated [`Payload`].
    ///
    /// Taking a [`auth::Payload`] rather than raw bytes makes it structurally impossible to
    /// process a datagram that has not cleared the authentication gate.
    ///
    /// Returns `true` if the datagram contained at least one **dated** protocol message
    /// (`ComparisonItem` / `Update` / `Ack`). The caller uses this to decide whether to register
    /// the sender in the #109 membership set: a sender that spoke only the value-only channel is a
    /// read-only mirror and must not gate tombstone GC.
    async fn handle_messages(
        &self,
        payload: auth::Payload<'_>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) -> bool {
        let payload = payload.as_bytes();
        trace!("received {} bytes from {peer}", payload.len());
        let mut in_comparison = Vec::new();
        let mut updates: Vec<(K, V)> = Vec::new();
        let mut acks: Vec<(K, u64)> = Vec::new();
        let mut value_in_comparison = Vec::new();
        let mut deserializer = Deserializer::from_slice(payload, DefaultOptions::new());
        // read messages in buffer
        loop {
            match Message::<K, V, V::Projected>::deserialize(&mut deserializer) {
                Err(ref kind) => {
                    if let bincode::ErrorKind::Io(err) = kind.as_ref() {
                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                            break;
                        }
                    }
                    // Never panic on network input: a single malformed datagram from any
                    // host would otherwise kill the receive loop (unauthenticated remote
                    // DoS). Drop the rest of the datagram and keep the loop alive, just like
                    // the oversized-datagram case above.
                    warn!("failed to deserialize datagram from {peer}, dropping it: {kind:?}");
                    break;
                }
                Ok(Message::ComparisonItem(segment)) => in_comparison.push(segment),
                Ok(Message::Update(update)) => updates.push(update),
                Ok(Message::Ack(ack)) => acks.push(ack),
                Ok(Message::ValueComparisonItem(segment)) => value_in_comparison.push(segment),
                // A dated store is authoritative and never integrates a value-only update; mirrors
                // are the only consumers of `ValueUpdate`. Ignore it defensively.
                Ok(Message::ValueUpdate(_)) => {}
            }
        }
        let spoke_dated = !in_comparison.is_empty() || !updates.is_empty() || !acks.is_empty();
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
                            reciprocal_acks
                                .push(Message::Ack::<K, V, V::Projected>((key, version)));
                        }
                    }
                }
            }
            if !reciprocal_acks.is_empty() {
                send_messages_to(
                    &reciprocal_acks,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &peer,
                    send_buf,
                )
                .await;
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
                    messages.push(Message::ComparisonItem::<K, V, V::Projected>(segment))
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
                send_messages_to(
                    &messages,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &peer,
                    send_buf,
                )
                .await;
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
                    // Advance our clock past the timestamp carried by the remote value, so a
                    // later local write is ordered after everything we have seen. This is
                    // what prevents lost updates under clock skew (issue #110).
                    self.clock.observe(remote_v.timestamp());
                    let tombstone_version = match guard.get(&k) {
                        Some(local_v) => {
                            let merged_v = local_v.reconcile(&remote_v);
                            if merged_v != *local_v {
                                (self.pre_insert.read())(&k, &merged_v);
                                let version =
                                    merged_v.is_tombstone().then(|| version_hash(&merged_v));
                                self.map_insert(&mut guard, k.clone(), merged_v);
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
                            self.map_insert(&mut guard, k.clone(), remote_v);
                            version
                        }
                    };
                    if let Some(version) = tombstone_version {
                        acks_to_send.push(Message::Ack::<K, V, V::Projected>((k, version)));
                    }
                }
            }
            if !acks_to_send.is_empty() {
                send_messages_to(
                    &acks_to_send,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &peer,
                    send_buf,
                )
                .await;
            }
        }
        // Value-only channel: answer a dateless mirror by diffing against the value-only
        // *projection* tree (never the dated map) and replying with `ValueUpdate`s carrying only
        // the projected payload. This path is entirely independent of the dated channel and of the
        // #109 causal-stability state — no acks, no membership, no GC interaction.
        if !value_in_comparison.is_empty() {
            debug!("received {} value-only segments", value_in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.projection.read();
                guard.diff_round(value_in_comparison, &mut out_comparison, &mut differences);
            }
            let mut messages = Vec::new();
            for segment in out_comparison {
                messages.push(Message::ValueComparisonItem::<K, V, V::Projected>(segment));
            }
            if !differences.is_empty() {
                let guard = self.projection.read();
                for range in differences {
                    for (k, p) in guard.get_range(&range) {
                        messages.push(Message::ValueUpdate((k.clone(), p.clone())));
                    }
                }
            }
            if !messages.is_empty() {
                send_messages_to(
                    &messages,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &peer,
                    send_buf,
                )
                .await;
            }
        }
        spoke_dated
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

pub(crate) async fn send_to_retry<A: ToSocketAddrs>(
    socket: &UdpSocket,
    authenticator: &auth::Authenticator,
    buf: &[u8],
    target: A,
) -> std::io::Result<usize> {
    // Frame the datagram once and reuse it across retries. When authentication is disabled,
    // `seal` returns `None` and the wire bytes are identical to the unauthenticated behavior.
    let framed = authenticator.seal(buf);
    let wire: &[u8] = framed.as_deref().unwrap_or(buf);
    let mut res = Ok(0);
    for _ in 0..MAX_SENDTO_RETRIES {
        res = socket.send_to(wire, &target).await;
        if res.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    res
}

pub(crate) async fn send_messages_to<K: Serialize, V: Serialize, P: Serialize>(
    messages: &[Message<K, V, P>],
    socket: Arc<UdpSocket>,
    authenticator: &auth::Authenticator,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
) {
    debug!("sending {} messages to {peer}", messages.len());
    // Reserve room for the authentication tag so the sealed datagram still fits a UDP payload.
    let max_payload = BUFFER_SIZE - authenticator.overhead();
    send_buf.clear();
    for message in messages {
        let last_size = send_buf.len();
        message
            .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
            .unwrap();
        if send_buf.len() > max_payload {
            trace!("sending {} bytes to {peer}", last_size);
            send_to_retry(&socket, authenticator, &send_buf[..last_size], &peer)
                .await
                .unwrap();
            trace!("sent {} bytes to {peer}", last_size);
            send_buf.drain(..last_size);
        }
    }
    trace!("sending last {} bytes to {peer}", send_buf.len());
    send_to_retry(&socket, authenticator, send_buf, &peer)
        .await
        .unwrap();
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
mod auth_attack {
    use std::time::Duration;

    use bincode::{DefaultOptions, Serializer};
    use serde::Serialize;
    use tokio::net::UdpSocket;

    use super::Message;
    use crate::hlc::Hlc;
    use crate::{auth, reconcile_store::Config, ReconcileStore};

    /// Serialize the F3 attack payload: an `Update` with a far-future timestamp that, if merged,
    /// would win against every legitimate write forever.
    fn forged_update() -> Vec<u8> {
        let far_future = Hlc::new(u64::MAX, 0, 0);
        let message =
            Message::Update::<i32, (Hlc, Option<String>), crate::reconcilable::ValueOnly<String>>(
                (0, (far_future, Some("evil".to_string()))),
            );
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
            .with_cluster_key(key);
        let store = ReconcileStore::<i32, String>::new(config).await;
        store.just_insert(0, "legit".to_string());
        let task = tokio::spawn(store.clone().run());

        let attacker = UdpSocket::bind("127.0.0.49:0").await.unwrap();
        let target = format!("{victim_addr}:{port}");
        let forged = forged_update();

        // (a) forged update sent WITHOUT any authentication tag
        attacker.send_to(&forged, &target).await.unwrap();
        // (b) forged update sealed with the WRONG key
        let wrong_key_sealed = auth::Authenticator::new(Some([0x99u8; auth::KEY_LEN]))
            .seal(&forged)
            .expect("enabled");
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

    use crate::hlc::Hlc;
    use crate::reconcile_engine::{version_hash, ReconcileEngine};
    use crate::reconcile_store::Config;

    type Tombstoned = (Hlc, Option<i32>);

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
        let tombstone: Tombstoned = (Hlc::new(1, 0, 0), None);
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
        let tombstone: Tombstoned = (Hlc::new(1, 0, 0), None);
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
