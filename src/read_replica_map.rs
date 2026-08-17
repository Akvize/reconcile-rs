// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! [`ReadReplicaMap`]: a lightweight, **dateless, read-only replica** of a dated
//! [`ReplicatedMap`](crate::ReplicatedMap), storing [`State<V>`] alone and saving the ~12–16
//! bytes per entry a passive consumer never needs.
//!
//! It converges over the existing range-diff protocol on the same port, speaking only the
//! **value-only channel** — which a dated peer answers from its timestamp-less projection tree, so
//! the dated↔dated path is untouched. It never acknowledges tombstones and is never added to a
//! dated peer's membership set, so it cannot block tombstone GC.
//!
//! A sink, not a source: it always integrates inbound updates by plain overwrite, holding no
//! timestamp to compare, and never pushes a value back.
//!
//! **Limitation.** It reflects a deletion only if it sees the tombstone before the dated store
//! collects it, and it keeps its own entry for a key after that — so a GC configured to outrun
//! replica propagation leaves `get` returning a stale value.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use parking_lot::{RwLock, RwLockReadGuard};
use rand::rngs::StdRng;
use rand::SeedableRng;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::entry::{Entry, State};
use crate::replica::{
    send_messages_to, send_to_retry, Message, PeerCap, SendPorts, MAX_MESSAGES_PER_DATAGRAM,
};
use crate::replicated_map::Config;
use crate::transport::{Transport, UdpTransport};
use crate::value_ref::ValueRef;
use crate::FingerprintTreeMap;
use gossip::auth;
use gossip::gen_ip::{gen_ip, net_of};
use gossip::replay;
use rsos::Fingerprint;

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);
const PEER_EXPIRATION: Duration = Duration::from_secs(60);

type OnUpdateCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &State<V>)>;

/// The wire value type, named only so the shared [`Message`] enum has a concrete `Update` payload
/// — which a read replica ignores, storing no dated value.
type WireDated<V> = Entry<Timestamp, V>;

/// A lightweight, dateless, read-only replica of a dated [`ReplicatedMap`](crate::ReplicatedMap);
/// see the [module documentation](crate::read_replica_map).
///
/// **Correct only under last-write-wins**: overwrite-on-arrival is right only because the dated
/// peer already resolved the conflict before sending the projection.
pub struct ReadReplicaMap<K, V> {
    /// The value-only tree mirroring the dated store. Its range fingerprints are timestamp-less by
    /// construction (see [`State`]), matching a dated peer's value-only projection.
    tree: Arc<RwLock<FingerprintTreeMap<K, State<V>>>>,
    port: u16,
    /// The datagram-I/O port (default adapter: [`UdpTransport`]), shared with every clone. A read
    /// replica sends and receives exclusively through it, exactly like
    /// [`Replica`](crate::Replica) — no `tokio::net` call sites of its own.
    transport: Arc<dyn Transport<Addr = SocketAddr>>,
    /// The single network this replica probes: the one containing its listen address, else the
    /// first declared, else loopback. Retunable via [`set_net`](Self::set_net).
    net: Arc<RwLock<IpNet>>,
    rng: Arc<RwLock<StdRng>>,
    peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    authenticator: auth::Authenticator,
    sender_counter: Arc<replay::SenderCounter>,
    replay_filter: Arc<replay::ReplayFilter>,
    /// Invoked just before each inbound value is integrated, so callers can be notified of changes.
    on_update: Arc<RwLock<OnUpdateCallback<K, V>>>,
    /// Hard cap on the number of dated-cluster peers the read replica tracks. Datagrams from unknown
    /// senders are dropped before any per-sender state is allocated when the peers map reaches
    /// this size. Sourced from [`Config::max_peers`].
    max_peers: PeerCap,
}

impl<K, V> Clone for ReadReplicaMap<K, V> {
    fn clone(&self) -> Self {
        ReadReplicaMap {
            tree: self.tree.clone(),
            port: self.port,
            transport: self.transport.clone(),
            net: self.net.clone(),
            rng: self.rng.clone(),
            peers: self.peers.clone(),
            authenticator: self.authenticator.clone(),
            sender_counter: self.sender_counter.clone(),
            replay_filter: self.replay_filter.clone(),
            on_update: self.on_update.clone(),
            max_peers: self.max_peers,
        }
    }
}

/// #293/#294: `ReadReplicaMap` mints no timestamps and runs no bulk-transfer/cross-net-throttle
/// machinery, so several `Config` fields that matter to a dated [`ReplicatedMap`] have no effect
/// here. A non-default value silently doing nothing is exactly the trap #293 exists to close —
/// warn once, at construction, rather than leave it silent.
fn warn_on_ignored_config_fields(config: &Config) {
    let default = Config::default();
    let mut ignored = Vec::new();
    if config.remote_interval != default.remote_interval {
        ignored.push("remote_interval");
    }
    if config.remote_fanout != default.remote_fanout {
        ignored.push("remote_fanout");
    }
    if config.node_id != default.node_id {
        ignored.push("node_id");
    }
    if config.bulk_send_rate != default.bulk_send_rate {
        ignored.push("bulk_send_rate");
    }
    if config.recv_buffer_size != default.recv_buffer_size {
        ignored.push("recv_buffer_size");
    }
    if config.send_buffer_size != default.send_buffer_size {
        ignored.push("send_buffer_size");
    }
    if config.max_concurrent_bulk_dumps != default.max_concurrent_bulk_dumps {
        ignored.push("max_concurrent_bulk_dumps");
    }
    if config.nets.iter().flatten().count() > 1 {
        ignored.push("nets (only the one matching listen_addr, or the first declared, is used)");
    }
    if !ignored.is_empty() {
        warn!(
            "ReadReplicaMap ignores these Config fields, set here to a non-default value: {}. \
             See #293/#294.",
            ignored.join(", ")
        );
    }
}

impl<K: Key, V: Value> ReadReplicaMap<K, V> {
    /// Create a read replica bound to the configured UDP socket.
    ///
    /// Honours the same [`Config`] as a dated store — including
    /// [`with_cluster_key`](Config::with_cluster_key), which an authenticated cluster requires.
    /// `node_id` is ignored: a read replica mints no timestamps.
    ///
    /// # Errors
    ///
    /// If the socket cannot be bound to `(config.listen_addr, config.port)`, or if
    /// `config.port == 0` — see [`Config::port`]'s docs.
    ///
    /// # Panics
    ///
    /// If `config.cluster_key` is `None` without also setting
    /// [`Config::with_insecure_no_key`] — see #325.
    pub async fn new(config: Config) -> io::Result<Self> {
        crate::replica::check_port_is_nonzero(&config)?;
        // The read replica keeps the OS default socket buffer sizes (`None`/`None`) rather than
        // reading `Config::recv_buffer_size`/`send_buffer_size`: it never bound a tuned socket, and
        // a port refactor is not the place to change how much kernel memory it claims.
        let transport =
            UdpTransport::bind(SocketAddr::new(config.listen_addr, config.port), None, None)
                .await?;
        debug!("ReadReplicaMap listening on: {}", transport.local_addr()?);
        Ok(Self::build(config, Arc::new(transport)))
    }

    /// Create a read replica over a caller-supplied [`Transport`], mirroring
    /// [`ReplicatedMap::new_with_transport`](crate::ReplicatedMap::new_with_transport) — which is
    /// what lets one be driven against a dated peer with no real sockets.
    ///
    /// The caller has already done the one fallible I/O step, binding.
    ///
    /// # Panics
    ///
    /// If `config.cluster_key` is `None` without also setting
    /// [`Config::with_insecure_no_key`] — see #325.
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use reconcile::{replicated_map::Config, InMemoryNetwork, ReadReplicaMap};
    /// let network = InMemoryNetwork::new();
    /// let transport = Arc::new(network.bind("127.0.0.1:8080".parse().unwrap()));
    /// let read_replica = ReadReplicaMap::<String, String>::new_with_transport(
    ///     Config::default().with_insecure_no_key(),
    ///     transport,
    /// );
    /// ```
    pub fn new_with_transport(
        config: Config,
        transport: Arc<dyn Transport<Addr = SocketAddr>>,
    ) -> Self {
        Self::build(config, transport)
    }

    /// Assemble a read replica from an already-constructed [`Transport`]. Pure wiring — no I/O.
    /// Panics rather than being fallible; see [`new_with_transport`](Self::new_with_transport).
    /// The fallible socket bind lives in [`new`](Self::new).
    fn build(config: Config, transport: Arc<dyn Transport<Addr = SocketAddr>>) -> Self {
        config.check_key_or_insecure_opt_in();
        warn_on_ignored_config_fields(&config);
        let authenticator = auth::Authenticator::new(config.cluster_key, config.encrypt);
        if matches!(authenticator, auth::Authenticator::Disabled) {
            warn!(
                "SECURITY: running with Config::with_insecure_no_key() — the lightweight read \
                 replica accepts UNAUTHENTICATED datagrams. Set Config::with_cluster_key to \
                 match the dated cluster."
            );
        }
        let authenticator_enabled = !matches!(authenticator, auth::Authenticator::Disabled);
        // A read replica tracks a single network: the one containing its listen address, else the
        // first declared network, else the historical loopback default.
        let nets: Vec<IpNet> = config.nets.iter().flatten().copied().collect();
        let net = net_of(&nets, config.listen_addr)
            .or_else(|| nets.first().copied())
            .unwrap_or_else(|| "127.0.0.1/8".parse().unwrap());
        ReadReplicaMap {
            tree: Arc::new(RwLock::new(FingerprintTreeMap::<K, State<V>>::new())),
            port: config.port,
            transport,
            net: Arc::new(RwLock::new(net)),
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            authenticator,
            sender_counter: Arc::new(replay::SenderCounter::new()),
            replay_filter: Arc::new(replay::ReplayFilter::new(
                config.freshness_window,
                authenticator_enabled,
            )),
            on_update: Arc::new(RwLock::new(Box::new(|_, _| {}))),
            max_peers: PeerCap::new(config.max_peers),
        }
    }

    /// Provide the address of a known dated peer, reducing the time to first sync.
    pub fn with_seed(self, peer: IpAddr) -> Self {
        self.peers.write().insert(peer, Instant::now());
        self
    }

    /// (runtime) Retune the network this read replica probes for discovery, visible to all clones.
    pub fn set_net(&self, net: IpNet) {
        *self.net.write() = net;
    }

    /// The network this read replica currently probes for discovery.
    pub fn net(&self) -> IpNet {
        *self.net.read()
    }

    /// Register a hook invoked outside the map lock, before each inbound value is integrated. A
    /// deletion arrives as `State::Tombstone`.
    pub fn add_on_update<F: Send + Sync + Fn(&K, &State<V>) + 'static>(&self, on_update: F) {
        *self.on_update.write() = Box::new(on_update);
    }

    /// Get the live value for a key, or `None` if the key is absent or holds a replicated tombstone.
    pub fn get(&self, k: &K) -> Option<ValueRef<'_, V>> {
        let guard = self.tree.read();
        RwLockReadGuard::try_map(guard, |tree| tree.get(k).and_then(|state| state.as_value()))
            .ok()
            .map(ValueRef)
    }

    /// Clone of the live value for `k`, or `None`. Unlike [`get`](Self::get), the read lock is
    /// released before this returns — the default read when the value will be compared against or
    /// fed into a subsequent write, mirroring [`ReplicatedMap::get_cloned`](crate::ReplicatedMap::get_cloned).
    pub fn get_cloned(&self, k: &K) -> Option<V> {
        self.get(k).map(|v| v.clone())
    }

    /// Whether the read replica currently holds a live value for the key (a tombstone counts as
    /// absent).
    pub fn contains_key(&self, k: &K) -> bool {
        self.tree
            .read()
            .get(k)
            .is_some_and(|state| !state.is_tombstone())
    }

    /// The number of **live** entries currently held (a replicated tombstone counts as absent, not
    /// present). Mirrors [`ReplicatedMap::len`](crate::ReplicatedMap::len): `O(n)`, it scans the
    /// tree filtering out tombstones.
    pub fn len(&self) -> usize {
        self.tree
            .read()
            .iter()
            .filter(|(_, state)| !state.is_tombstone())
            .count()
    }

    /// Whether the read replica holds no live entry (a tree holding only tombstones is empty).
    /// `O(n)` worst case, but returns as soon as it finds a live value.
    pub fn is_empty(&self) -> bool {
        !self
            .tree
            .read()
            .iter()
            .any(|(_, state)| !state.is_tombstone())
    }

    /// Value-only fingerprint over a range. After convergence this equals the dated peer's
    /// [`value_fingerprint`](crate::ReplicatedMap::value_fingerprint) over the same range.
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.tree.read().aggregate(range).fingerprint()
    }

    /// The smallest live key and its value, or `None` if the read replica holds no live entry.
    /// Same complexity as [`ReplicatedMap::first_key_value`](crate::ReplicatedMap::first_key_value).
    pub fn first_key_value(&self) -> Option<(K, V)> {
        let guard = self.tree.read();
        guard
            .iter()
            .find(|(_, state)| !state.is_tombstone())
            .map(|(k, state)| (k.clone(), state.as_value().expect("checked above").clone()))
    }

    /// The largest live key and its value, or `None` if the read replica holds no live entry. Same
    /// complexity as [`ReplicatedMap::last_key_value`](crate::ReplicatedMap::last_key_value).
    pub fn last_key_value(&self) -> Option<(K, V)> {
        let guard = self.tree.read();
        let mut index = guard.len();
        while index > 0 {
            index -= 1;
            let key = guard.select(index).clone();
            if let Some(value) = guard.get(&key).and_then(|state| state.as_value()) {
                return Some((key, value.clone()));
            }
        }
        None
    }

    /// Call `f` for every live entry, in key order, under the tree read lock. Do not block or call
    /// back into the replica from `f`.
    pub fn for_each<F: FnMut(&K, &V)>(&self, mut f: F) {
        let guard = self.tree.read();
        for (k, state) in guard.iter() {
            if let Some(value) = state.as_value() {
                f(k, value);
            }
        }
    }

    /// Call `f` for every live entry whose key falls in `range`, in key order. Mirrors the
    /// [`fingerprint`](Self::fingerprint) range signature; same locking discipline as
    /// [`for_each`](Self::for_each).
    pub fn for_each_in_range<R: RangeBounds<K>, F: FnMut(&K, &V)>(&self, range: R, mut f: F) {
        let guard = self.tree.read();
        for (k, state) in guard.range(range) {
            if let Some(value) = state.as_value() {
                f(k, value);
            }
        }
    }

    /// Snapshot all live entries into an owned `Vec`, in key order. Clones under the read lock;
    /// prefer [`for_each`](Self::for_each) to avoid the copy for large scans.
    pub fn to_vec(&self) -> Vec<(K, V)> {
        let guard = self.tree.read();
        guard
            .iter()
            .filter_map(|(k, state)| state.as_value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// Snapshot the live entries whose keys fall in `range` into an owned `Vec`, in key order.
    pub fn range_to_vec<R: RangeBounds<K>>(&self, range: R) -> Vec<(K, V)> {
        let guard = self.tree.read();
        guard
            .range(range)
            .filter_map(|(k, state)| state.as_value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// The keys of all live entries, in key order. Thin owned convenience over [`to_vec`](Self::to_vec).
    pub fn keys(&self) -> Vec<K> {
        let guard = self.tree.read();
        guard
            .iter()
            .filter_map(|(k, state)| state.as_value().map(|_| k.clone()))
            .collect()
    }

    /// The values of all live entries, in key order.
    pub fn values(&self) -> Vec<V> {
        let guard = self.tree.read();
        guard
            .iter()
            .filter_map(|(_, state)| state.as_value().cloned())
            .collect()
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    /// Integrate inbound value-only updates by plain overwrite (a read replica holds no timestamp to
    /// compare against — it trusts the authoritative dated peer). Hooks run outside the map lock,
    /// so a hook may safely call back into the read replica.
    fn integrate(&self, updates: Vec<(K, State<V>)>) {
        if updates.is_empty() {
            return;
        }
        {
            let hook = self.on_update.read();
            for (k, state) in &updates {
                hook(k, state);
            }
        }
        let mut guard = self.tree.write();
        for (k, state) in updates {
            guard.insert(k, state);
        }
    }

    /// Bundle the outbound ports the batched-send helpers need, exactly as
    /// [`Replica`](crate::Replica) does. See [`SendPorts`].
    fn send_ports(&self) -> SendPorts<'_, dyn Transport<Addr = SocketAddr>> {
        SendPorts {
            transport: &*self.transport,
            authenticator: &self.authenticator,
            sender_counter: &self.sender_counter,
        }
    }

    /// Send our value-only comparison items to every known peer plus a random address (discovery),
    /// kicking off / continuing a value-only reconciliation round.
    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let segments = rbsr::initial_ranges(&*self.tree.read());
        send_buf.clear();
        for segment in segments {
            gossip::bincode::encode(
                &Message::ValueComparisonItem::<K, WireDated<V>, State<V>>(segment),
                send_buf,
            )
            .unwrap();
        }
        let mut peers = self.get_peers();
        // A random address out of the peer network, for discovery — like the dated store, we do not
        // add it to the known peers; a real peer there will answer and be recorded then.
        let net = *self.net.read();
        let addr = gen_ip(&mut *self.rng.write(), net);
        peers.push(addr);
        for peer in peers {
            trace!(
                "read replica initial_ranges {} bytes to {peer}",
                send_buf.len()
            );
            if let Err(err) = send_to_retry(
                &*self.transport,
                &self.authenticator,
                &self.sender_counter,
                send_buf,
                SocketAddr::new(peer, self.port),
            )
            .await
            {
                warn!(
                    "read replica failed to send reconciliation initiation to {peer}: {err}; \
                     continuing"
                );
            }
        }
    }

    async fn handle_messages(
        &self,
        payload: auth::Payload<'_, auth::Verified>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) {
        let payload = payload.as_bytes();
        trace!("read replica received {} bytes from {peer}", payload.len());
        let mut value_in_comparison = Vec::new();
        let mut value_updates: Vec<(K, State<V>)> = Vec::new();
        // `MAX_MESSAGES_PER_DATAGRAM` bounds the expansion; a malformed datagram is dropped whole.
        let messages: Vec<Message<K, WireDated<V>, State<V>>> =
            match gossip::bincode::decode_stream(payload, MAX_MESSAGES_PER_DATAGRAM) {
                Ok(messages) => messages,
                Err(kind) => {
                    warn!(
                        "read replica failed to deserialize datagram from {peer}, dropping it: \
                         {kind:?}"
                    );
                    return;
                }
            };
        for message in messages {
            match message {
                Message::ValueComparisonItem(segment) => value_in_comparison.push(segment),
                Message::ValueUpdate(update) => value_updates.push(update),
                // The dated channel is meaningless to a read replica (it cannot store dated values
                // nor participate in causal stability). Ignore it.
                Message::ComparisonItem(_) | Message::Update(_) | Message::Ack(_) => {}
            }
        }

        self.integrate(value_updates);

        if !value_in_comparison.is_empty() {
            debug!(
                "read replica received {} value-only segments",
                value_in_comparison.len()
            );
            let mut out_comparison = Vec::new();
            let mut differences = Vec::new();
            {
                let guard = self.tree.read();
                rbsr::protocol_round(
                    &*guard,
                    value_in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // `differences` are ranges this read replica would owe the peer. A read-only replica
            // never sends authoritative values, so we deliberately drop them and only bounce back
            // the refined comparison items that keep the peer's side of the diff progressing.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::<K, WireDated<V>, State<V>>::ValueComparisonItem)
                    .collect();
                send_messages_to(&messages, &self.send_ports(), &peer, send_buf).await;
            }
        }
    }

    /// Run the read replica's reconciliation loop forever. Spawn this on a task; the read replica
    /// converges to the dated cluster's current values and reflects deletions as tombstones.
    pub async fn run(self) {
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        self.start_reconciliation(&mut send_buf).await;
        loop {
            match timeout(ACTIVITY_TIMEOUT, self.transport.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    debug!("read replica: no recent activity; initiating value-only diff");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => warn!("read replica network error in recv_from: {err}"),
                Ok(Ok((size, peer))) => {
                    if peer.port() != self.port {
                        warn!(
                            "read replica received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    if size == recv_buf.len() {
                        warn!("read replica buffer too small for message, discarded");
                    } else {
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                // Reject a differently-versioned peer distinguishably from an
                                // authentication failure — see `Replica::run`'s identical gate.
                                let payload = match payload.check_version() {
                                    Ok(payload) => payload,
                                    Err(version) => {
                                        trace!(
                                            "read replica dropped datagram from {peer}: wire \
                                             version {version} != {}",
                                            auth::WIRE_VERSION
                                        );
                                        continue;
                                    }
                                };
                                let sender = peer.ip();
                                // Per-peer cap check: drop datagrams from unknown senders when the
                                // peers map is at capacity, before any per-sender state is
                                // allocated (peers slot or replay-filter entry).
                                {
                                    let guard = self.peers.read();
                                    let (known, current_len) =
                                        (guard.contains_key(&sender), guard.len());
                                    if !self.max_peers.admits(known, current_len) {
                                        trace!(
                                            "read replica dropped datagram from {peer}: peer cap \
                                             reached ({current_len}/{})",
                                            self.max_peers.max()
                                        );
                                        continue;
                                    }
                                }
                                let (seq, stamp) = (payload.seq, payload.stamp);
                                let Some(payload) =
                                    payload.verify_replay(&self.replay_filter, sender)
                                else {
                                    trace!(
                                        "read replica dropped replayed datagram from {peer}: \
                                         seq={seq} stamp={stamp}"
                                    );
                                    continue;
                                };
                                self.handle_messages(payload, peer, &mut send_buf).await;
                                // Record the sender so we keep gossiping value-only diffs to it.
                                self.peers.write().insert(sender, Instant::now());
                            }
                            None => trace!(
                                "read replica dropped datagram from {peer}: missing or invalid MAC"
                            ),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replicated_map::Config;

    fn ephemeral_config() -> Config {
        // A fresh port per call (#293: Config::port must be nonzero) on the loopback default
        // network.
        Config::default()
            .with_port(crate::replica::next_ephemeral_test_port())
            .with_insecure_no_key()
    }

    /// `get` returns the live value, and absent keys are `None`.
    #[tokio::test]
    async fn get_returns_integrated_value() {
        let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
            .await
            .expect("bind failed");
        assert!(read_replica.get(&1).is_none());
        read_replica.integrate(vec![(1, State::Present("hello".to_string()))]);
        assert_eq!(read_replica.get(&1).as_deref(), Some(&"hello".to_string()));
        assert!(read_replica.contains_key(&1));
        assert_eq!(read_replica.len(), 1);
    }

    /// A replicated tombstone (`State::Tombstone`) hides the value but is still a stored entry.
    #[tokio::test]
    async fn replicates_tombstones() {
        let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
            .await
            .expect("bind failed");
        read_replica.integrate(vec![(1, State::Present("v".to_string()))]);
        assert_eq!(read_replica.get(&1).as_deref(), Some(&"v".to_string()));

        // A later tombstone overwrites it: the value disappears from `get`, and it no longer counts
        // as a live entry (a read replica has no timestamp and trusts the authoritative peer).
        read_replica.integrate(vec![(1, State::Tombstone)]);
        assert!(read_replica.get(&1).is_none());
        assert!(!read_replica.contains_key(&1));
        assert_eq!(read_replica.len(), 0, "the tombstone is not a live entry");
        // The tombstone itself is still retained internally (the tree keeps it until the dated peer
        // observes it acknowledged and moves on) — `len` deliberately doesn't surface that raw size.
        assert_eq!(
            read_replica.tree.read().len(),
            1,
            "the tombstone is retained as a tree entry"
        );
    }

    /// The collection-shaped read API (`for_each`/`for_each_in_range`/`to_vec`/`range_to_vec`/
    /// `keys`/`values`) mirrors [`ReplicatedMap`](crate::ReplicatedMap)'s: live entries only, in key
    /// order, tombstones excluded.
    #[tokio::test]
    async fn collection_reads_exclude_tombstones() {
        let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
            .await
            .expect("bind failed");
        read_replica.integrate(vec![
            (1, State::Present(10)),
            (2, State::Present(20)),
            (3, State::Tombstone),
            (4, State::Present(40)),
        ]);

        assert_eq!(read_replica.to_vec(), vec![(1, 10), (2, 20), (4, 40)]);
        assert_eq!(read_replica.keys(), vec![1, 2, 4]);
        assert_eq!(read_replica.values(), vec![10, 20, 40]);
        assert_eq!(read_replica.range_to_vec(2..=3), vec![(2, 20)]);
        assert!(read_replica.range_to_vec(3..3).is_empty());

        let mut collected = Vec::new();
        read_replica.for_each(|k, v| collected.push((*k, *v)));
        assert_eq!(collected, read_replica.to_vec());

        let mut in_range = Vec::new();
        read_replica.for_each_in_range(2.., |k, v| in_range.push((*k, *v)));
        assert_eq!(in_range, vec![(2, 20), (4, 40)]);
    }

    /// `first_key_value`/`last_key_value` skip a tombstone sitting at the extremal raw key,
    /// mirroring [`ReplicatedMap`](crate::ReplicatedMap)'s.
    #[tokio::test]
    async fn first_and_last_key_value_skip_boundary_tombstones() {
        let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
            .await
            .expect("bind failed");
        assert_eq!(read_replica.first_key_value(), None);
        assert_eq!(read_replica.last_key_value(), None);

        read_replica.integrate(vec![
            (1, State::Tombstone),
            (2, State::Present(20)),
            (3, State::Present(30)),
            (4, State::Present(40)),
            (5, State::Tombstone),
        ]);
        assert_eq!(read_replica.first_key_value(), Some((2, 20)));
        assert_eq!(read_replica.last_key_value(), Some((4, 40)));

        read_replica.integrate(vec![(2, State::Tombstone), (4, State::Tombstone)]);
        assert_eq!(read_replica.first_key_value(), Some((3, 30)));
        assert_eq!(read_replica.last_key_value(), Some((3, 30)));

        read_replica.integrate(vec![(3, State::Tombstone)]);
        assert_eq!(read_replica.first_key_value(), None);
        assert_eq!(read_replica.last_key_value(), None);
    }

    /// The on-update hook fires for every integrated value, including tombstones.
    #[tokio::test]
    async fn on_update_hook_fires() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let read_replica = ReadReplicaMap::<i32, i32>::new(ephemeral_config())
            .await
            .expect("bind failed");
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        read_replica.add_on_update(move |_, _| {
            count2.fetch_add(1, Ordering::SeqCst);
        });
        read_replica.integrate(vec![(1, State::Present(10)), (2, State::Tombstone)]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// The read replica's value-only fingerprint matches an independently-built tree of the same
    /// logical content — i.e. timestamps genuinely play no part in the hash.
    #[tokio::test]
    async fn value_fingerprint_is_timestamp_independent() {
        let read_replica = ReadReplicaMap::<i32, String>::new(ephemeral_config())
            .await
            .expect("bind failed");
        read_replica.integrate(vec![
            (1, State::Present("a".to_string())),
            (2, State::Tombstone),
        ]);

        let mut reference: FingerprintTreeMap<i32, State<String>> = FingerprintTreeMap::new();
        reference.insert(1, State::Present("a".to_string()));
        reference.insert(2, State::Tombstone);

        assert_eq!(
            read_replica.fingerprint(..),
            reference.aggregate(..).fingerprint()
        );
    }

    /// A live value and its `State` projection hash identically only via the value-only basis:
    /// per-entry, the dateless read replica saves the whole `Timestamp` (the point of the dateless
    /// read replica).
    #[test]
    fn value_only_is_smaller_per_entry() {
        let dated = std::mem::size_of::<Entry<Timestamp, u64>>();
        let light = std::mem::size_of::<State<u64>>();
        assert!(
            light < dated,
            "value-only entry ({light} B) should be smaller than dated entry ({dated} B)"
        );
    }
}
