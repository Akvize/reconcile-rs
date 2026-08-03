// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReconcileMirror`], a lightweight, **dateless, read-only mirror** of a dated
//! [`ReconcileStore`](crate::ReconcileStore).
//!
//! # What it is for
//!
//! A dated store keeps a [`Timestamp`] next to every value so it can resolve conflicts
//! (last-write-wins) and run the tombstone causal-stability machinery. For a fleet with many
//! *passive read replicas* that only ever consume values, that timestamp is pure overhead: ~12–16
//! bytes per entry that the replica never needs. A `ReconcileMirror` stores only
//! [`State<V>`] — the `Option<V>` payload, no timestamp — and still converges with a dated peer
//! over the **existing range-based diff protocol**, on the same UDP port.
//!
//! # How it stays causal-stability-safe
//!
//! The mirror speaks only the **value-only channel** of the protocol (the `ValueComparisonItem` /
//! `ValueUpdate` messages). A
//! dated peer answers those by diffing against its value-only *projection* tree, so the mirror never
//! sees a timestamp and the dated↔dated path is untouched. Crucially, the mirror **never
//! acknowledges tombstones and is never added to a dated peer's causal-stability membership set**, so it cannot
//! block tombstone garbage collection — the regression the naive "single value-only hash" design
//! would have caused.
//!
//! # Read-only
//!
//! The mirror **always integrates** inbound updates (plain overwrite — it holds no timestamp to
//! compare) and **never sends authoritative values**: it drives the diff by exchanging comparison
//! items but ignores any difference it is asked to push back. It is a sink, not a source.
//!
//! # Limitations
//!
//! - A mirror reflects a deletion as a tombstone only if it observes the tombstone before the dated
//!   store **garbage-collects** it. With the default tombstone timeout (60 s) a mirror polling on
//!   the order of a second has ample time; but if GC is configured to outrun mirror propagation,
//!   the mirror may retain the pre-deletion value. Likewise, once a dated store GC's a tombstone the
//!   mirror keeps its own value-only entry for that key (it never pushes data back), so `get` may
//!   still return the stale value. Both are consequences of the mirror being a passive, read-only
//!   replica with no causal-stability bookkeeping of its own.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::auth;
use crate::bounds::{Key, Value};
use crate::clock::Timestamp;
use crate::codec::BincodeCodec;
use crate::fingerprint::Fingerprint;
use crate::gen_ip::{gen_ip, net_of};
use crate::proto;
use crate::reconcilable::{Entry, State};
use crate::reconcile_engine::{send_messages_to, send_to_retry, Message, SendPorts};
use crate::reconcile_store::Config;
use crate::replay;
use crate::transport::UdpTransport;
use crate::HRTree;

const BUFFER_SIZE: usize = 65507;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(1);
const PEER_EXPIRATION: Duration = Duration::from_secs(60);

type OnUpdateCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &State<V>)>;

/// The wire value type a mirror names for (de)serialization. The mirror never stores a dated value;
/// it only needs the type so the shared [`Message`] enum has a concrete `Update` payload (which it
/// ignores) and so the value-only projection type resolves to
/// [`State<V>`](crate::reconcilable::State).
type WireDated<V> = Entry<Timestamp, V>;

/// A lightweight, dateless, read-only mirror of a dated [`ReconcileStore`](crate::ReconcileStore).
///
/// See the [module documentation](crate::mirror) for the design and the causal-stability-safety guarantees.
///
/// # Correct only under last-write-wins
///
/// A mirror stores no timestamps, so it cannot resolve a conflict: inbound updates are applied by
/// plain overwrite. That is correct *only* because the authoritative dated peer already
/// resolved the conflict under LWW before sending the projection. Under any other resolution
/// policy, last-writer-by-arrival would be wrong and the mirror would need redesigning — see
/// `ARCHITECTURE.md` §7 D9, and D6 for why the policy is fixed.
pub struct ReconcileMirror<K, V> {
    /// The value-only mirror. Its range fingerprints are timestamp-less by construction (see
    /// [`State`](crate::reconcilable::State)), matching a dated peer's value-only projection.
    tree: Arc<RwLock<HRTree<K, State<V>>>>,
    port: u16,
    socket: Arc<UdpSocket>,
    /// The single network this read-only mirror probes for discovery. A mirror is a
    /// dateless sink, usually seeded onto a dated cluster, so it tracks just one network: the one
    /// containing its listen address, else the first declared network, else the loopback default.
    /// Shared so it can be retuned at runtime (see [`set_net`](Self::set_net)).
    net: Arc<RwLock<IpNet>>,
    rng: Arc<RwLock<StdRng>>,
    peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    authenticator: auth::Authenticator,
    sender_counter: Arc<replay::SenderCounter>,
    replay_filter: Arc<replay::ReplayFilter>,
    /// Invoked just before each inbound value is integrated, so callers can be notified of changes.
    on_update: Arc<RwLock<OnUpdateCallback<K, V>>>,
    /// Hard cap on the number of dated-cluster peers the mirror tracks. Datagrams from unknown
    /// senders are dropped before any per-sender state is allocated when the peers map reaches
    /// this size. Sourced from [`Config::max_peers`].
    max_peers: usize,
}

impl<K, V> Clone for ReconcileMirror<K, V> {
    fn clone(&self) -> Self {
        ReconcileMirror {
            tree: self.tree.clone(),
            port: self.port,
            socket: self.socket.clone(),
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

impl<K: Key, V: Value> ReconcileMirror<K, V> {
    /// Create a new mirror bound to the configured UDP socket.
    ///
    /// The mirror honours the same [`Config`] as a dated store, including
    /// [`with_cluster_key`](Config::with_cluster_key): to mirror an authenticated cluster it must
    /// share the cluster key. The Hybrid-Logical-Clock `node_id` is ignored (a mirror mints no
    /// timestamps).
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the UDP socket cannot be bound to
    /// `(config.listen_addr, config.port)` — for example, because the port is already in use or
    /// the address is not available on this host.
    pub async fn new(config: Config) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::new(config.listen_addr, config.port)).await?;
        debug!("ReconcileMirror listening on: {}", socket.local_addr()?);
        let authenticator = auth::Authenticator::new(config.cluster_key, config.encrypt);
        if !authenticator.is_enabled() {
            warn!(
                "SECURITY: no cluster key set — the lightweight mirror accepts UNAUTHENTICATED \
                 datagrams. Set Config::with_cluster_key to match the dated cluster."
            );
        }
        // A mirror tracks a single network: the one containing its listen address, else the first
        // declared network, else the historical loopback default.
        let nets: Vec<IpNet> = config.nets.iter().flatten().copied().collect();
        let net = net_of(&nets, config.listen_addr)
            .or_else(|| nets.first().copied())
            .unwrap_or_else(|| "127.0.0.1/8".parse().unwrap());
        Ok(ReconcileMirror {
            tree: Arc::new(RwLock::new(HRTree::<K, State<V>>::new())),
            port: config.port,
            socket: Arc::new(socket),
            net: Arc::new(RwLock::new(net)),
            rng: Arc::new(RwLock::new(StdRng::from_entropy())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            authenticator,
            sender_counter: Arc::new(replay::SenderCounter::new()),
            replay_filter: Arc::new(replay::ReplayFilter::new(config.freshness_window)),
            on_update: Arc::new(RwLock::new(Box::new(|_, _| {}))),
            max_peers: config.max_peers,
        })
    }

    /// Provide the address of a known dated peer, reducing the time to first sync.
    pub fn with_seed(self, peer: IpAddr) -> Self {
        self.peers.write().insert(peer, Instant::now());
        self
    }

    /// (runtime) Retune the network this mirror probes for discovery, visible to all clones.
    pub fn set_net(&self, net: IpNet) {
        *self.net.write() = net;
    }

    /// The network this mirror currently probes for discovery.
    pub fn net(&self) -> IpNet {
        *self.net.read()
    }

    /// Register a hook invoked (outside the map lock) just before each inbound value is integrated.
    ///
    /// Useful to be notified of changes mirrored from the dated cluster. The tombstone case is
    /// `State::Tombstone`.
    pub fn add_on_update<F: Send + Sync + Fn(&K, &State<V>) + 'static>(&self, on_update: F) {
        *self.on_update.write() = Box::new(on_update);
    }

    /// Get the live value for a key, or `None` if the key is absent or holds a mirrored tombstone.
    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.tree.read();
        RwLockReadGuard::try_map(guard, |tree| tree.get(k).and_then(|vo| vo.value())).ok()
    }

    /// Whether the mirror currently holds a live value for the key (a tombstone counts as absent).
    pub fn contains_key(&self, k: &K) -> bool {
        self.tree.read().get(k).is_some_and(|vo| !vo.is_tombstone())
    }

    /// Number of entries currently stored, **including mirrored tombstones**.
    pub fn len(&self) -> usize {
        self.tree.read().len()
    }

    /// Whether the mirror is empty (no entries at all, tombstones included).
    pub fn is_empty(&self) -> bool {
        self.tree.read().is_empty()
    }

    /// Value-only fingerprint over a range. After convergence this equals the dated peer's
    /// [`value_fingerprint`](crate::ReconcileStore::value_fingerprint) over the same range.
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.tree.read().hash(&range)
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    /// Integrate inbound value-only updates by plain overwrite (the mirror holds no timestamp to
    /// compare against — it trusts the authoritative dated peer). Hooks run outside the map lock,
    /// so a hook may safely call back into the mirror.
    /// Apply inbound value-only updates by **overwriting** the local entry.
    ///
    /// A mirror holds no stamps, so there is nothing to compare and no merge to perform: the
    /// arriving projection is taken as-is. This is sound only because the dated sender already
    /// applied last-write-wins — see the type-level note and `ARCHITECTURE.md` §7 D9.
    fn integrate(&self, updates: Vec<(K, State<V>)>) {
        if updates.is_empty() {
            return;
        }
        {
            let hook = self.on_update.read();
            for (k, vo) in &updates {
                hook(k, vo);
            }
        }
        let mut guard = self.tree.write();
        for (k, vo) in updates {
            guard.insert(k, vo);
        }
    }

    /// Send our value-only comparison items to every known peer plus a random address (discovery),
    /// kicking off / continuing a value-only reconciliation round.
    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let segments = proto::start_diff(&self.tree.read());
        send_buf.clear();
        for segment in segments {
            Message::ValueComparisonItem::<K, WireDated<V>, State<V>>(segment)
                .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                .unwrap();
        }
        let mut peers = self.get_peers();
        // A random address out of the peer network, for discovery — like the dated store, we do not
        // add it to the known peers; a real peer there will answer and be recorded then.
        let net = *self.net.read();
        let addr = gen_ip(&mut *self.rng.write(), net);
        peers.push(addr);
        for peer in peers {
            trace!("mirror start_diff {} bytes to {peer}", send_buf.len());
            if let Err(err) = send_to_retry(
                &UdpTransport::new(Arc::clone(&self.socket)),
                &self.authenticator,
                &self.sender_counter,
                send_buf,
                SocketAddr::new(peer, self.port),
            )
            .await
            {
                warn!(
                    "mirror failed to send reconciliation initiation to {peer}: {err}; continuing"
                );
            }
        }
    }

    async fn handle_messages(
        &self,
        payload: auth::Payload<'_>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) {
        let payload = payload.as_bytes();
        trace!("mirror received {} bytes from {peer}", payload.len());
        let mut value_in_comparison = Vec::new();
        let mut value_updates: Vec<(K, State<V>)> = Vec::new();
        let mut deserializer = Deserializer::from_slice(payload, DefaultOptions::new());
        loop {
            match Message::<K, WireDated<V>, State<V>>::deserialize(&mut deserializer) {
                Err(ref kind) => {
                    if let bincode::ErrorKind::Io(err) = kind.as_ref() {
                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                            break;
                        }
                    }
                    // Never panic on network input (remote-DoS hardening, like the dated engine).
                    warn!(
                        "mirror failed to deserialize datagram from {peer}, dropping it: {kind:?}"
                    );
                    break;
                }
                Ok(Message::ValueComparisonItem(segment)) => value_in_comparison.push(segment),
                Ok(Message::ValueUpdate(update)) => value_updates.push(update),
                // The dated channel is meaningless to a mirror (it cannot store dated values nor
                // participate in causal stability). Ignore it.
                Ok(Message::ComparisonItem(_)) | Ok(Message::Update(_)) | Ok(Message::Ack(_)) => {}
            }
        }

        self.integrate(value_updates);

        if !value_in_comparison.is_empty() {
            debug!(
                "mirror received {} value-only segments",
                value_in_comparison.len()
            );
            let mut out_comparison = Vec::new();
            let mut differences = Vec::new();
            {
                let guard = self.tree.read();
                proto::diff_round(
                    &guard,
                    value_in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // `differences` are ranges this mirror would owe the peer. A read-only mirror never
            // sends authoritative values, so we deliberately drop them and only bounce back the
            // refined comparison items that keep the peer's side of the diff progressing.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::<K, WireDated<V>, State<V>>::ValueComparisonItem)
                    .collect();
                let transport = UdpTransport::new(Arc::clone(&self.socket));
                let codec = BincodeCodec::new();
                let ports = SendPorts {
                    transport: &transport,
                    codec: &codec,
                    authenticator: &self.authenticator,
                    sender_counter: &self.sender_counter,
                };
                send_messages_to(&messages, &ports, &peer, send_buf).await;
            }
        }
    }

    /// Run the mirror's reconciliation loop forever. Spawn this on a task; the mirror converges to
    /// the dated cluster's current values and reflects deletions as tombstones.
    pub async fn run(self) {
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        self.start_reconciliation(&mut send_buf).await;
        loop {
            match timeout(ACTIVITY_TIMEOUT, self.socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    debug!("mirror: no recent activity; initiating value-only diff");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => warn!("mirror network error in recv_from: {err}"),
                Ok(Ok((size, peer))) => {
                    if peer.port() != self.port {
                        warn!(
                            "mirror received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    if size == recv_buf.len() {
                        warn!("mirror buffer too small for message, discarded");
                    } else {
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                let sender = peer.ip();
                                // Per-peer cap check: drop datagrams from unknown senders when the
                                // peers map is at capacity, before any per-sender state is
                                // allocated (peers slot or replay-filter entry).
                                {
                                    let guard = self.peers.read();
                                    if !guard.contains_key(&sender) && guard.len() >= self.max_peers
                                    {
                                        trace!(
                                            "mirror dropped datagram from {peer}: peer cap \
                                             reached ({}/{})",
                                            guard.len(),
                                            self.max_peers
                                        );
                                        continue;
                                    }
                                }
                                if self.authenticator.is_enabled()
                                    && !self.replay_filter.check_and_record(
                                        sender,
                                        payload.seq,
                                        payload.stamp,
                                    )
                                {
                                    trace!(
                                        "mirror dropped replayed datagram from {peer}: \
                                         seq={} stamp={}",
                                        payload.seq,
                                        payload.stamp
                                    );
                                    continue;
                                }
                                self.handle_messages(payload, peer, &mut send_buf).await;
                                // Record the sender so we keep gossiping value-only diffs to it.
                                self.peers.write().insert(sender, Instant::now());
                            }
                            None => trace!(
                                "mirror dropped datagram from {peer}: missing or invalid MAC"
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
    use crate::reconcile_store::Config;

    fn ephemeral_config() -> Config {
        // Port 0 (ephemeral) on the loopback default network.
        Config::default()
    }

    /// `get` returns the live value, and absent keys are `None`.
    #[tokio::test]
    async fn get_returns_integrated_value() {
        let mirror = ReconcileMirror::<i32, String>::new(ephemeral_config())
            .await
            .expect("bind failed");
        assert!(mirror.get(&1).is_none());
        mirror.integrate(vec![(1, State::Present("hello".to_string()))]);
        assert_eq!(mirror.get(&1).as_deref(), Some(&"hello".to_string()));
        assert!(mirror.contains_key(&1));
        assert_eq!(mirror.len(), 1);
    }

    /// A mirrored tombstone (`State::Tombstone`) hides the value but is still a stored entry.
    #[tokio::test]
    async fn mirrors_tombstones() {
        let mirror = ReconcileMirror::<i32, String>::new(ephemeral_config())
            .await
            .expect("bind failed");
        mirror.integrate(vec![(1, State::Present("v".to_string()))]);
        assert_eq!(mirror.get(&1).as_deref(), Some(&"v".to_string()));

        // A later tombstone overwrites it: the value disappears from `get`, but the key is retained
        // as a tombstone (the mirror has no timestamp and trusts the authoritative peer).
        mirror.integrate(vec![(1, State::Tombstone)]);
        assert!(mirror.get(&1).is_none());
        assert!(!mirror.contains_key(&1));
        assert_eq!(mirror.len(), 1, "the tombstone is retained as an entry");
    }

    /// The on-update hook fires for every integrated value, including tombstones.
    #[tokio::test]
    async fn on_update_hook_fires() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mirror = ReconcileMirror::<i32, i32>::new(ephemeral_config())
            .await
            .expect("bind failed");
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        mirror.add_on_update(move |_, _| {
            count2.fetch_add(1, Ordering::SeqCst);
        });
        mirror.integrate(vec![(1, State::Present(10)), (2, State::Tombstone)]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// The mirror's value-only fingerprint matches an independently-built tree of the same logical
    /// content — i.e. timestamps genuinely play no part in the hash.
    #[tokio::test]
    async fn value_fingerprint_is_timestamp_independent() {
        let mirror = ReconcileMirror::<i32, String>::new(ephemeral_config())
            .await
            .expect("bind failed");
        mirror.integrate(vec![
            (1, State::Present("a".to_string())),
            (2, State::Tombstone),
        ]);

        let mut reference: HRTree<i32, State<String>> = HRTree::new();
        reference.insert(1, State::Present("a".to_string()));
        reference.insert(2, State::Tombstone);

        assert_eq!(mirror.fingerprint(..), reference.hash(&..));
    }

    /// A live value and its `State` projection hash identically only via the value-only basis:
    /// per-entry, the dateless mirror saves the whole `Timestamp` (the point of the dateless mirror).
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
