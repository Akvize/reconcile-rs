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
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeBounds;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bincode::{DefaultOptions, Deserializer, Serializer};
use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use socket2::SockRef;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::auth;
use crate::bounds::{Key, Value};
use crate::clock::{Clock, HlcClock, Timestamp, Timestamped};
use crate::discovery::{Discovery, RandomProbe};
use crate::fingerprint::Fingerprint;
use crate::gen_ip::{host_net, net_of};
use crate::observability;
use crate::proto::{self, HashSegment};
use crate::reconcilable::{MaybeTombstone, Projectable, Reconcilable};
use crate::reconcile_store::{Config, MAX_NETS};
use crate::replay;
use crate::HRTree;

const BUFFER_SIZE: usize = 65507;
const PEER_EXPIRATION: Duration = Duration::from_secs(60);
/// Byte budget for the tombstone re-acknowledgments piggybacked onto each reconciliation datagram.
/// Kept well under [`BUFFER_SIZE`] so the datagram still fits after authentication framing; when
/// more tombstones are held than fit in one round, a round-advancing window covers the remainder on
/// subsequent rounds.
const REACK_BYTE_BUDGET: usize = 8 * 1024;

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

/// Derive a node's **local network** from the declared `nets` and its `listen_addr`.
///
/// The local network is whichever declared net contains the listen address — the one a node
/// reconciles with aggressively (every round). When none does (a misconfiguration), this falls back
/// to the node's own host route (`/32`-`/128`), so only the node itself is local and every peer is
/// treated as remote (throttled) rather than silently mis-qualified.
///
/// Reused on construction **and** on every runtime mutation of `nets`, so the warning fires only at
/// those mutation boundaries — never once per reconciliation round.
fn derive_local_net(nets: &[IpNet], listen_addr: IpAddr) -> IpNet {
    net_of(nets, listen_addr).unwrap_or_else(|| {
        warn!(
            "listen address {listen_addr} is contained in none of the configured networks \
             {nets:?}; cannot identify the local network — treating only this node as local, so \
             every peer is remote and reconciled on the throttled cross-network cadence. Declare \
             the network containing {listen_addr} via Config::with_net or ReconcileStore::add_net.",
        );
        host_net(listen_addr)
    })
}

/// Apply the configured `SO_RCVBUF` / `SO_SNDBUF` to the gossip socket.
///
/// `setsockopt` never errors for asking too much: the kernel clamps each request to the OS maximum
/// (`net.core.rmem_max` / `wmem_max` on Linux), so a generous default only ever helps. Clamping is
/// therefore expected on an untuned host — **not** a warning condition — so the achieved size is
/// reported at `debug`; an operator who needs the full buffer raises the sysctl (see the README)
/// and can confirm via `/proc/net/snmp` `RcvbufErrors`. Only an actual `setsockopt` failure (which
/// does not happen for a buffer request on a valid socket) is surfaced as a `warn`. A `None` size
/// leaves the inherited OS default untouched.
///
/// Note: on Linux `getsockopt` reports the *doubled* value (bookkeeping overhead), so a fully
/// honoured request reads back larger than asked.
fn set_socket_buffers(socket: &UdpSocket, config: &Config) {
    let sock = SockRef::from(socket);
    if let Some(size) = config.recv_buffer_size {
        match sock.set_recv_buffer_size(size) {
            Ok(()) => match sock.recv_buffer_size() {
                Ok(actual) => debug!(
                    "gossip socket SO_RCVBUF: requested {size} B, OS granted {actual} B \
                     (raise net.core.rmem_max if a larger buffer is needed)"
                ),
                Err(e) => debug!("could not read back SO_RCVBUF: {e}"),
            },
            Err(e) => warn!("failed to set gossip socket SO_RCVBUF to {size} B: {e}"),
        }
    }
    if let Some(size) = config.send_buffer_size {
        match sock.set_send_buffer_size(size) {
            Ok(()) => match sock.send_buffer_size() {
                Ok(actual) => debug!(
                    "gossip socket SO_SNDBUF: requested {size} B, OS granted {actual} B \
                     (raise net.core.wmem_max if a larger buffer is needed)"
                ),
                Err(e) => debug!("could not read back SO_SNDBUF: {e}"),
            },
            Err(e) => warn!("failed to set gossip socket SO_SNDBUF to {size} B: {e}"),
        }
    }
}

/// The internal reconciliation engine at the network level.
/// This struct does not handle removals, which are managed by the external layer.
/// For more information, see [`ReconcileStore`](crate::reconcile_store::ReconcileStore).
pub(crate) struct ReconcileEngine<K, V: Projectable> {
    /// All engine state lives behind a single [`Arc`] so that cloning the engine (every clone
    /// shares the same maps, peers, clock, …) is a single refcount bump. Cloning is therefore
    /// cheap and bound-free, and the previously hand-written `Clone` impl reduces to a derive.
    /// Fields are reached transparently through the [`Deref`] impl below, so every existing
    /// `self.field` / `engine.field` access keeps working unchanged.
    inner: Arc<Inner<K, V>>,
}

/// Shared, refcounted state of a [`ReconcileEngine`]; see that struct for the rationale.
pub(crate) struct Inner<K, V: Projectable> {
    pub(crate) map: Arc<RwLock<HRTree<K, V>>>,
    /// Value-only **projection** of [`map`](Self::map), kept in sync at every map mutation.
    ///
    /// Its range fingerprints are timestamp-less by construction (see
    /// [`ValueOnly`](crate::reconcilable::ValueOnly)), which is what lets a dateless mirror
    /// converge with this dated store over the existing range-diff protocol. It is read-only state
    /// for the dated↔dated path and never touches the causal-stability bookkeeping.
    pub(crate) projection: Arc<RwLock<HRTree<K, V::Projected>>>,
    port: u16,
    socket: Arc<UdpSocket>,
    /// The geographical networks the cluster spans, each a CIDR. One random address is
    /// probed per network each round for auto-discovery, and a peer's network is derived from its IP
    /// via `IpNet::contains`. Shared and mutable at runtime (see [`set_nets`](Self::set_nets)) so the
    /// topology can be retuned live without recreating the node — the [`run`](Self::run) loop snapshots
    /// it at the start of each round.
    nets: Arc<RwLock<Vec<IpNet>>>,
    /// The network containing this node's listen address — the one it reconciles with aggressively.
    /// When no configured net contains the listen address, this is the node's own host route, so
    /// only itself is local (see [`Self::new`]). **Derived** from [`nets`](Self::nets): recomputed on
    /// every runtime mutation of `nets` (never set independently), so it can never drift out of sync.
    local_net: Arc<RwLock<IpNet>>,
    /// This node's own listen address, kept so [`local_net`](Self::local_net) can be re-derived
    /// whenever [`nets`](Self::nets) changes at runtime.
    listen_addr: IpAddr,
    /// Send the full anti-entropy comparison to remote-network peers every `remote_interval`
    /// rounds (the [`round`](Self::round) counter); local-network peers are contacted every round.
    /// Shared atomic so it can be retuned at runtime.
    remote_interval: Arc<AtomicU32>,
    /// Max peers contacted per remote network on each cross-network round (bounds WAN fan-out).
    /// Shared atomic so it can be retuned at runtime.
    remote_fanout: Arc<AtomicUsize>,
    /// How long the [`run`](Self::run) loop waits for inbound activity before initiating a
    /// reconciliation round — the effective gossip cadence. Shared so it can be retuned at runtime.
    reconcile_interval: Arc<RwLock<Duration>>,
    /// Rate (bytes/sec) at which a single bulk anti-entropy value transfer to one peer is paced, or
    /// `None` to send back-to-back. Mirrors [`Config::bulk_send_rate`](crate::reconcile_store::Config::bulk_send_rate)
    /// and is read by [`spawn_paced_send`](Self::spawn_paced_send).
    bulk_send_rate: Option<usize>,
    /// Peers with a bulk value transfer currently in flight. At most one paced dump
    /// runs per peer at a time: while one is in flight, the reconcile timer re-firing (the receiver
    /// applies updates silently, so the holder sees a lull and re-initiates) must NOT spawn a second
    /// dump over ranges still in transit — that is precisely the byte amplification this prevents.
    /// Entries are inserted before a dump is spawned and removed (via an RAII guard, so it survives a
    /// panic in the send task) when it finishes.
    bulk_in_flight: Arc<RwLock<HashSet<SocketAddr>>>,
    /// Count of bulk dump tasks currently in flight across all peers. When this reaches
    /// [`max_concurrent_bulk_dumps`](Self::max_concurrent_bulk_dumps), new dumps are skipped before
    /// allocating their snapshot Vec; the protocol is retry-driven, so the requesting peer's next
    /// diff round re-triggers the dump once a slot is free. Decremented by an RAII guard on the
    /// spawned task, so a panicking task never leaks a slot.
    bulk_dumps_in_flight: Arc<AtomicUsize>,
    /// Global cap on the number of concurrently active paced bulk dumps.
    max_concurrent_bulk_dumps: usize,
    /// Monotonic reconciliation-round counter, shared across clones, gating the cross-network cadence.
    round: Arc<AtomicU32>,
    rng: Arc<RwLock<StdRng>>,
    /// Speculative peer discovery, behind the [`Discovery`] port. Defaults to [`RandomProbe`] (one
    /// random address per declared network each round); consulted once per reconciliation round and
    /// shares this engine's live `nets` and `rng`. Its results are one-shot targets, never seeded as
    /// known peers — see [`start_reconciliation`](Self::start_reconciliation).
    probe: Arc<dyn Discovery>,
    pub(crate) peers: Arc<RwLock<HashMap<IpAddr, Instant>>>,
    pub(crate) pre_insert: Arc<RwLock<PreInsertCallback<K, V>>>,
    /// Per-datagram authentication policy; carries the cluster key when enabled.
    authenticator: auth::Authenticator,
    /// Sender-side replay counter: a monotonically increasing sequence number stamped onto every
    /// outgoing datagram in authenticated modes. Shared across all clones so that parallel send
    /// paths draw from a single strictly-increasing sequence.
    sender_counter: Arc<replay::SenderCounter>,
    /// Receiver-side per-peer replay filter. Enforces the sequence-number window and freshness
    /// window for all inbound datagrams in authenticated modes. Shared across clones (the same
    /// receive loop instance holds the only clone that calls `check_and_record`).
    replay_filter: Arc<replay::ReplayFilter>,
    /// Monotonic set of every peer this node has ever exchanged messages with.
    ///
    /// Unlike [`peers`](Self::peers), entries are never expired automatically: a tombstone
    /// must be acknowledged by every member (or the member explicitly decommissioned) before
    /// it can be garbage-collected. This is what gates tombstone GC on *causal stability* and
    /// prevents a peer that is partitioned for longer than the peer-expiration window from
    /// resurrecting a deleted value on its return.
    ///
    /// Multi-network note: remote-network members are gossiped to on a slower cadence, so
    /// their tombstone acknowledgments arrive later and cross-network GC is correspondingly slower —
    /// but still strictly correct, since GC never proceeds before *every* member has acked. Lowering
    /// [`Config::remote_interval`] or raising [`Config::remote_fanout`] speeds it up at the
    /// cost of WAN traffic.
    pub(crate) members: Arc<RwLock<HashSet<IpAddr>>>,
    /// Per-tombstone acknowledgments: `key -> (peer -> version token of the tombstone it holds)`.
    pub(crate) tombstone_acks: Arc<RwLock<HashMap<K, HashMap<IpAddr, u64>>>>,
    /// The set of keys this node currently holds as a tombstone, maintained at the single map
    /// mutation sink ([`map_insert`](ReconcileEngine::map_insert) /
    /// [`gc_remove`](ReconcileEngine::gc_remove)). It lets the gossip round enumerate held
    /// tombstones in O(1) (without scanning the map) and re-acknowledge each of them to every
    /// member every round, so causal-stability acknowledgments keep flowing until a tombstone is
    /// collected — without which the acknowledgment matrix never completes in a cluster of three or
    /// more nodes.
    pub(crate) live_tombstones: Arc<RwLock<HashSet<K>>>,
    /// This node's clock, reached only through the [`Clock`] port so the engine never reads
    /// physical time itself ([`HlcClock`] is the default adapter; a test injects a deterministic
    /// stub via [`new_with_clock`](ReconcileEngine::new_with_clock)). Shared across all clones so
    /// that every write and every received timestamp advances the same clock.
    clock: Arc<dyn Clock>,
    /// `true` when the node id was generated at random (i.e. `Config::node_id` was `None`).
    /// Exposed via [`node_id_is_random`](ReconcileEngine::node_id_is_random) so that the store
    /// layer can warn operators when persistence is configured but the identity is ephemeral.
    pub(crate) node_id_is_random: bool,
    /// Hard cap on the number of tracked remote peers. Datagrams from unknown senders are dropped
    /// before any per-sender state is allocated when the membership set reaches this size.
    max_peers: usize,
}

impl<K, V: Projectable> Clone for ReconcileEngine<K, V> {
    fn clone(&self) -> Self {
        ReconcileEngine {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V: Projectable> std::ops::Deref for ReconcileEngine<K, V> {
    type Target = Inner<K, V>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Represent an atomic message for the reconciliation protocol.
///
/// The three original variants form the **dated** channel (`ComparisonItem` / `Update` / `Ack`),
/// exchanged between full dated stores exactly as before — their bincode tags (0, 1, 2) and wire
/// bytes are unchanged. The two trailing variants form the additive **value-only** channel used by
/// lightweight dateless mirrors: they are tagged 3 and 4, so a node that does not
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
    /// identified a difference. Carries the projected payload only (no [`Timestamp`]).
    ValueUpdate((K, P)),
}

impl<K: Key, V: Value + MaybeTombstone + Projectable + Reconcilable + Timestamped>
    ReconcileEngine<K, V>
{
    /// Create a new engine, binding the gossip UDP socket.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the UDP socket cannot be bound to
    /// `(config.listen_addr, config.port)` — for example, because the port is already in use or
    /// the address is not available on this host.
    pub async fn new(config: Config) -> io::Result<Self> {
        // Default adapter for the `Clock` port: the chrono-backed Hybrid Logical Clock. This is the
        // only place the engine names a concrete clock; everything else goes through `dyn Clock`.
        let node_id_is_random = config.node_id.is_none();
        let node_id = config.node_id.unwrap_or_else(rand::random);
        let clock: Arc<dyn Clock> = Arc::new(HlcClock::new(node_id));
        Self::build(config, clock, node_id_is_random).await
    }

    /// Construct an engine over an explicit [`Clock`] adapter. Test-only seam: a deterministic
    /// clock (`ManualClock`) makes HLC behaviour reproducible without real wall-clock time.
    #[cfg(test)]
    pub(crate) async fn new_with_clock(config: Config, clock: Arc<dyn Clock>) -> io::Result<Self> {
        // A test-supplied clock implies a test-supplied (stable) identity; mark it as non-random
        // so persistence tests do not trigger the operator warning.
        Self::build(config, clock, false).await
    }

    async fn build(
        config: Config,
        clock: Arc<dyn Clock>,
        node_id_is_random: bool,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::new(config.listen_addr, config.port)).await?;
        info!("Listening on: {}", socket.local_addr()?);
        // Size the kernel send/receive buffers before any traffic flows.
        set_socket_buffers(&socket, &config);
        let authenticator = auth::Authenticator::new(config.cluster_key, config.encrypt);
        if authenticator.is_encrypted() {
            debug!("per-datagram authenticated encryption (XChaCha20-Poly1305) ENABLED");
        } else if authenticator.is_enabled() {
            debug!("per-datagram MAC authentication ENABLED");
        } else {
            warn!(
                "SECURITY: no cluster key set — UDP reconciliation is UNAUTHENTICATED. Any host \
                 that can send UDP to this port can forge updates and poison the cluster via \
                 last-write-wins. Set Config::with_cluster_key on every node, or restrict the \
                 network to a trusted underlay. See REVIEW.md F3."
            );
        }
        let map = HRTree::<K, V>::new();
        let projection = HRTree::<K, V::Projected>::new();
        // The geographical networks this cluster spans. With none declared, fall back to the
        // historical flat loopback cluster.
        let mut nets: Vec<IpNet> = config.nets.iter().flatten().copied().collect();
        if nets.is_empty() {
            nets.push("127.0.0.1/8".parse().unwrap());
        }
        // Qualify the local network from the declared nets (warns + host-route fallback on
        // misconfiguration). Re-derived identically on every runtime mutation of `nets`.
        let local_net = derive_local_net(&nets, config.listen_addr);
        // `nets` and `rng` are shared with the default speculative `RandomProbe` discovery, so a
        // runtime topology change is immediately reflected in what it probes.
        let nets = Arc::new(RwLock::new(nets));
        let rng = Arc::new(RwLock::new(StdRng::from_entropy()));
        let probe: Arc<dyn Discovery> =
            Arc::new(RandomProbe::new(Arc::clone(&nets), Arc::clone(&rng)));
        Ok(ReconcileEngine {
            inner: Arc::new(Inner {
                map: Arc::new(RwLock::new(map)),
                projection: Arc::new(RwLock::new(projection)),
                port: config.port,
                socket: Arc::new(socket),
                nets,
                local_net: Arc::new(RwLock::new(local_net)),
                listen_addr: config.listen_addr,
                remote_interval: Arc::new(AtomicU32::new(config.remote_interval)),
                remote_fanout: Arc::new(AtomicUsize::new(config.remote_fanout)),
                reconcile_interval: Arc::new(RwLock::new(config.reconcile_interval)),
                bulk_send_rate: config.bulk_send_rate,
                bulk_in_flight: Arc::new(RwLock::new(HashSet::new())),
                bulk_dumps_in_flight: Arc::new(AtomicUsize::new(0)),
                max_concurrent_bulk_dumps: config.max_concurrent_bulk_dumps,
                round: Arc::new(AtomicU32::new(0)),
                rng,
                probe,
                peers: Arc::new(RwLock::new(HashMap::new())),
                pre_insert: Arc::new(RwLock::new(Box::new(|_, _| {}))),
                authenticator,
                sender_counter: Arc::new(replay::SenderCounter::new()),
                replay_filter: Arc::new(replay::ReplayFilter::new(config.freshness_window)),
                members: Arc::new(RwLock::new(HashSet::new())),
                tombstone_acks: Arc::new(RwLock::new(HashMap::new())),
                live_tombstones: Arc::new(RwLock::new(HashSet::new())),
                clock,
                node_id_is_random,
                max_peers: config.max_peers,
            }),
        })
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

    /// Insert into the dated `map` **and** mirror the value-only projection (and the live-tombstone
    /// index), under a consistent lock order (`map` → `live_tombstones` → `projection`) shared by
    /// every mutation path so the structures never deadlock against each other. The caller already
    /// holds the `map` write guard.
    fn map_insert(&self, guard: &mut HRTree<K, V>, key: K, value: V) -> Option<V> {
        // Keep the live-tombstone index in step with the map at its single mutation sink: a
        // tombstone value adds the key; any live value (a fresh insert, or an LWW overwrite that
        // resurrects a previously-deleted key) removes it. This index drives the per-round
        // causal-stability re-acknowledgment in `start_reconciliation`.
        {
            let mut live_tombstones = self.live_tombstones.write();
            if value.is_tombstone() {
                live_tombstones.insert(key.clone());
            } else {
                live_tombstones.remove(&key);
            }
        }
        self.projection.write().insert(key.clone(), value.project());
        guard.insert(key, value)
    }

    /// Remove a key from the dated `map` and its value-only projection (the GC removal path).
    pub(crate) fn gc_remove(&self, key: &K) -> Option<V> {
        let mut guard = self.map.write();
        self.live_tombstones.write().remove(key);
        self.projection.write().remove(key);
        guard.remove(key)
    }

    /// Mint a fresh Hybrid Logical Clock timestamp for a local write.
    pub fn clock_now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Advance the clock past `stamp` using the trusted path — for stamps this node itself
    /// authored (e.g. restored from its own persisted state). Unlike the remote-peer path,
    /// this does not apply the far-future clamp, so the clock reliably chases its own output
    /// even after a backward wall-clock step (NTP correction, VM resume).
    pub(crate) fn clock_observe_trusted(&self, stamp: Timestamp) {
        self.clock.observe_trusted(stamp);
    }

    /// Returns `true` when the node id was generated at random (`Config::node_id` was `None`).
    pub(crate) fn node_id_is_random(&self) -> bool {
        self.node_id_is_random
    }

    /// (runtime) Replace the declared networks wholesale and re-derive the local network.
    pub(crate) fn set_nets(&self, nets: &[IpNet]) {
        let nets = nets.to_vec();
        let local = derive_local_net(&nets, self.listen_addr);
        // local_net is always derived from nets; update it together so a reader never observes a
        // local net that is inconsistent with the freshly-installed nets for long.
        *self.local_net.write() = local;
        *self.nets.write() = nets;
    }

    /// (runtime) Declare an additional network. Idempotent; returns `false` (and logs) if the
    /// [`MAX_NETS`](crate::reconcile_store::MAX_NETS) cap is reached.
    pub(crate) fn add_net(&self, net: IpNet) -> bool {
        let mut guard = self.nets.write();
        if guard.contains(&net) {
            return true;
        }
        if guard.len() >= MAX_NETS {
            warn!("cannot add network {net}: already at the maximum of {MAX_NETS} networks");
            return false;
        }
        guard.push(net);
        *self.local_net.write() = derive_local_net(&guard, self.listen_addr);
        true
    }

    /// (runtime) Stop declaring a network. Returns whether it was present. Anti-entropy repair of
    /// already-known peers is **not** gated on net membership, so removing a net never orphans a
    /// real peer — it only stops discovery probes into that range and may reclassify peers from
    /// local to remote (throttled) cadence.
    pub(crate) fn remove_net(&self, net: IpNet) -> bool {
        let mut guard = self.nets.write();
        let before = guard.len();
        guard.retain(|n| *n != net);
        let removed = guard.len() != before;
        if removed {
            *self.local_net.write() = derive_local_net(&guard, self.listen_addr);
        }
        removed
    }

    pub(crate) fn nets(&self) -> Vec<IpNet> {
        self.nets.read().clone()
    }

    pub(crate) fn local_net(&self) -> IpNet {
        *self.local_net.read()
    }

    pub(crate) fn set_remote_interval(&self, interval: u32) {
        self.remote_interval.store(interval, Ordering::Relaxed);
    }

    pub(crate) fn set_remote_fanout(&self, fanout: usize) {
        self.remote_fanout.store(fanout, Ordering::Relaxed);
    }

    pub(crate) fn set_reconcile_interval(&self, interval: Duration) {
        *self.reconcile_interval.write() = interval;
    }

    fn get_peers(&self) -> Vec<IpAddr> {
        let mut guard = self.peers.write();
        guard.retain(|_, instant| instant.elapsed() < PEER_EXPIRATION);
        guard.keys().cloned().collect()
    }

    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        // 1) Call the pre-insert hook *before* taking the map lock
        (self.pre_insert.read())(&key, &value);

        // A tombstone value is a removal; a live value is an insertion. Counting here (rather
        // than in `ReconcileStore`) keeps every local mutation path covered.
        if value.is_tombstone() {
            observability::record_remove();
        } else {
            observability::record_insert();
        }

        // 2) Now acquire write lock and insert (mirroring the value-only projection)
        let mut guard = self.map.write();
        self.map_insert(&mut guard, key, value)
    }

    /// Broadcast a batch of protocol messages to every currently-known peer.
    ///
    /// Spawns a detached task so the calling write path does not block on the network. Shared by
    /// every local-mutation broadcast path (`insert` / `insert_bulk`) so the spawn/loop/send
    /// boilerplate lives in exactly one place.
    fn broadcast(&self, messages: Vec<Message<K, V, V::Projected>>) {
        let peers = self.get_peers();
        let port = self.port;
        let socket = Arc::clone(&self.socket);
        let authenticator = self.authenticator.clone();
        let sender_counter = Arc::clone(&self.sender_counter);
        tokio::spawn(async move {
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(
                    &messages,
                    Arc::clone(&socket),
                    &authenticator,
                    &sender_counter,
                    &peer,
                    &mut send_buf,
                )
                .await;
            }
        });
    }

    /// Try to claim both a per-peer in-flight slot and a global concurrent-dump slot.
    ///
    /// Returns `Some((peer_guard, global_guard))` when both slots are available, or `None` when
    /// either is already taken. The caller checks this **before** snapshotting the differing range
    /// into a `Vec`, so a skipped dump allocates nothing. The guards release their slots on drop,
    /// ensuring cleanup even if the spawned task panics.
    fn try_claim_dump_slot(
        &self,
        peer: SocketAddr,
    ) -> Option<(BulkInFlightGuard, BulkDumpCountGuard)> {
        // Per-peer guard: at most one dump per peer at a time.
        if !self.bulk_in_flight.write().insert(peer) {
            return None;
        }
        // Global budget: at most `max_concurrent_bulk_dumps` across all peers. The
        // compare-exchange loop increments only if currently below the cap. If at cap, release the
        // per-peer mark before returning so that slot is not leaked.
        let budget = self.max_concurrent_bulk_dumps;
        let claimed = self
            .bulk_dumps_in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < budget {
                    Some(n + 1)
                } else {
                    None
                }
            })
            .is_ok();
        if !claimed {
            self.bulk_in_flight.write().remove(&peer);
            trace!("skipped bulk dump to {peer}: global dump budget ({budget}) exhausted");
            return None;
        }
        Some((
            BulkInFlightGuard {
                set: Arc::clone(&self.bulk_in_flight),
                peer,
            },
            BulkDumpCountGuard {
                counter: Arc::clone(&self.bulk_dumps_in_flight),
            },
        ))
    }

    /// Send a bulk batch of differing values to one peer on a detached, **rate-paced** task.
    ///
    /// This is the cold/bulk anti-entropy path: when a peer differs over a whole range it must
    /// receive every value in that range (most starkly, a cold/empty peer pulling the whole
    /// dataset). Dumping them back-to-back inline on the receive loop would (a) overrun the
    /// receiver's socket buffer — datagrams dropped in the kernel — and (b) hold the receive loop for
    /// the whole transfer, so no other peer is served meanwhile.
    ///
    /// Two cooperating mechanisms fix the cold-sync amplification:
    ///
    /// 1. **Pacing** to [`bulk_send_rate`](Inner::bulk_send_rate) bytes/sec, on a spawned task, keeps
    ///    the burst below the receiver's overrun threshold and off the receive loop.
    /// 2. A **single-dump-per-peer guard**: while a dump is in flight to a peer, a second is not
    ///    started. The receiver applies updates *silently* (an `Update` triggers no reply), so the
    ///    holder's own reconcile timer sees a lull and re-initiates — without this guard it would
    ///    re-dump ranges still in transit, which is the byte amplification we are removing. Anything
    ///    genuinely still missing (e.g. a dropped datagram) is picked up by the next reconcile round
    ///    once the current dump finishes.
    /// 3. A **global concurrent-dump budget** (`max_concurrent_bulk_dumps`): even with the per-peer
    ///    guard, M peers diffing simultaneously would each hold a full snapshot Vec. This cap limits
    ///    total in-flight dump tasks. The caller checks the budget via [`try_claim_dump_slot`](Self::try_claim_dump_slot)
    ///    **before** building the snapshot Vec, so a skipped dump allocates nothing.
    fn spawn_paced_send(
        &self,
        messages: Vec<Message<K, V, V::Projected>>,
        peer: SocketAddr,
        peer_guard: BulkInFlightGuard,
        global_guard: BulkDumpCountGuard,
    ) {
        let socket = Arc::clone(&self.socket);
        let authenticator = self.authenticator.clone();
        let sender_counter = Arc::clone(&self.sender_counter);
        let rate = self.bulk_send_rate;
        tokio::spawn(async move {
            // Hold both RAII guards for the lifetime of the task: they release their respective
            // slots on drop, even if this task is aborted or panics.
            let _peer_guard = peer_guard;
            let _global_guard = global_guard;
            let mut send_buf = Vec::new();
            send_messages_paced(
                &messages,
                socket,
                &authenticator,
                &sender_counter,
                &peer,
                &mut send_buf,
                rate,
            )
            .await;
        });
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self.just_insert(key.clone(), value.clone());
        self.broadcast(vec![Message::Update::<K, V, V::Projected>((key, value))]);
        ret
    }

    /// Broadcast a single locally-mutated entry to peers, mirroring [`insert`](Self::insert)'s
    /// propagation. Used by in-place mutation paths (`ReconcileStore::get_mut`) that write the map
    /// directly and must still notify peers so the edit reconciles, without re-applying it locally.
    pub(crate) fn broadcast_update(&self, key: K, value: V) {
        self.broadcast(vec![Message::Update::<K, V, V::Projected>((key, value))]);
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        // 1) First run all pre-insert hooks outside the map lock
        for (key, value) in key_values {
            (self.pre_insert.read())(key, value);
            if value.is_tombstone() {
                observability::record_remove();
            } else {
                observability::record_insert();
            }
        }
        // 2) Then acquire the write lock once and insert them (mirroring the projection)
        let mut guard = self.map.write();
        for (key, value) in key_values {
            self.map_insert(&mut guard, key.clone(), value.clone());
        }
    }

    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.just_insert_bulk(key_values);
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, V, V::Projected>(kv.clone()))
            .collect();
        self.broadcast(messages);
    }

    /// Drive the gossip and reconciliation loops forever.
    ///
    /// This method does not return and cannot fail: network send errors are logged and
    /// counted, never fatal, so a vanished or unreachable peer cannot stop the loops.
    #[instrument(name = "reconcile.run", skip_all, fields(port = self.port))]
    pub async fn run(self) {
        // extra byte that easily detect when the buffer is too small
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        // start the protocol at the beginning
        self.start_reconciliation(&mut send_buf).await;
        // infinite loop
        loop {
            // Re-read each iteration so the cadence can be retuned at runtime.
            let recv_timeout = *self.reconcile_interval.read();
            match timeout(recv_timeout, self.socket.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    // timeout
                    debug!("no recent activity; initiating diff protocol");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => {
                    // network error
                    warn!("network error in recv_from: {err}");
                    observability::record_datagram_dropped("recv_error");
                }
                Ok(Ok((size, peer))) => {
                    // received datagram
                    observability::record_bytes_received(size);
                    if peer.port() != self.port {
                        warn!(
                            "received message from {peer}, but protocol port is {}",
                            self.port
                        );
                    }
                    if size == recv_buf.len() {
                        warn!("Buffer too small for message, discarded");
                        observability::record_datagram_dropped("too_large");
                    } else {
                        // Authenticate the datagram *before* any deserialization. Only a cleared
                        // `Payload` can reach `handle_messages`; a missing or invalid tag is
                        // dropped silently (trace-only, to avoid attacker-driven log flooding).
                        match self.authenticator.open(&recv_buf[..size]) {
                            Some(payload) => {
                                let sender = peer.ip();
                                // Per-peer cap check: if this sender is not yet tracked (not a
                                // member) and the membership set is at capacity, drop the datagram
                                // before allocating any per-sender state (replay filter entry,
                                // peers map slot, or membership slot). Authenticated datagrams are
                                // verified first (above), so this gate cannot be triggered by a
                                // forged source address in authenticated mode.
                                //
                                // The check is intentionally placed *before* the replay filter so
                                // that no replay-filter entry is created for a capped-out sender.
                                // Known senders (already in `members`) bypass the cap.
                                let is_known = self.members.read().contains(&sender);
                                if !is_known {
                                    let current_len = self.members.read().len();
                                    if current_len >= self.max_peers {
                                        trace!(
                                            "dropped datagram from {peer}: peer cap reached \
                                             ({current_len}/{})",
                                            self.max_peers
                                        );
                                        observability::record_datagram_dropped("peer_cap");
                                        continue;
                                    }
                                }
                                // In authenticated modes, perform the replay check *after*
                                // authentication (so forgeries cannot poison the per-peer state)
                                // and *before* message processing. In unauthenticated mode
                                // seq/stamp are both 0 and `is_enabled()` is false, so the check
                                // is skipped entirely.
                                if self.authenticator.is_enabled()
                                    && !self.replay_filter.check_and_record(
                                        sender,
                                        payload.seq,
                                        payload.stamp,
                                    )
                                {
                                    trace!(
                                        "dropped replayed or stale datagram from {peer}: \
                                         seq={} stamp={}",
                                        payload.seq,
                                        payload.stamp
                                    );
                                    observability::record_datagram_dropped("replay");
                                    continue;
                                }
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
                                // must NOT join the causal-stability membership set or it would block GC
                                // forever (the very regression we must avoid). Such a mirror is
                                // served reactively off `peer` and is not tracked as a peer/member.
                                if spoke_dated {
                                    self.peers.write().insert(sender, Instant::now());
                                    self.members.write().insert(sender);
                                }
                            }
                            None => {
                                trace!("dropped datagram from {peer}: missing or invalid MAC");
                                observability::record_datagram_dropped("bad_mac");
                            }
                        }
                    }
                }
            }
        }
    }

    #[instrument(name = "reconcile.round", skip_all)]
    pub async fn start_reconciliation(&self, send_buf: &mut Vec<u8>) {
        let timer = observability::timer();
        observability::record_reconcile_round();
        let segments = {
            let guard = self.map.read();
            proto::start_diff(&guard)
        };
        send_buf.clear();
        for segment in segments {
            Message::ComparisonItem::<K, V, V::Projected>(segment)
                .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                .expect("serializing a ComparisonItem into an in-memory buffer cannot fail");
        }
        // Geography-aware target selection. With a single network this reduces to the
        // historical behaviour: every known peer is local, so all of them are contacted each round,
        // plus a single random discovery probe.
        //
        // Snapshot the runtime-tunable topology/throttle once per round, so a concurrent mutation
        // cannot tear a round and we never hold a lock across the sends below.
        let nets = self.nets.read().clone();
        let local = *self.local_net.read();
        let remote_interval = self.remote_interval.load(Ordering::Relaxed).max(1);
        let remote_fanout = self.remote_fanout.load(Ordering::Relaxed);
        let round = self.round.fetch_add(1, Ordering::Relaxed);
        // Treat an interval of 0 as "every round" to avoid a modulo-by-zero.
        let do_remote = round.is_multiple_of(remote_interval);
        let known = self.get_peers();

        // De-duplicate so a discovery probe that happens to hit a known peer is not sent twice.
        let mut targets: HashSet<IpAddr> = HashSet::new();

        // Speculative discovery through the `Discovery` port (default: one random probe per network,
        // see `RandomProbe`). The probed address might not correspond to a real peer, so we do NOT
        // add it to the known-peer list; if a peer exists there, it will reply and be registered
        // then. An authoritative source (e.g. DNS) instead drives the store's seed/decommission loop
        // and is not consulted here. A transient failure simply yields no extra targets this round.
        targets.extend(self.probe.discover().await.unwrap_or_default());

        // Local network: contact every known peer, every round (fast intra-network convergence).
        for &addr in &known {
            if local.contains(&addr) {
                targets.insert(addr);
            }
        }

        // Remote peers: only on cross-network rounds, a bounded random subset per network — PLUS an
        // `unclassified` bucket for known peers matching no declared network. Anti-entropy repair is
        // deliberately **decoupled from net membership**: a peer learned by actual contact is never
        // excluded from repair regardless of the declared topology. Nets only steer discovery and the
        // local/remote throttle split, so changing the topology at runtime can never orphan a real
        // peer from repair (worst case: suboptimal WAN traffic, never silent divergence). Fan-out is
        // applied per bucket to keep WAN traffic bounded without designating any node as a relay.
        if do_remote {
            let remote_nets: Vec<IpNet> = nets.iter().copied().filter(|&n| n != local).collect();
            let mut buckets: HashMap<Option<usize>, Vec<IpAddr>> = HashMap::new();
            for &addr in &known {
                if local.contains(&addr) {
                    continue; // already contacted every round above
                }
                let bucket = remote_nets.iter().position(|n| n.contains(&addr));
                buckets.entry(bucket).or_default().push(addr);
            }
            let mut rng = self.rng.write();
            for (_, mut peers) in buckets {
                peers.shuffle(&mut *rng);
                targets.extend(peers.into_iter().take(remote_fanout));
            }
        }

        // Piggyback causal-stability re-acknowledgments for the tombstones we hold.
        self.append_tombstone_reacks(send_buf, round);

        // initiate the reconciliation protocol with the selected peers and discovery probes
        for peer in targets {
            trace!("start_diff {} bytes to {peer}", send_buf.len());
            if let Err(err) = send_to_retry(
                &self.socket,
                &self.authenticator,
                &self.sender_counter,
                send_buf,
                (peer, self.port),
            )
            .await
            {
                warn!("failed to send reconciliation initiation to {peer}: {err}; continuing");
            }
        }
        observability::record_round_duration(timer);
    }

    /// Append a causal-stability re-acknowledgment (`Message::Ack`) for each tombstone this node
    /// currently holds onto `send_buf`, returning the number appended.
    ///
    /// Acks otherwise only flow back to the node a tombstone was *received* from, so in a cluster of
    /// three or more nodes two non-originating replicas never learn that the other holds the
    /// deletion and [`is_tombstone_stable`](Self::is_tombstone_stable) never completes. Re-acking
    /// held tombstones every round — riding the broadcast `send_buf` to the same geography-throttled
    /// targets as the diff itself — makes the ack matrix converge transitively. It also dissolves
    /// the ack-before-tombstone ordering hazard: a receiver that does not hold the tombstone yet
    /// drops the ack (the admission gate in `handle_messages`, which records an ack only for a
    /// locally-held tombstone) and simply records it on a later round.
    ///
    /// The datagram is kept bounded: at most [`REACK_BYTE_BUDGET`] bytes of acks are appended, and
    /// when more tombstones are held than fit, the window start advances with `round` so every
    /// tombstone is re-acked within a bounded number of rounds. The keys are sorted so that window
    /// is deterministic across rounds (`HashSet` iteration order is not).
    fn append_tombstone_reacks(&self, send_buf: &mut Vec<u8>, round: u32) -> usize {
        let mut keys: Vec<K> = self.live_tombstones.read().iter().cloned().collect();
        if keys.is_empty() {
            return 0;
        }
        keys.sort_unstable();
        let n = keys.len();
        let budget = send_buf.len() + REACK_BYTE_BUDGET;
        let start = (round as usize) % n;
        let map_guard = self.map.read();
        let mut appended = 0;
        let mut budget_truncated = false;
        for offset in 0..n {
            if send_buf.len() >= budget {
                budget_truncated = true;
                break;
            }
            let key = &keys[(start + offset) % n];
            // Re-confirm against the map: the tombstone may have been resurrected or GC'd since we
            // snapshotted the index, and only the live tombstone's version is a valid ack.
            if let Some(v) = map_guard.get(key).filter(|v| v.is_tombstone()) {
                Message::Ack::<K, V, V::Projected>((key.clone(), version_hash(v)))
                    .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
                    .expect("serializing an Ack into an in-memory buffer cannot fail");
                appended += 1;
            }
        }
        if budget_truncated {
            trace!(
                "re-acked {appended}/{n} held tombstones this round (datagram byte budget reached); \
                 the remainder rotates in on subsequent rounds"
            );
        }
        observability::record_tombstone_reacks(appended);
        appended
    }

    /// Handle the messages contained in an already-authenticated [`Payload`].
    ///
    /// Taking a [`auth::Payload`] rather than raw bytes makes it structurally impossible to
    /// process a datagram that has not cleared the authentication gate.
    ///
    /// Returns `true` if the datagram contained at least one **dated** protocol message
    /// (`ComparisonItem` / `Update` / `Ack`). The caller uses this to decide whether to register
    /// the sender in the causal-stability membership set: a sender that spoke only the value-only channel is a
    /// read-only mirror and must not gate tombstone GC.
    #[instrument(name = "reconcile.handle", skip_all, fields(peer = %peer))]
    async fn handle_messages(
        &self,
        payload: auth::Payload<'_>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) -> bool {
        let timer = observability::timer();
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
                    observability::record_datagram_dropped("malformed");
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
            let map_guard = self.map.read();
            let mut guard = self.tombstone_acks.write();
            for (key, version) in acks {
                // Accept an ack only for a key we locally hold as a tombstone. Acks for
                // never-existent or non-tombstone keys are dropped here so they cannot accumulate
                // in `tombstone_acks` without bound.
                //
                // This gate cannot tell "a key we will never hold" from "a key we don't hold YET":
                // in a cluster of three or more nodes an ack can arrive before the deletion it
                // acknowledges (the acking peer learned the deletion via a different gossip edge),
                // and the dropped ack is not re-delivered by the sender. That is benign because
                // every node re-acknowledges the tombstones it holds on every reconciliation round
                // (see `start_reconciliation`): once we hold the tombstone, the next round's re-ack
                // records the peer's acknowledgment. Pairwise reciprocation is no longer needed —
                // the periodic re-ack is what makes the ack matrix complete across three or more
                // replicas.
                if map_guard.get(&key).is_some_and(|v| v.is_tombstone()) {
                    guard.entry(key).or_default().insert(peer_ip, version);
                } else {
                    trace!(
                        "dropped ack from {peer_ip} for key with no local tombstone; \
                         ignoring to prevent unbounded bookkeeping"
                    );
                }
            }
        }
        // handle messages
        if !in_comparison.is_empty() {
            debug!("received {} segments", in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.map.read();
                proto::diff_round(&guard, in_comparison, &mut out_comparison, &mut differences);
            }
            // Refinement comparison items are small and latency-sensitive: send them inline, now.
            if !out_comparison.is_empty() {
                debug!("returning {} segments", out_comparison.len());
                trace!("segments: {out_comparison:?}");
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::ComparisonItem::<K, V, V::Projected>)
                    .collect();
                send_messages_to(
                    &messages,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &self.sender_counter,
                    &peer,
                    send_buf,
                )
                .await;
            }
            // The differing values are the bulk payload — a cold/empty peer pulls the whole dataset
            // here. Hand them to a rate-paced background task so the burst cannot overrun the
            // receiver and the receive loop stays free for other peers.
            if !differences.is_empty() {
                debug!("returning {} diff_ranges", differences.len());
                trace!("diff_ranges: {differences:?}");
                // Claim both slots (per-peer + global budget) *before* snapshotting the range
                // into a Vec. A skipped dump allocates nothing; the peer re-initiates on its
                // next diff round once a slot is free.
                if let Some((peer_guard, global_guard)) = self.try_claim_dump_slot(peer) {
                    let updates: Vec<Message<K, V, V::Projected>> = {
                        let guard = self.map.read();
                        let mut updates = Vec::new();
                        for range in differences {
                            for (k, v) in guard.get_range(&range) {
                                updates.push(Message::Update((k.clone(), v.clone())));
                            }
                        }
                        updates
                    };
                    if !updates.is_empty() {
                        self.spawn_paced_send(updates, peer, peer_guard, global_guard);
                    }
                    // If updates is empty the guards drop here, releasing both slots.
                }
            }
        }
        if !updates.is_empty() {
            debug!("received {} updates", updates.len());
            observability::record_updates_received(updates.len());
            // Tombstones we now hold as a result of these updates, to be acknowledged back to
            // the peer so it can eventually garbage-collect them once causally stable.
            let mut acks_to_send = Vec::new();
            // 1) Under a read lock, decide which merged values would actually change state. We must
            //    NOT run the pre-insert hook here: hooks are contractually executed *outside* the
            //    map's write lock (matching `just_insert`), so a hook that re-inserts cannot
            //    re-enter the lock and deadlock.
            let mut to_apply: Vec<(K, V)> = Vec::new();
            {
                let guard = self.map.read();
                for (k, remote_v) in updates {
                    // Advance our clock past the timestamp carried by the remote value, so a
                    // later local write is ordered after everything we have seen. This is
                    // what prevents lost updates under clock skew.
                    self.clock.observe(remote_v.timestamp());
                    match guard.get(&k) {
                        Some(local_v) => {
                            let merged_v = local_v.reconcile(&remote_v);
                            if merged_v != *local_v {
                                to_apply.push((k, merged_v));
                            } else if local_v.is_tombstone() {
                                // We already hold an equal-or-newer value; still acknowledge it
                                // if it is the same tombstone, so the peer learns we have it.
                                acks_to_send.push(Message::Ack::<K, V, V::Projected>((
                                    k,
                                    version_hash(local_v),
                                )));
                            }
                        }
                        None => to_apply.push((k, remote_v)),
                    }
                }
            }
            // 2) Run the pre-insert hooks with no lock held, exactly as `just_insert` does.
            for (k, v) in &to_apply {
                (self.pre_insert.read())(k, v);
            }
            // 3) Re-acquire the write lock and apply. We re-reconcile against the now-current local
            //    value because the lock was released between steps 1 and 3: a concurrent write (for
            //    instance one performed by the hook itself) may have installed a newer value, which
            //    LWW must not clobber. `reconcile` is idempotent `max` over the timestamp total
            //    order, so re-applying a value that became stale meanwhile is a no-op — the stale
            //    case is handled identically to the direct path.
            if !to_apply.is_empty() {
                let mut guard = self.map.write();
                for (k, v) in to_apply {
                    let merged_v = match guard.get(&k) {
                        Some(local_v) => local_v.reconcile(&v),
                        None => v,
                    };
                    let version = merged_v.is_tombstone().then(|| version_hash(&merged_v));
                    self.map_insert(&mut guard, k.clone(), merged_v);
                    if let Some(version) = version {
                        acks_to_send.push(Message::Ack::<K, V, V::Projected>((k, version)));
                    }
                }
            }
            if !acks_to_send.is_empty() {
                send_messages_to(
                    &acks_to_send,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &self.sender_counter,
                    &peer,
                    send_buf,
                )
                .await;
            }
        }
        // Value-only channel: answer a dateless mirror by diffing against the value-only
        // *projection* tree (never the dated map) and replying with `ValueUpdate`s carrying only
        // the projected payload. This path is entirely independent of the dated channel and of the
        // causal-stability state — no acks, no membership, no GC interaction.
        if !value_in_comparison.is_empty() {
            debug!("received {} value-only segments", value_in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.projection.read();
                proto::diff_round(
                    &guard,
                    value_in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // Refinement comparison items are small and latency-sensitive: send them inline, now.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::ValueComparisonItem::<K, V, V::Projected>)
                    .collect();
                send_messages_to(
                    &messages,
                    Arc::clone(&self.socket),
                    &self.authenticator,
                    &self.sender_counter,
                    &peer,
                    send_buf,
                )
                .await;
            }
            // Bulk value-only payload — a dateless mirror pulling the dataset. Rate-pace it on a
            // background task, exactly like the dated bulk path.
            if !differences.is_empty() {
                if let Some((peer_guard, global_guard)) = self.try_claim_dump_slot(peer) {
                    let updates: Vec<Message<K, V, V::Projected>> = {
                        let guard = self.projection.read();
                        let mut updates = Vec::new();
                        for range in differences {
                            for (k, p) in guard.get_range(&range) {
                                updates.push(Message::ValueUpdate((k.clone(), p.clone())));
                            }
                        }
                        updates
                    };
                    if !updates.is_empty() {
                        self.spawn_paced_send(updates, peer, peer_guard, global_guard);
                    }
                }
            }
        }
        observability::record_handle_duration(timer);
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
    ///
    /// Per-peer replay state is intentionally **not** evicted: a replay attacker who captured a
    /// datagram from this peer while it was active and replays it within the freshness window
    /// would otherwise bypass the bitmap duplicate check (the entry was removed) and re-add the
    /// peer to membership. Keeping the replay state lets the bitmap reject the captured datagram.
    pub(crate) fn decommission_peer(&self, peer: IpAddr) {
        self.members.write().remove(&peer);
        self.peers.write().remove(&peer);
        for key_acks in self.tombstone_acks.write().values_mut() {
            key_acks.remove(&peer);
        }
    }

    /// Register (or refresh) a known peer at runtime, the `&self` counterpart of the
    /// [`with_seed`](crate::ReconcileStore::with_seed) builder.
    ///
    /// Inserting into [`peers`](Self::peers) (re)arms the `PEER_EXPIRATION` window and makes the
    /// address a gossip target on the next round. This is what a dynamic discovery source (e.g.
    /// DNS-based Kubernetes discovery) calls each round for every resolved peer. It deliberately
    /// does **not** touch [`members`](Self::members): membership — which gates tombstone GC — must
    /// still be *earned* by a genuine authenticated, dated datagram, so an unverified discovered
    /// address can never block garbage collection.
    pub(crate) fn seed_peer(&self, peer: IpAddr) {
        self.peers.write().insert(peer, Instant::now());
    }

    /// A snapshot of the monotonic membership set (peers that gate tombstone GC).
    pub(crate) fn members_snapshot(&self) -> HashSet<IpAddr> {
        self.members.read().clone()
    }

    /// Number of entries currently in the peers gossip-routing map.
    ///
    /// Exposed for test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub(crate) fn peers_map_len(&self) -> usize {
        self.peers.read().len()
    }

    /// Number of entries currently in the per-peer replay filter.
    ///
    /// Exposed for test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub(crate) fn replay_filter_len(&self) -> usize {
        self.replay_filter.len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub(crate) fn tombstone_acks_len(&self) -> usize {
        self.tombstone_acks.read().len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for test assertions under the `internal-testing` feature gate.
    #[cfg(any(test, feature = "internal-testing"))]
    pub(crate) fn bulk_dumps_in_flight_count(&self) -> usize {
        self.bulk_dumps_in_flight.load(Ordering::Acquire)
    }

    /// This node's configured listen address, used by discovery to never decommission itself.
    pub(crate) fn listen_addr(&self) -> IpAddr {
        self.listen_addr
    }
}

pub(crate) async fn send_to_retry<A: ToSocketAddrs>(
    socket: &UdpSocket,
    authenticator: &auth::Authenticator,
    sender_counter: &replay::SenderCounter,
    buf: &[u8],
    target: A,
) -> std::io::Result<usize> {
    // Allocate a sequence number and stamp, then frame the datagram once and reuse it across
    // retries. When authentication is disabled, `seal` returns `None` and the wire bytes are
    // identical to the unauthenticated behavior.
    let seq = sender_counter.next_seq();
    let stamp = sender_counter.next_stamp();
    let framed = authenticator.seal(seq, stamp, buf);
    let wire: &[u8] = framed.as_deref().unwrap_or(buf);
    let mut res = Ok(0);
    for _ in 0..MAX_SENDTO_RETRIES {
        res = socket.send_to(wire, &target).await;
        if res.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    match &res {
        Ok(sent) => observability::record_bytes_sent(*sent),
        Err(err) => {
            error!("send_to failed after {MAX_SENDTO_RETRIES} retries: {err}");
            observability::record_send_failure();
        }
    }
    res
}

/// Send `messages` to `peer` back-to-back (unpaced).
///
/// Used for the small, latency-sensitive batches — refinement comparison items, tombstone acks, and
/// local-write broadcasts. The bulk anti-entropy value dump uses the paced variant instead;
/// see [`send_messages_paced`] and [`ReconcileEngine::spawn_paced_send`].
pub(crate) async fn send_messages_to<K: Serialize, V: Serialize, P: Serialize>(
    messages: &[Message<K, V, P>],
    socket: Arc<UdpSocket>,
    authenticator: &auth::Authenticator,
    sender_counter: &replay::SenderCounter,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
) {
    send_messages_paced(
        messages,
        socket,
        authenticator,
        sender_counter,
        peer,
        send_buf,
        None,
    )
    .await
}

/// Send `messages` to `peer` as ≤64 KiB datagrams, optionally metered to `rate` bytes/sec.
///
/// With `rate = None` the datagrams go out back-to-back (the historical behaviour). With
/// `rate = Some(bytes_per_sec)` the function sleeps between datagrams so the average send rate stays
/// at or below that bound — used for bulk anti-entropy value transfers so the burst cannot overrun
/// the receiver's socket buffer. Because it sleeps, it must run **off** the receive
/// loop (see [`ReconcileEngine::spawn_paced_send`]); pacing inline in `handle_messages` would stall
/// reception of every other peer for the duration of the transfer.
#[instrument(name = "reconcile.send", skip_all, fields(peer = %peer, count = messages.len()))]
pub(crate) async fn send_messages_paced<K: Serialize, V: Serialize, P: Serialize>(
    messages: &[Message<K, V, P>],
    socket: Arc<UdpSocket>,
    authenticator: &auth::Authenticator,
    sender_counter: &replay::SenderCounter,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
    rate: Option<usize>,
) {
    debug!("sending {} messages to {peer}", messages.len());
    // Reserve room for the authentication tag so the sealed datagram still fits a UDP payload.
    let max_payload = BUFFER_SIZE - authenticator.overhead();
    send_buf.clear();
    // Anchor the pacing schedule once, so it self-corrects rather than drifting per datagram.
    let start = Instant::now();
    let mut sent_bytes: usize = 0;
    for message in messages {
        let last_size = send_buf.len();
        message
            .serialize(&mut Serializer::new(&mut *send_buf, DefaultOptions::new()))
            .expect("serializing a protocol Message into an in-memory buffer cannot fail");
        if send_buf.len() > max_payload {
            trace!("sending {} bytes to {peer}", last_size);
            if let Err(err) = send_to_retry(
                &socket,
                authenticator,
                sender_counter,
                &send_buf[..last_size],
                peer,
            )
            .await
            {
                warn!("failed to send datagram to {peer}: {err}; continuing");
            } else {
                trace!("sent {} bytes to {peer}", last_size);
            }
            send_buf.drain(..last_size);
            sent_bytes += last_size;
            pace(rate, start, sent_bytes).await;
        }
    }
    trace!("sending last {} bytes to {peer}", send_buf.len());
    if let Err(err) = send_to_retry(&socket, authenticator, sender_counter, send_buf, peer).await {
        warn!("failed to send final datagram to {peer}: {err}; continuing");
    } else {
        trace!("sent last {} bytes to {peer}", send_buf.len());
    }
}

/// Sleep, if necessary, so that having sent `sent_bytes` since `start` does not exceed `rate`
/// bytes/sec. A `None` (or zero) rate is a no-op. The schedule is anchored to `start`, so it
/// self-corrects and does not drift; the caller does not pace after the final datagram.
async fn pace(rate: Option<usize>, start: Instant, sent_bytes: usize) {
    let Some(rate) = rate.filter(|&r| r > 0) else {
        return;
    };
    let expected = Duration::from_secs_f64(sent_bytes as f64 / rate as f64);
    if let Some(delay) = expected.checked_sub(start.elapsed()) {
        sleep(delay).await;
    }
}

/// RAII marker that a bulk dump to `peer` is in flight. Removing the peer on `Drop` —
/// rather than at the end of the send task's body — guarantees the in-flight mark is cleared even if
/// the send task panics, so a transient failure can never wedge a peer into "permanently
/// transferring" (which would silently stop it from ever syncing again). See
/// [`ReconcileEngine::spawn_paced_send`].
struct BulkInFlightGuard {
    set: Arc<RwLock<HashSet<SocketAddr>>>,
    peer: SocketAddr,
}

impl Drop for BulkInFlightGuard {
    fn drop(&mut self) {
        self.set.write().remove(&self.peer);
    }
}

/// RAII counter-decrement for the global concurrent-dump budget. Decrements the shared atomic on
/// `Drop`, guaranteeing the slot is freed even if the task holding it panics or is aborted. See
/// [`ReconcileEngine::try_claim_dump_slot`].
struct BulkDumpCountGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for BulkDumpCountGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(test)]
mod deadlock_regressions {

    use crate::auth;
    use crate::clock::Timestamp;
    use crate::reconcilable::ValueOnly;
    use crate::reconcile_engine::ReconcileEngine;
    use crate::{reconcile_store::Config, ReconcileStore};
    use bincode::{DefaultOptions, Serializer};
    use serde::Serialize;
    use std::net::SocketAddr;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    use std::time::Duration;

    use super::Message;

    #[tokio::test(flavor = "multi_thread")]
    async fn pre_insert_hook_can_call_insert_again_without_deadlock() {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr("127.0.0.44".parse().unwrap());
        // let tree = HRTree::from_iter(vec![(1, 10), (2, 20)]);
        let svc = ReconcileStore::new(config).await.expect("bind failed");
        svc.insert_bulk(&[(1, 10_u8)]);

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

    /// Serialize an `Update` so it can be fed straight into the engine's network ingest path
    /// (`handle_messages`), exactly as a peer's datagram would arrive once the (disabled)
    /// authentication gate has been cleared.
    fn update_message_bytes(key: i32, value: (Timestamp, Option<u8>)) -> Vec<u8> {
        let message = Message::Update::<i32, (Timestamp, Option<u8>), ValueOnly<u8>>((key, value));
        let mut buf = Vec::new();
        message
            .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
            .unwrap();
        buf
    }

    /// Network-path counterpart of [`pre_insert_hook_can_call_insert_again_without_deadlock`]:
    /// a value arriving over the wire is integrated by [`handle_messages`], which must
    /// run the pre-insert hook *outside* the map's write lock, just like the direct `just_insert`
    /// path. Otherwise a hook that re-inserts re-enters the (non-reentrant) write lock and deadlocks
    /// the receive loop.
    ///
    /// Unlike the direct-path test above (which can assert synchronously), the regression here is a
    /// deadlock that would *hang* the task processing the datagram and, with it, runtime teardown.
    /// So the whole scenario runs on a dedicated thread owning a current-thread runtime, reporting
    /// over a channel; the test body waits with `recv_timeout`, failing fast (and merely leaking the
    /// wedged thread) instead of hanging the runner if the fix ever regresses.
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
                    .with_listen_addr("127.0.0.50".parse().unwrap());
                let engine = ReconcileEngine::<i32, (Timestamp, Option<u8>)>::new(config)
                    .await
                    .expect("bind failed");

                // The same re-entrant hook as the direct-path test, registered on the engine whose
                // map `handle_messages` writes to: it calls back into `just_insert` on that engine.
                // Were `handle_messages` still running the hook under `map.write()`, this inner
                // insert would re-enter the write lock and deadlock.
                let hook_engine = engine.clone();
                let once = Arc::new(AtomicBool::new(false));
                let guard = once.clone();
                *engine.pre_insert.write() =
                    Box::new(move |&k: &i32, v: &(Timestamp, Option<u8>)| {
                        if !guard.swap(true, Ordering::SeqCst) {
                            let inner = (
                                Timestamp::new(u64::MAX, 1, 0),
                                Some(v.1.unwrap_or_default() + 100),
                            );
                            let _ = hook_engine.just_insert(k + 100, inner);
                        }
                    });

                // Feed an `Update` (future timestamp, so it is integrated) through the real network
                // ingest path. No cluster key, so `open` clears the bytes unchanged into a `Payload`.
                let bytes = update_message_bytes(42, (Timestamp::new(u64::MAX, 0, 0), Some(99)));
                let payload = auth::Authenticator::new(None, false)
                    .open(&bytes)
                    .expect("unauthenticated mode clears any datagram");
                let peer: SocketAddr = "127.0.0.51:8083".parse().unwrap();
                let mut send_buf = Vec::new();
                engine.handle_messages(payload, peer, &mut send_buf).await;

                // Read back the value the re-entrant hook inserted from the network path. The read
                // guard is dropped explicitly so it does not outlive `engine` at the block's end.
                let map_guard = engine.map.read();
                let reinserted = map_guard.get(&142).and_then(|(_, v)| *v);
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

    use super::Message;
    use crate::clock::Timestamp;
    use crate::{auth, reconcile_store::Config, ReconcileStore};

    /// Serialize the F3 attack payload: an `Update` with a far-future timestamp that, if merged,
    /// would win against every legitimate write forever.
    fn forged_update() -> Vec<u8> {
        let far_future = Timestamp::new(u64::MAX, 0, 0);
        let message = Message::Update::<
            i32,
            (Timestamp, Option<String>),
            crate::reconcilable::ValueOnly<String>,
        >((0, (far_future, Some("evil".to_string()))));
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
        let store = ReconcileStore::<i32, String>::new(config)
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
        let wrong_key_sealed = auth::Authenticator::new(Some([0x99u8; auth::KEY_LEN]), false)
            .seal(1, Utc::now().timestamp_millis().max(0) as u64, &forged)
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

    use bincode::{DefaultOptions, Deserializer};
    use serde::Deserialize;

    use crate::clock::Timestamp;
    use crate::reconcilable::ValueOnly;
    use crate::reconcile_engine::{version_hash, Message, ReconcileEngine};
    use crate::reconcile_store::Config;

    type Tombstoned = (Timestamp, Option<i32>);

    async fn engine(addr: &str) -> ReconcileEngine<i32, Tombstoned> {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr(addr.parse().unwrap());
        ReconcileEngine::new(config).await.expect("bind failed")
    }

    #[tokio::test]
    async fn tombstone_not_stable_until_all_members_ack() {
        let eng = engine("127.0.0.60").await;
        let peer_a: IpAddr = "127.0.0.61".parse().unwrap();
        let peer_b: IpAddr = "127.0.0.62".parse().unwrap();
        let key = 7;
        let tombstone: Tombstoned = (Timestamp::new(1, 0, 0), None);
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

    /// Every reconciliation round must re-emit an `Ack` for each tombstone the node currently holds,
    /// so the causal-stability ack matrix keeps converging at three or more nodes (where a held
    /// tombstone is otherwise never re-advertised once two replicas agree on it).
    /// Deterministic, socket-free: insert a tombstone, run one round, and inspect the datagram.
    #[tokio::test]
    async fn reconciliation_round_reacks_held_tombstones() {
        let eng = engine("127.0.0.66").await;
        let live_key = 1;
        let tombstone_key = 2;
        // A live value must NOT be re-acked; a tombstone must be.
        eng.just_insert(live_key, (Timestamp::new(1, 0, 0), Some(11)));
        let tombstone: Tombstoned = (Timestamp::new(2, 0, 0), None);
        eng.just_insert(tombstone_key, tombstone);
        let expected_version = version_hash(&tombstone);

        let mut buf = Vec::new();
        eng.start_reconciliation(&mut buf).await;

        // Decode every message in the datagram and collect the acks.
        let mut acks: Vec<(i32, u64)> = Vec::new();
        let mut de = Deserializer::from_slice(&buf, DefaultOptions::new());
        while let Ok(msg) = Message::<i32, Tombstoned, ValueOnly<i32>>::deserialize(&mut de) {
            if let Message::Ack(ack) = msg {
                acks.push(ack);
            }
        }

        assert!(
            acks.contains(&(tombstone_key, expected_version)),
            "the round must re-ack the held tombstone (key {tombstone_key}); got {acks:?}"
        );
        assert!(
            !acks.iter().any(|(k, _)| *k == live_key),
            "a live value must never be re-acked; got {acks:?}"
        );
    }

    #[tokio::test]
    async fn decommission_releases_a_silent_peer() {
        let eng = engine("127.0.0.63").await;
        let live: IpAddr = "127.0.0.64".parse().unwrap();
        let gone: IpAddr = "127.0.0.65".parse().unwrap();
        let key = 9;
        let tombstone: Tombstoned = (Timestamp::new(1, 0, 0), None);
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

#[cfg(test)]
mod clock_port {
    use std::sync::Arc;

    use crate::clock::{ManualClock, Timestamp};
    use crate::reconcile_engine::ReconcileEngine;
    use crate::reconcile_store::Config;

    /// The engine mints timestamps only through the injected [`Clock`](crate::clock::Clock) port, so
    /// a deterministic adapter makes `clock_now()` fully reproducible — no wall-clock time involved.
    /// This is the engine-level testability the port exists to provide.
    #[tokio::test]
    async fn engine_mints_through_the_injected_clock() {
        let config = Config::default()
            .with_port(8080)
            .with_listen_addr("127.0.0.70".parse().unwrap());
        let clock = Arc::new(ManualClock::new(42));
        let eng: ReconcileEngine<i32, (Timestamp, Option<i32>)> =
            ReconcileEngine::new_with_clock(config, clock)
                .await
                .expect("bind failed");

        assert_eq!(eng.clock_now(), Timestamp::new(0, 1, 42));
        assert_eq!(eng.clock_now(), Timestamp::new(0, 2, 42));
    }
}

#[cfg(test)]
mod pacing {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::net::UdpSocket;

    use crate::auth::Authenticator;
    use crate::reconcilable::ValueOnly;
    use crate::reconcile_engine::{send_messages_paced, Message};

    type Msg = Message<u64, Vec<u8>, ValueOnly<u8>>;

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
        let authenticator = Authenticator::new(None, false);
        let sender_counter = crate::replay::SenderCounter::new();
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap(); // discard port
        let mut send_buf = Vec::new();
        let start = Instant::now();
        send_messages_paced(
            messages,
            socket,
            &authenticator,
            &sender_counter,
            &peer,
            &mut send_buf,
            rate,
        )
        .await;
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
}

#[cfg(test)]
mod socket_buffers {
    use socket2::SockRef;

    use crate::clock::Timestamp;
    use crate::reconcile_engine::ReconcileEngine;
    use crate::reconcile_store::Config;

    type Tombstoned = (Timestamp, Option<i32>);

    async fn engine(addr: &str, config: Config) -> ReconcileEngine<i32, Tombstoned> {
        ReconcileEngine::new(config.with_listen_addr(addr.parse().unwrap()))
            .await
            .expect("bind failed")
    }

    /// The receive-buffer knob must actually size the gossip socket. Asserting an
    /// absolute byte count would be flaky — the kernel clamps `SO_RCVBUF` to `net.core.rmem_max`,
    /// which varies per host (and CI sandbox) — so we assert *monotonicity* instead: the multi-MiB
    /// default must yield a strictly larger buffer than an explicitly tiny request. That holds for
    /// any `rmem_max` above a few KiB, which is true on every realistic host.
    #[tokio::test]
    async fn recv_buffer_size_is_configurable() {
        // Default config requests the multi-MiB DEFAULT_SOCKET_BUFFER_SIZE.
        let big = engine("127.0.0.90", Config::default()).await;
        // An explicit, tiny request — well below any plausible rmem_max, so it is honoured as-is.
        let small = engine(
            "127.0.0.91",
            Config::default().with_recv_buffer_size(8 * 1024),
        )
        .await;

        let big_buf = SockRef::from(&*big.socket).recv_buffer_size().unwrap();
        let small_buf = SockRef::from(&*small.socket).recv_buffer_size().unwrap();

        assert!(
            big_buf > small_buf,
            "the multi-MiB default ({big_buf} B) should exceed an explicitly tiny buffer \
             ({small_buf} B)"
        );
    }

    /// `None` opts out of the tuning entirely, leaving the inherited OS default. Combined with the
    /// test above (the default is large), this pins down both ends of the knob: a tiny explicit
    /// request shrinks the buffer below the large default.
    #[tokio::test]
    async fn recv_buffer_size_none_leaves_os_default() {
        let config = Config {
            recv_buffer_size: None,
            ..Config::default()
        };
        let eng = engine("127.0.0.92", config).await;
        // Just assert it builds and the socket is usable; the OS default is host-dependent, so we
        // only check the call path does not panic and yields a positive buffer.
        let buf = SockRef::from(&*eng.socket).recv_buffer_size().unwrap();
        assert!(buf > 0, "a socket always has a positive receive buffer");
    }
}

#[cfg(test)]
mod tombstone_ack_bounds {
    use std::net::SocketAddr;

    use bincode::{DefaultOptions, Serializer};
    use serde::Serialize;

    use super::Message;
    use crate::auth;
    use crate::clock::Timestamp;
    use crate::reconcile_engine::{version_hash, ReconcileEngine};
    use crate::reconcile_store::Config;

    type Tombstoned = (Timestamp, Option<i32>);

    async fn engine(addr: &str) -> ReconcileEngine<i32, Tombstoned> {
        // Use a distinct port per module to avoid bind conflicts between parallel test runs.
        let config = Config::default()
            .with_port(0)
            .with_listen_addr(addr.parse().unwrap());
        ReconcileEngine::new(config).await.expect("bind failed")
    }

    fn ack_bytes(key: i32, version: u64) -> Vec<u8> {
        let msg =
            Message::Ack::<i32, Tombstoned, crate::reconcilable::ValueOnly<i32>>((key, version));
        let mut buf = Vec::new();
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
            .expect("unauthenticated open");
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
        // Insert a live value (Some(_) = not a tombstone).
        eng.just_insert(key, (Timestamp::new(1, 0, 0), Some(42)));

        let peer: SocketAddr = "127.0.0.96:9000".parse().unwrap();
        let bytes = ack_bytes(key, 123);
        let payload = auth::Authenticator::new(None, false)
            .open(&bytes)
            .expect("unauthenticated open");
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
        let tombstone: Tombstoned = (Timestamp::new(2, 0, 0), None);
        let version = version_hash(&tombstone);
        eng.just_insert(key, tombstone);

        let peer: SocketAddr = "127.0.0.98:9000".parse().unwrap();
        let bytes = ack_bytes(key, version);
        let payload = auth::Authenticator::new(None, false)
            .open(&bytes)
            .expect("unauthenticated open");
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
    use crate::reconcile_store::Config;

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
        use crate::clock::Timestamp;
        use crate::reconcile_engine::ReconcileEngine;

        type Tombstoned = (Timestamp, Option<i32>);

        let config = Config::default()
            .with_port(0)
            .with_listen_addr("127.0.0.99".parse().unwrap())
            .with_max_concurrent_bulk_dumps(1);
        let eng = ReconcileEngine::<i32, Tombstoned>::new(config)
            .await
            .expect("bind failed");

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
