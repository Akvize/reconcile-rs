// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`Replica`], the inner layer of the [`ReplicatedMap`](crate::replicated_map::ReplicatedMap)
//! that handles communication between instances at the network level.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeBounds;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::bounds::{Key, Value};
use crate::clock::{Clock, HlcClock, NodeId, Timestamp};
use crate::discovery::{Discovery, RandomProbe};
use crate::entry::{Entry, State};
use crate::observability;
use crate::replicated_map::{Config, ConfigError, MAX_NETS, MIN_BULK_SEND_RATE};
use crate::transport::{Transport, UdpTransport};
use crate::FingerprintTreeMap;
use gossip::auth;
use gossip::gen_ip::{host_net, net_of};
use gossip::replay;
use rbsr::RangeAggregate;
use rsos::Fingerprint;

const BUFFER_SIZE: usize = 65507;
/// Upper bound on protocol messages decoded from a single datagram. A datagram is at most
/// [`BUFFER_SIZE`] bytes and the smallest message is at least one byte, so it can never legitimately
/// contain more than this many messages; the cap turns a crafted datagram's decode-expansion into a
/// bounded operation rather than an unbounded allocation.
pub(crate) const MAX_MESSAGES_PER_DATAGRAM: usize = BUFFER_SIZE;
const PEER_EXPIRATION: Duration = Duration::from_secs(60);
/// Byte budget for the tombstone ack resends piggybacked onto each reconciliation datagram. Kept
/// well under [`BUFFER_SIZE`] so the datagram still fits after authentication framing; when more
/// tombstones are held than fit in one round, a round-advancing window covers the remainder on
/// subsequent rounds.
const TOMBSTONE_ACK_RESEND_BYTE_BUDGET: usize = 8 * 1024;

const MAX_SENDTO_RETRIES: u32 = 4;

type PreInsertCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &V)>;

/// A deterministic, cross-node version token for a value: the low 64 bits of `rsos::digest`
/// (`ARCHITECTURE.md` §5 invariant 7).
///
/// A peer acknowledges the exact tombstone version it holds, so a stale ack cannot authorize GC of
/// a newer one.
pub(crate) fn version_hash<V: Serialize>(value: &V) -> u64 {
    rsos::digest(value).0[0]
}

/// Hard cap on distinct tracked peers, owning the one admission rule both receive loops share: a
/// sender is admitted while already known, or while the count is under the cap. Sourced from
/// [`Config::max_peers`](crate::replicated_map::Config::max_peers).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerCap(usize);

impl PeerCap {
    pub(crate) fn new(max_peers: usize) -> Self {
        PeerCap(max_peers)
    }

    /// Whether a datagram from a sender should be admitted, given whether the sender is already
    /// tracked and how many distinct peers are currently tracked.
    pub(crate) fn admits(self, known: bool, current_len: usize) -> bool {
        known || current_len < self.0
    }

    pub(crate) fn max(self) -> usize {
        self.0
    }
}

/// Derive a node's **local network** — whichever declared net contains `listen_addr`, and the one
/// reconciled with every round.
///
/// With no match, falls back to the node's own host route, so every peer is treated as remote
/// rather than mis-qualified. Called at construction and on each `nets` mutation, so the warning
/// fires only there.
fn derive_local_net(nets: &[IpNet], listen_addr: IpAddr) -> IpNet {
    net_of(nets, listen_addr).unwrap_or_else(|| {
        warn!(
            "listen address {listen_addr} is contained in none of the configured networks \
             {nets:?}; cannot identify the local network — treating only this node as local, so \
             every peer is remote and reconciled on the throttled cross-network cadence. Declare \
             the network containing {listen_addr} via Config::with_net or ReplicatedMap::add_net.",
        );
        host_net(listen_addr)
    })
}

/// The internal reconciliation engine at the network level; removals are the outer layer's
/// ([`ReplicatedMap`](crate::replicated_map::ReplicatedMap)).
///
/// `V` is the plain user value type, stored and exchanged as
/// [`Entry<Timestamp, V>`](crate::entry::Entry) (`ARCHITECTURE.md` §4). [`Transport`] is held as a
/// trait object; encoding goes through [`gossip::bincode`], so no codec parameter appears.
pub(crate) struct Replica<K, V> {
    /// All engine state behind one [`Arc`], so a clone is a refcount bump. Fields are reached
    /// through the [`Deref`] impl below.
    inner: Arc<Inner<K, V>>,
}

/// Shared, refcounted state of a [`Replica`]; see that struct for the rationale.
pub(crate) struct Inner<K, V> {
    pub(crate) map: Arc<RwLock<FingerprintTreeMap<K, Entry<Timestamp, V>>>>,
    /// Value-only **projection** of [`map`](Self::map), kept in sync at every mutation.
    ///
    /// Timestamp-less by construction (`ARCHITECTURE.md` §5 invariant 8), which is what lets a
    /// dateless read replica converge with this dated store. Never touches causal stability.
    pub(crate) projection: Arc<RwLock<FingerprintTreeMap<K, State<V>>>>,
    port: u16,
    /// The datagram-I/O port (default adapter: [`UdpTransport`]). The engine sends and receives
    /// only through this, so it never names a concrete socket type.
    transport: Arc<dyn Transport<Addr = SocketAddr>>,
    /// The geographical networks the cluster spans. One random probe per network per round; a
    /// peer's net comes from its IP. Mutable at runtime, snapshotted per round.
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
    /// `None` to send back-to-back. Mirrors [`Config::bulk_send_rate`](crate::replicated_map::Config::bulk_send_rate)
    /// and is read by [`spawn_paced_send`](Self::spawn_paced_send).
    bulk_send_rate: Option<usize>,
    /// Peers with a bulk transfer in flight: at most one paced dump per peer, or a re-firing
    /// reconcile timer would re-dump ranges still in transit. Cleared by an RAII guard.
    bulk_in_flight: Arc<RwLock<HashSet<SocketAddr>>>,
    /// Bulk dump tasks in flight across all peers. At
    /// [`max_concurrent_bulk_dumps`](Self::max_concurrent_bulk_dumps) a new dump is skipped before
    /// its snapshot is allocated; the peer's next diff round retries. Released by an RAII guard.
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
    pub(crate) pre_insert: Arc<RwLock<PreInsertCallback<K, Entry<Timestamp, V>>>>,
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
    /// Monotonic set of every peer this node has exchanged messages with; never expired, unlike
    /// [`peers`](Self::peers).
    ///
    /// This is the causal-stability gate on tombstone GC (`ARCHITECTURE.md` §5 invariant 6).
    /// Remote-net members ack on the slower cross-network cadence, so GC is slower there but no
    /// less correct.
    pub(crate) members: Arc<RwLock<HashSet<IpAddr>>>,
    /// Per-tombstone acknowledgments: `key -> (peer -> version token of the tombstone it holds)`.
    pub(crate) tombstone_acks: Arc<RwLock<HashMap<K, HashMap<IpAddr, u64>>>>,
    /// The keys held as tombstones, maintained at the single map mutation sink. Lets a round
    /// enumerate them without scanning the map and resend an ack for each — without which the ack
    /// matrix never completes past two nodes, acks being pairwise.
    pub(crate) live_tombstones: Arc<RwLock<HashSet<K>>>,
    /// This node's clock, reached only through the [`Clock`] port so the engine never reads
    /// physical time itself ([`HlcClock`] is the default adapter; a test injects a deterministic
    /// stub via [`new_with_clock`](Replica::new_with_clock)). Shared across all clones so
    /// that every write and every received timestamp advances the same clock.
    clock: Arc<dyn Clock>,
    /// `true` when the node id was generated at random (i.e. `Config::node_id` was `None`).
    /// Exposed via [`node_id_is_random`](Replica::node_id_is_random) so that the store
    /// layer can warn operators when persistence is configured but the identity is ephemeral.
    pub(crate) node_id_is_random: bool,
    /// Hard cap on the number of tracked remote peers. Datagrams from unknown senders are dropped
    /// before any per-sender state is allocated when the membership set reaches this size.
    max_peers: PeerCap,
}

impl<K, V> Clone for Replica<K, V> {
    fn clone(&self) -> Self {
        Replica {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> std::ops::Deref for Replica<K, V> {
    type Target = Inner<K, V>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// One atomic message of the reconciliation protocol.
///
/// Variant order is the wire tag order: the **dated** channel is 0-2
/// (`ComparisonItem`/`Update`/`Ack`), the **value-only** channel read replicas use is 3-4. A node
/// that does not know the latter fails to deserialize and drops the datagram, which the receive
/// loop tolerates.
///
/// `V` is the dated value, `P` its timestamp-less projection.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum Message<K: Serialize, V: Serialize, P: Serialize> {
    /// Provides information about a set of keys that allows checking
    /// whether there are differences between the two instances over this set
    ComparisonItem(RangeAggregate<K>),
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
    ValueComparisonItem(RangeAggregate<K>),
    /// An individual timestamp-less update sent to a dateless read replica once the value-only diff
    /// identified a difference. Carries the projected payload only (no [`Timestamp`]).
    ValueUpdate((K, P)),
}

/// Reject `config.port == 0` (#293): gossip does no per-peer port discovery, so every outbound
/// datagram to a peer is addressed to `config.port` literally. Port `0` binds an OS-assigned
/// ephemeral port for receiving, but that assigned port is never read back into the value peers
/// are addressed on — a node configured this way sends every peer datagram to port `0` and can
/// never converge with anything. Shared by [`Replica::bind_udp`] and
/// [`ReadReplicaMap::new`](crate::ReadReplicaMap::new), the two entry points that bind a real
/// socket; the in-memory-transport constructors are unaffected since their caller chooses the
/// port directly.
pub(crate) fn check_port_is_nonzero(config: &Config) -> io::Result<()> {
    if config.port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Config::port must be nonzero: gossip addresses every outbound datagram to this \
             port, so 0 (\"OS picks one\") can never converge — see Config::port's docs and \
             Config::new",
        ));
    }
    Ok(())
}

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Create an engine over the default [`UdpTransport`].
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
        // Default adapter for the `Clock` port: the chrono-backed Hybrid Logical Clock. This is the
        // only place the engine names a concrete clock; everything else goes through `dyn Clock`.
        let node_id_is_random = config.node_id.is_none();
        let node_id = config
            .node_id
            .unwrap_or_else(|| NodeId::new(rand::random()));
        let clock: Arc<dyn Clock> = Arc::new(HlcClock::new(node_id));
        let transport = Self::bind_udp(&config).await?;
        Ok(Self::build(config, transport, clock, node_id_is_random))
    }

    /// Construct an engine over the default [`UdpTransport`], but a caller-supplied [`Clock`]
    /// instead of [`HlcClock`]. A caller-supplied clock also implies a caller-supplied (stable)
    /// identity, so this marks it as non-random — see
    /// [`node_id_is_random`](Replica::node_id_is_random).
    ///
    /// The engine trusts `clock` completely, with no way to check it at the type level; see
    /// `ReplicatedMap::new_with_clock`'s docs (the public entry point this seam is reached
    /// through) for the full risk writeup and a worked example.
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
    pub async fn new_with_clock(config: Config, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let transport = Self::bind_udp(&config).await?;
        Ok(Self::build(config, transport, clock, false))
    }

    /// Construct an engine over a caller-supplied [`Transport`], with the default clock.
    ///
    /// Infallible: the only fallible step in [`new`](Self::new) is binding the UDP socket, which
    /// the caller has already done (or does not need to do at all).
    pub(crate) fn with_transport(
        config: Config,
        transport: Arc<dyn Transport<Addr = SocketAddr>>,
    ) -> Self {
        let node_id_is_random = config.node_id.is_none();
        let node_id = config
            .node_id
            .unwrap_or_else(|| NodeId::new(rand::random()));
        let clock: Arc<dyn Clock> = Arc::new(HlcClock::new(node_id));
        Self::build(config, transport, clock, node_id_is_random)
    }

    /// As [`with_transport`](Self::with_transport), but over a caller-supplied [`Clock`] too.
    /// Test-only seam: pairing an in-memory transport with a `ManualClock` makes convergence
    /// deterministic without real sockets or wall-clock time.
    #[cfg(test)]
    pub(crate) fn new_with_transport(
        config: Config,
        transport: Arc<dyn Transport<Addr = SocketAddr>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::build(config, transport, clock, false)
    }

    /// Bind the default [`UdpTransport`] for `config` and log the bound address.
    async fn bind_udp(config: &Config) -> io::Result<Arc<dyn Transport<Addr = SocketAddr>>> {
        check_port_is_nonzero(config)?;
        let transport = UdpTransport::bind(
            SocketAddr::new(config.listen_addr, config.port),
            config.recv_buffer_size,
            config.send_buffer_size,
        )
        .await?;
        info!("Listening on: {}", transport.local_addr()?);
        Ok(Arc::new(transport))
    }
}

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Assemble an engine from an already-constructed [`Transport`] and a clock. Pure wiring — no
    /// I/O — so it is infallible; the fallible socket bind lives in [`new`](Self::new).
    fn build(
        config: Config,
        transport: Arc<dyn Transport<Addr = SocketAddr>>,
        clock: Arc<dyn Clock>,
        node_id_is_random: bool,
    ) -> Self {
        config.check_key_or_insecure_opt_in();
        let authenticator = auth::Authenticator::new(config.cluster_key, config.encrypt);
        match &authenticator {
            #[cfg(feature = "encryption")]
            auth::Authenticator::Encrypted(_) => {
                debug!("per-datagram authenticated encryption (XChaCha20-Poly1305) ENABLED");
            }
            auth::Authenticator::Enabled(_) => {
                debug!("per-datagram MAC authentication ENABLED");
            }
            auth::Authenticator::Disabled => {
                warn!(
                    "SECURITY: running with Config::with_insecure_no_key() — UDP reconciliation \
                     is UNAUTHENTICATED. Any host that can send UDP to this port can forge \
                     updates and poison the cluster via last-write-wins, and any host inside the \
                     configured nets will eventually receive the ENTIRE DATASET via paced diff \
                     dumps once RandomProbe discovers it. Set Config::with_cluster_key on every \
                     node, or restrict the network to a trusted underlay. See REVIEW.md F3."
                );
            }
        }
        let authenticator_enabled = !matches!(authenticator, auth::Authenticator::Disabled);
        let bulk_send_rate = config.bulk_send_rate.map(|rate| {
            if rate > 0 && rate < MIN_BULK_SEND_RATE {
                warn!(
                    "bulk_send_rate {rate} B/s is below the {MIN_BULK_SEND_RATE} B/s floor and \
                     would wedge a peer's bulk dump for an effectively unbounded sleep; clamping \
                     up to the floor. See #331."
                );
                MIN_BULK_SEND_RATE
            } else {
                rate
            }
        });
        let map = FingerprintTreeMap::<K, Entry<Timestamp, V>>::new();
        let projection = FingerprintTreeMap::<K, State<V>>::new();
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
        Replica {
            inner: Arc::new(Inner {
                map: Arc::new(RwLock::new(map)),
                projection: Arc::new(RwLock::new(projection)),
                port: config.port,
                transport,
                nets,
                local_net: Arc::new(RwLock::new(local_net)),
                listen_addr: config.listen_addr,
                remote_interval: Arc::new(AtomicU32::new(config.remote_interval)),
                remote_fanout: Arc::new(AtomicUsize::new(config.remote_fanout)),
                reconcile_interval: Arc::new(RwLock::new(config.reconcile_interval)),
                bulk_send_rate,
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
                replay_filter: Arc::new(replay::ReplayFilter::new(
                    config.freshness_window,
                    authenticator_enabled,
                )),
                members: Arc::new(RwLock::new(HashSet::new())),
                tombstone_acks: Arc::new(RwLock::new(HashMap::new())),
                live_tombstones: Arc::new(RwLock::new(HashSet::new())),
                clock,
                node_id_is_random,
                max_peers: PeerCap::new(config.max_peers),
            }),
        }
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.map.read().aggregate(range).fingerprint()
    }

    /// Fingerprint of the value-only [`projection`](Self::projection) over a range.
    ///
    /// This is the timestamp-less counterpart of [`fingerprint`](Self::fingerprint); a dateless
    /// read replica that has converged with this store computes the same value over the same range.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.projection.read().aggregate(range).fingerprint()
    }

    /// Insert into the dated `map` **and** mirror the value-only projection (and the
    /// live-tombstone index), under a consistent lock order (`map` -> `live_tombstones` ->
    /// `projection`) shared by every mutation path so the structures never deadlock against each
    /// other. The caller already holds the `map` write guard.
    fn map_insert(
        &self,
        guard: &mut FingerprintTreeMap<K, Entry<Timestamp, V>>,
        key: K,
        value: Entry<Timestamp, V>,
    ) -> Option<Entry<Timestamp, V>> {
        // Keep the live-tombstone index in step with the map at its single mutation sink: a
        // tombstone value adds the key; any live value (a fresh insert, or an LWW overwrite that
        // resurrects a previously-deleted key) removes it. This index drives the per-round
        // causal-stability ack resend in `start_reconciliation`.
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

    /// Remove a key from the dated `map`, its value-only projection, and the live-tombstone
    /// index (the GC removal path).
    pub(crate) fn gc_remove(&self, key: &K) -> Option<Entry<Timestamp, V>> {
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

    /// This node's Hybrid-Logical-Clock identity, read back from the clock adapter so it can never
    /// disagree with the `node_id` actually stamped onto minted timestamps.
    pub(crate) fn node_id(&self) -> NodeId {
        self.clock.node_id()
    }

    /// (runtime) Replace the declared networks wholesale and re-derive the local network.
    ///
    /// # Errors
    ///
    /// If `nets` exceeds [`MAX_NETS`] — the same cap [`Config::with_net`]/`try_with_net` enforce
    /// at construction time and [`add_net`](Self::add_net) enforces at runtime (#293).
    pub(crate) fn set_nets(&self, nets: &[IpNet]) -> Result<(), ConfigError> {
        if nets.len() > MAX_NETS {
            return Err(ConfigError::TooManyNets);
        }
        let nets = nets.to_vec();
        let local = derive_local_net(&nets, self.listen_addr);
        // local_net is always derived from nets; update it together so a reader never observes a
        // local net that is inconsistent with the freshly-installed nets for long.
        *self.local_net.write() = local;
        *self.nets.write() = nets;
        Ok(())
    }

    /// (runtime) Declare an additional network. Idempotent; returns `false` (and logs) if the
    /// [`MAX_NETS`](crate::replicated_map::MAX_NETS) cap is reached.
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

    /// Bundle this engine's outbound ports and send state for the batched-message helpers
    /// ([`send_messages_to`] / [`send_messages_paced`]). See [`SendPorts`].
    fn send_ports(&self) -> SendPorts<'_, dyn Transport<Addr = SocketAddr>> {
        SendPorts {
            transport: &*self.transport,
            authenticator: &self.authenticator,
            sender_counter: &self.sender_counter,
        }
    }

    pub fn just_insert(&self, key: K, value: Entry<Timestamp, V>) -> Option<Entry<Timestamp, V>> {
        // Hooks run outside the write lock: a hook that re-inserts must not re-enter it and
        // deadlock (matching the update-merge path in `handle_messages`).
        (self.pre_insert.read())(&key, &value);

        // A tombstone value is a removal; a live value is an insertion. Counting here (rather
        // than in `ReplicatedMap`) keeps every local mutation path covered.
        if value.is_tombstone() {
            observability::record_remove();
        } else {
            observability::record_insert();
        }

        let mut guard = self.map.write();
        self.map_insert(&mut guard, key, value)
    }

    /// Broadcast a batch of messages to every known peer, on a detached task so the write path
    /// does not block on the network.
    fn broadcast(&self, messages: Vec<Message<K, Entry<Timestamp, V>, State<V>>>) {
        let peers = self.get_peers();
        let port = self.port;
        let transport = Arc::clone(&self.transport);
        let authenticator = self.authenticator.clone();
        let sender_counter = Arc::clone(&self.sender_counter);
        tokio::spawn(async move {
            let ports = SendPorts {
                transport: &*transport,
                authenticator: &authenticator,
                sender_counter: &sender_counter,
            };
            let mut send_buf = Vec::new();
            for addr in peers {
                let peer = SocketAddr::new(addr, port);
                send_messages_to(&messages, &ports, &peer, &mut send_buf).await;
            }
        });
    }

    /// Claim both a per-peer in-flight slot and a global dump slot, or `None` if either is taken.
    ///
    /// Called **before** snapshotting the range, so a skipped dump allocates nothing; the guards
    /// release on drop, panic included.
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

    /// Send a bulk batch of differing values to one peer on a detached, **rate-paced** task —
    /// the cold-sync path.
    ///
    /// Three mechanisms bound it, all against the same amplification: pacing to
    /// [`bulk_send_rate`](Inner::bulk_send_rate) off the receive loop, one dump per peer (an
    /// `Update` triggers no reply, so the holder's reconcile timer would otherwise re-dump ranges
    /// in transit), and a global [`try_claim_dump_slot`](Self::try_claim_dump_slot) budget
    /// bounding total in-flight snapshot memory. Anything genuinely missed is picked up by the
    /// next round.
    fn spawn_paced_send(
        &self,
        messages: Vec<Message<K, Entry<Timestamp, V>, State<V>>>,
        peer: SocketAddr,
        peer_guard: BulkInFlightGuard,
        global_guard: BulkDumpCountGuard,
    ) {
        let transport = Arc::clone(&self.transport);
        let authenticator = self.authenticator.clone();
        let sender_counter = Arc::clone(&self.sender_counter);
        let rate = self.bulk_send_rate;
        tokio::spawn(async move {
            // Hold both RAII guards for the lifetime of the task: they release their respective
            // slots on drop, even if this task is aborted or panics.
            let _peer_guard = peer_guard;
            let _global_guard = global_guard;
            let ports = SendPorts {
                transport: &*transport,
                authenticator: &authenticator,
                sender_counter: &sender_counter,
            };
            let mut send_buf = Vec::new();
            send_messages_paced(&messages, &ports, &peer, &mut send_buf, rate).await;
        });
    }

    pub fn insert(&self, key: K, value: Entry<Timestamp, V>) -> Option<Entry<Timestamp, V>> {
        let ret = self.just_insert(key.clone(), value.clone());
        self.broadcast(vec![Message::Update::<K, Entry<Timestamp, V>, State<V>>((
            key, value,
        ))]);
        ret
    }

    /// Broadcast a single locally-mutated entry to peers, mirroring [`insert`](Self::insert)'s
    /// propagation. Used by in-place mutation paths (`ReplicatedMap::get_mut`) that write the map
    /// directly and must still notify peers so the edit reconciles, without re-applying it locally.
    pub(crate) fn broadcast_update(&self, key: K, value: Entry<Timestamp, V>) {
        self.broadcast(vec![Message::Update::<K, Entry<Timestamp, V>, State<V>>((
            key, value,
        ))]);
    }

    pub fn just_insert_bulk(&self, key_values: &[(K, Entry<Timestamp, V>)]) {
        // Hooks run outside the write lock, for the same re-entrancy reason as `just_insert`.
        for (key, value) in key_values {
            (self.pre_insert.read())(key, value);
            if value.is_tombstone() {
                observability::record_remove();
            } else {
                observability::record_insert();
            }
        }
        let mut guard = self.map.write();
        for (key, value) in key_values {
            self.map_insert(&mut guard, key.clone(), value.clone());
        }
    }

    pub fn insert_bulk(&self, key_values: &[(K, Entry<Timestamp, V>)]) {
        self.just_insert_bulk(key_values);
        let messages: Vec<_> = key_values
            .iter()
            .map(|kv| Message::Update::<K, Entry<Timestamp, V>, State<V>>(kv.clone()))
            .collect();
        self.broadcast(messages);
    }

    /// Drive the gossip and reconciliation loops forever.
    ///
    /// This method does not return and cannot fail: network send errors are logged and
    /// counted, never fatal, so a vanished or unreachable peer cannot stop the loops.
    #[instrument(name = "reconcile.run", skip_all, fields(port = self.port))]
    pub async fn run(self) {
        // One byte larger than the largest legal datagram, so a message that fills it exactly
        // is distinguishable from one that was truncated.
        let mut recv_buf = [0; BUFFER_SIZE + 1];
        let mut send_buf = Vec::new();
        self.start_reconciliation(&mut send_buf).await;
        loop {
            // Re-read each iteration so the cadence can be retuned at runtime.
            let recv_timeout = *self.reconcile_interval.read();
            match timeout(recv_timeout, self.transport.recv_from(&mut recv_buf)).await {
                Err(_) => {
                    debug!("no recent activity; initiating diff protocol");
                    self.start_reconciliation(&mut send_buf).await;
                }
                Ok(Err(err)) => {
                    warn!("network error in recv_from: {err}");
                    observability::record_datagram_dropped("recv_error");
                }
                Ok(Ok((size, peer))) => {
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
                                // Reject a differently-versioned peer with a distinguishable,
                                // counted reason — never confused with "malformed" or "bad_mac".
                                // Runs on already-authenticated bytes (a forged version claim is
                                // rejected the same way a forged payload is), but ahead of every
                                // other per-sender bookkeeping below.
                                let payload = match payload.check_version() {
                                    Ok(payload) => payload,
                                    Err(version) => {
                                        trace!(
                                            "dropped datagram from {peer}: wire version {version} \
                                             != {}",
                                            gossip::auth::WIRE_VERSION
                                        );
                                        observability::record_datagram_dropped("version");
                                        continue;
                                    }
                                };
                                let sender = peer.ip();
                                // If this sender is new and membership is at capacity, drop before
                                // allocating any per-sender state (replay filter, peers map,
                                // membership). Placed ahead of the replay filter so a capped-out
                                // sender never gets an entry there either. Known senders bypass it.
                                let (is_known, current_len) = {
                                    let guard = self.members.read();
                                    (guard.contains(&sender), guard.len())
                                };
                                if !self.max_peers.admits(is_known, current_len) {
                                    trace!(
                                        "dropped datagram from {peer}: peer cap reached \
                                         ({current_len}/{})",
                                        self.max_peers.max()
                                    );
                                    observability::record_datagram_dropped("peer_cap");
                                    continue;
                                }
                                // A no-op in unauthenticated mode: the filter was built disabled.
                                let (seq, stamp) = (payload.seq, payload.stamp);
                                let Some(payload) =
                                    payload.verify_replay(&self.replay_filter, sender)
                                else {
                                    trace!(
                                        "dropped replayed or stale datagram from {peer}: \
                                         seq={seq} stamp={stamp}"
                                    );
                                    observability::record_datagram_dropped("replay");
                                    continue;
                                };
                                let spoke_dated =
                                    self.handle_messages(payload, peer, &mut send_buf).await;
                                // Only accepted datagrams register a sender, so a spoofed host
                                // cannot become a member and block GC forever. A sender that spoke
                                // only the value-only channel is a read replica: it never acks
                                // tombstones, so it must never join `members` either.
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
            rbsr::initial_ranges(&*guard)
        };
        send_buf.clear();
        for segment in segments {
            gossip::bincode::encode(
                &Message::ComparisonItem::<K, Entry<Timestamp, V>, State<V>>(segment),
                send_buf,
            )
            .expect("serializing a ComparisonItem into an in-memory buffer cannot fail");
        }
        // Snapshot the runtime-tunable topology once per round: no torn round, no lock held
        // across the sends below.
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

        // Speculative probes only: an address that answers is registered then, not now. An
        // authoritative source drives the store's seed/decommission loop instead.
        targets.extend(self.probe.discover().await.unwrap_or_default());

        // Local network: contact every known peer, every round (fast intra-network convergence).
        for &addr in &known {
            if local.contains(&addr) {
                targets.insert(addr);
            }
        }

        // Remote peers on cross-network rounds only, a bounded subset per bucket, plus an
        // `unclassified` bucket: repair is decoupled from net membership, so a topology change
        // can never orphan a contacted peer from repair.
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

        // Piggyback causal-stability ack resends for the tombstones we hold.
        self.resend_held_tombstone_acks(send_buf, round);

        // initiate the reconciliation protocol with the selected peers and discovery probes
        for peer in targets {
            trace!("initial_ranges {} bytes to {peer}", send_buf.len());
            if let Err(err) = send_to_retry(
                &*self.transport,
                &self.authenticator,
                &self.sender_counter,
                send_buf,
                SocketAddr::new(peer, self.port),
            )
            .await
            {
                warn!("failed to send reconciliation initiation to {peer}: {err}; continuing");
            }
        }
        observability::record_round_duration(timer);
    }

    /// Append an ack for each held tombstone to `send_buf`, returning the count.
    ///
    /// Acks are otherwise pairwise, so past two nodes
    /// [`is_tombstone_stable`](Self::is_tombstone_stable) never completes; resending every round
    /// makes the matrix converge transitively, and makes an ack that arrived before its tombstone
    /// (dropped by the admission gate) recoverable on a later round.
    ///
    /// Bounded to [`TOMBSTONE_ACK_RESEND_BYTE_BUDGET`] bytes per datagram, over a window whose
    /// start advances with `round` across sorted keys, so every tombstone is covered within a
    /// bounded number of rounds.
    fn resend_held_tombstone_acks(&self, send_buf: &mut Vec<u8>, round: u32) -> usize {
        let mut keys: Vec<K> = self.live_tombstones.read().iter().cloned().collect();
        if keys.is_empty() {
            return 0;
        }
        keys.sort_unstable();
        let n = keys.len();
        let budget = send_buf.len() + TOMBSTONE_ACK_RESEND_BYTE_BUDGET;
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
            // Re-confirm against the map: the tombstone may have been resurrected or GC'd since
            // we snapshotted the index, and only the live tombstone's version is a valid ack.
            if let Some(v) = map_guard.get(key).filter(|v| v.is_tombstone()) {
                gossip::bincode::encode(
                    &Message::Ack::<K, Entry<Timestamp, V>, State<V>>((
                        key.clone(),
                        version_hash(v),
                    )),
                    send_buf,
                )
                .expect("serializing an Ack into an in-memory buffer cannot fail");
                appended += 1;
            }
        }
        if budget_truncated {
            trace!(
                "resent {appended}/{n} held-tombstone acks this round (datagram byte budget \
                 reached); the remainder rotates in on subsequent rounds"
            );
        }
        observability::record_tombstone_acks_resent(appended);
        appended
    }

    /// Handle the messages in an already-authenticated, replay-checked [`Payload`] — taking
    /// [`auth::Payload<auth::Verified>`](auth::Payload) rather than bytes makes an unchecked
    /// datagram unrepresentable here.
    ///
    /// Returns whether the datagram carried at least one **dated** message, which is what
    /// qualifies the sender for membership: a value-only sender is a read replica and must not
    /// gate tombstone GC.
    #[instrument(name = "reconcile.handle", skip_all, fields(peer = %peer))]
    async fn handle_messages(
        &self,
        payload: auth::Payload<'_, auth::Verified>,
        peer: SocketAddr,
        send_buf: &mut Vec<u8>,
    ) -> bool {
        let timer = observability::timer();
        let payload = payload.as_bytes();
        trace!("received {} bytes from {peer}", payload.len());
        let mut in_comparison = Vec::new();
        let mut updates: Vec<(K, Entry<Timestamp, V>)> = Vec::new();
        let mut acks: Vec<(K, u64)> = Vec::new();
        let mut value_in_comparison = Vec::new();
        // Decode the whole datagram through `gossip::bincode`. `MAX_MESSAGES_PER_DATAGRAM` bounds the
        // message count (a datagram can hold no more one-byte messages than its byte length), so a
        // crafted datagram cannot be expanded without limit. A malformed datagram is dropped whole —
        // never panicking the receive loop, an unauthenticated remote-DoS hazard.
        let messages: Vec<Message<K, Entry<Timestamp, V>, State<V>>> =
            match gossip::bincode::decode_stream(payload, MAX_MESSAGES_PER_DATAGRAM) {
                Ok(messages) => messages,
                Err(kind) => {
                    warn!("failed to deserialize datagram from {peer}, dropping it: {kind:?}");
                    observability::record_datagram_dropped("malformed");
                    return false;
                }
            };
        for message in messages {
            match message {
                Message::ComparisonItem(segment) => in_comparison.push(segment),
                Message::Update(update) => updates.push(update),
                Message::Ack(ack) => acks.push(ack),
                Message::ValueComparisonItem(segment) => value_in_comparison.push(segment),
                // A dated store is authoritative and never integrates a value-only update; read replicas
                // are the only consumers of `ValueUpdate`. Ignore it defensively.
                Message::ValueUpdate(_) => {}
            }
        }
        let spoke_dated = !in_comparison.is_empty() || !updates.is_empty() || !acks.is_empty();
        // record tombstone acknowledgments received from the peer
        if !acks.is_empty() {
            let peer_ip = peer.ip();
            let map_guard = self.map.read();
            let mut guard = self.tombstone_acks.write();
            for (key, version) in acks {
                // Only acks for locally-held tombstones, so `tombstone_acks` cannot grow
                // unbounded. An ack arriving before its deletion is dropped here and recovered by
                // the next round's ack resend.
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
        if !in_comparison.is_empty() {
            debug!("received {} segments", in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.map.read();
                rbsr::protocol_round(
                    &*guard,
                    in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // Refinement comparison items are small and latency-sensitive: send them inline, now.
            if !out_comparison.is_empty() {
                debug!("returning {} segments", out_comparison.len());
                trace!("segments: {out_comparison:?}");
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::ComparisonItem::<K, Entry<Timestamp, V>, State<V>>)
                    .collect();
                send_messages_to(&messages, &self.send_ports(), &peer, send_buf).await;
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
                    let updates: Vec<Message<K, Entry<Timestamp, V>, State<V>>> = {
                        let guard = self.map.read();
                        let mut updates = Vec::new();
                        for range in differences {
                            for (k, v) in guard.range(range) {
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
            let mut to_apply: Vec<(K, Entry<Timestamp, V>)> = Vec::new();
            {
                let guard = self.map.read();
                for (k, remote_v) in updates {
                    // Advance our clock past the timestamp carried by the remote value, so a
                    // later local write is ordered after everything we have seen. This is
                    // what prevents lost updates under clock skew.
                    self.clock.observe(remote_v.stamp);
                    match guard.get(&k) {
                        Some(local_v) => {
                            // Under LWW the stamp comparison alone answers "would merging change
                            // state?", so the value is never cloned or compared here.
                            if remote_v.stamp > local_v.stamp {
                                to_apply.push((k, remote_v));
                            } else if local_v.is_tombstone() {
                                // We already hold an equal-or-newer value; still acknowledge it
                                // if it is the same tombstone, so the peer learns we have it.
                                acks_to_send.push(
                                    Message::Ack::<K, Entry<Timestamp, V>, State<V>>((
                                        k,
                                        version_hash(local_v),
                                    )),
                                );
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
            // 3) Re-acquire and re-reconcile: the lock was released, so a concurrent write may
            //    have landed. `reconcile` is idempotent `max`, so re-applying is safe either way.
            if !to_apply.is_empty() {
                let mut guard = self.map.write();
                for (k, v) in to_apply {
                    let merged_v = match guard.get(&k) {
                        Some(local_v) => local_v.merge(&v),
                        None => v,
                    };
                    let version = merged_v.is_tombstone().then(|| version_hash(&merged_v));
                    self.map_insert(&mut guard, k.clone(), merged_v);
                    if let Some(version) = version {
                        acks_to_send.push(Message::Ack::<K, Entry<Timestamp, V>, State<V>>((
                            k, version,
                        )));
                    }
                }
            }
            if !acks_to_send.is_empty() {
                send_messages_to(&acks_to_send, &self.send_ports(), &peer, send_buf).await;
            }
        }
        // Value-only channel: answer a dateless read replica by diffing against the value-only
        // *projection* tree (never the dated map) and replying with `ValueUpdate`s carrying only
        // the projected payload. This path is entirely independent of the dated channel and of the
        // causal-stability state — no acks, no membership, no GC interaction.
        if !value_in_comparison.is_empty() {
            debug!("received {} value-only segments", value_in_comparison.len());
            let mut differences = Vec::new();
            let mut out_comparison = Vec::new();
            {
                let guard = self.projection.read();
                rbsr::protocol_round(
                    &*guard,
                    value_in_comparison,
                    &mut out_comparison,
                    &mut differences,
                );
            }
            // Refinement comparison items are small and latency-sensitive: send them inline, now.
            if !out_comparison.is_empty() {
                let messages: Vec<_> = out_comparison
                    .into_iter()
                    .map(Message::ValueComparisonItem::<K, Entry<Timestamp, V>, State<V>>)
                    .collect();
                send_messages_to(&messages, &self.send_ports(), &peer, send_buf).await;
            }
            // Bulk value-only payload — a dateless read replica pulling the dataset. Rate-pace it on a
            // background task, exactly like the dated bulk path.
            if !differences.is_empty() {
                if let Some((peer_guard, global_guard)) = self.try_claim_dump_slot(peer) {
                    let updates: Vec<Message<K, Entry<Timestamp, V>, State<V>>> = {
                        let guard = self.projection.read();
                        let mut updates = Vec::new();
                        for range in differences {
                            for (k, p) in guard.range(range) {
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

    /// Whether the tombstone for `key` at this version has been acknowledged by every member and
    /// is safe to collect. With no members known, GC is allowed.
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

    /// Whether `peer` still owes an acknowledgment on some held tombstone — i.e. whether its
    /// absence would block GC.
    ///
    /// Walks [`live_tombstones`](Self::live_tombstones), not
    /// [`tombstone_acks`](Self::tombstone_acks): a freshly deleted tombstone has no ack entry yet
    /// and must still count as pending.
    pub(crate) fn has_pending_tombstone_acks(&self, peer: IpAddr) -> bool {
        let live = self.live_tombstones.read();
        if live.is_empty() {
            return false;
        }
        let map = self.map.read();
        let acks = self.tombstone_acks.read();
        live.iter().any(|key| {
            let Some(entry) = map.get(key) else {
                return false;
            };
            let version = version_hash(entry);
            acks.get(key).and_then(|peer_acks| peer_acks.get(&peer)) != Some(&version)
        })
    }

    /// Permanently remove a peer from membership, clearing its recorded acks, so tombstones stop
    /// waiting for it.
    ///
    /// Per-peer replay state is deliberately **kept** (AGENTS.md §8): dropping it would let a
    /// captured datagram replayed inside the freshness window re-add the peer.
    pub(crate) fn decommission_peer(&self, peer: IpAddr) {
        self.members.write().remove(&peer);
        self.peers.write().remove(&peer);
        for key_acks in self.tombstone_acks.write().values_mut() {
            key_acks.remove(&peer);
        }
    }

    /// Register or refresh a known peer at runtime — what a discovery source calls per resolved
    /// address.
    ///
    /// Re-arms `PEER_EXPIRATION` and makes the address a gossip target. Never touches
    /// [`members`](Self::members) (`ARCHITECTURE.md` §5 invariant 6).
    pub(crate) fn seed_peer(&self, peer: IpAddr) {
        self.peers.write().insert(peer, Instant::now());
    }

    /// A snapshot of the monotonic membership set (peers that gate tombstone GC).
    pub(crate) fn members_snapshot(&self) -> HashSet<IpAddr> {
        self.members.read().clone()
    }

    /// Number of entries currently in the peers gossip-routing map.
    ///
    /// Exposed for test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn peers_map_len(&self) -> usize {
        self.peers.read().len()
    }

    /// Number of entries currently in the per-peer replay filter.
    ///
    /// Exposed for test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn replay_filter_len(&self) -> usize {
        self.replay_filter.len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn tombstone_acks_len(&self) -> usize {
        self.tombstone_acks.read().len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub(crate) fn bulk_dumps_in_flight_count(&self) -> usize {
        self.bulk_dumps_in_flight.load(Ordering::Acquire)
    }

    /// This node's configured listen address, used by discovery to never decommission itself.
    pub(crate) fn listen_addr(&self) -> IpAddr {
        self.listen_addr
    }
}

/// The three things a batched send needs, which always travel together: the [`Transport`], the
/// authenticator, and the per-sender replay counter.
pub(crate) struct SendPorts<'a, T: ?Sized> {
    pub(crate) transport: &'a T,
    pub(crate) authenticator: &'a auth::Authenticator,
    pub(crate) sender_counter: &'a replay::SenderCounter,
}

pub(crate) async fn send_to_retry<T: Transport<Addr = SocketAddr> + ?Sized>(
    transport: &T,
    authenticator: &auth::Authenticator,
    sender_counter: &replay::SenderCounter,
    buf: &[u8],
    target: SocketAddr,
) -> std::io::Result<usize> {
    // Allocate a sequence number and stamp, then frame the datagram once and reuse it across
    // retries. `seal` always frames — even disabled adds the wire-version byte.
    let seq = sender_counter.next_seq();
    let stamp = sender_counter.next_stamp();
    let wire = authenticator.seal(seq, stamp, buf);
    let mut res = Ok(0);
    for _ in 0..MAX_SENDTO_RETRIES {
        res = transport.send_to(&wire, &target).await;
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

/// Send `messages` to `peer` back-to-back: the small latency-sensitive batches. Bulk dumps use
/// [`send_messages_paced`].
pub(crate) async fn send_messages_to<K, V, P, T>(
    messages: &[Message<K, V, P>],
    ports: &SendPorts<'_, T>,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
) where
    K: Serialize,
    V: Serialize,
    P: Serialize,
    T: Transport<Addr = SocketAddr> + ?Sized,
{
    send_messages_paced(messages, ports, peer, send_buf, None).await
}

/// Send `messages` to `peer` as ≤64 KiB datagrams, metered to `rate` bytes/sec when set.
///
/// Sleeps between datagrams, so it must run **off** the receive loop
/// ([`Replica::spawn_paced_send`]) — pacing inline would stall reception for every other peer.
#[instrument(name = "reconcile.send", skip_all, fields(peer = %peer, count = messages.len()))]
pub(crate) async fn send_messages_paced<K, V, P, T>(
    messages: &[Message<K, V, P>],
    ports: &SendPorts<'_, T>,
    peer: &SocketAddr,
    send_buf: &mut Vec<u8>,
    rate: Option<usize>,
) where
    K: Serialize,
    V: Serialize,
    P: Serialize,
    T: Transport<Addr = SocketAddr> + ?Sized,
{
    debug!("sending {} messages to {peer}", messages.len());
    // Reserve room for the authentication tag so the sealed datagram still fits a UDP payload.
    let max_payload = BUFFER_SIZE - ports.authenticator.overhead();
    send_buf.clear();
    // Anchor the pacing schedule once, so it self-corrects rather than drifting per datagram.
    let start = Instant::now();
    let mut sent_bytes: usize = 0;
    for message in messages {
        let last_size = send_buf.len();
        gossip::bincode::encode(message, send_buf)
            .expect("serializing a protocol Message into an in-memory buffer cannot fail");
        let this_message_len = send_buf.len() - last_size;
        if send_buf.len() > max_payload {
            // Flush whatever was accumulated *before* this message, if anything — a real,
            // correctly-sized datagram, unaffected by whether this message itself fits.
            if last_size > 0 {
                trace!("sending {} bytes to {peer}", last_size);
                if let Err(err) = send_to_retry(
                    ports.transport,
                    ports.authenticator,
                    ports.sender_counter,
                    &send_buf[..last_size],
                    *peer,
                )
                .await
                {
                    warn!("failed to send datagram to {peer}: {err}; continuing");
                } else {
                    trace!("sent {} bytes to {peer}", last_size);
                }
                sent_bytes += last_size;
                pace(rate, start, sent_bytes).await;
            }
            send_buf.drain(..last_size);
            if this_message_len > max_payload {
                // This message's own encoding exceeds `max_payload` on its own — no datagram it
                // could ever be packed into, alone or otherwise. Sending it anyway (either as a
                // bogus empty datagram when it was first in the batch, or as an oversized one
                // otherwise) never converges the key and only ever fails with EMSGSIZE. Drop it,
                // counted and logged distinctly from a transport send failure so it is alertable
                // rather than silently retried forever.
                error!(
                    "dropping oversized message to {peer}: encodes to {this_message_len} bytes, \
                     exceeding the {max_payload}-byte datagram budget; this key will never \
                     converge on this peer until a smaller value is written"
                );
                observability::record_value_oversized();
                send_buf.clear();
            }
        }
    }
    // Empty exactly when the batch was empty, or ended with an oversized message that was just
    // dropped above — either way, an empty datagram is not a real send.
    if !send_buf.is_empty() {
        trace!("sending last {} bytes to {peer}", send_buf.len());
        if let Err(err) = send_to_retry(
            ports.transport,
            ports.authenticator,
            ports.sender_counter,
            send_buf,
            *peer,
        )
        .await
        {
            warn!("failed to send final datagram to {peer}: {err}; continuing");
        } else {
            trace!("sent last {} bytes to {peer}", send_buf.len());
        }
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

/// RAII marker that a bulk dump to `peer` is in flight. Clearing on `Drop` means a panicking send
/// task cannot wedge a peer into permanently transferring.
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
/// [`Replica::try_claim_dump_slot`].
struct BulkDumpCountGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for BulkDumpCountGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Release);
    }
}

/// A fresh, real bindable port for a test that needs one but does not care which (#293:
/// `Config::port` must be nonzero — gossip has no per-peer port discovery, so `0` can never
/// converge — but many single-node/no-real-peer tests only used `0` for its other property, an
/// OS-assigned port that never collides with a concurrently running test).
///
/// A process-local counter cannot reproduce that collision-freedom: `cargo nextest` runs every
/// test in its own process, so a `static` counter starts fresh in each one, and two tests in
/// different processes can compute the identical "next" port and race to bind it. Probing the OS
/// for a genuinely free port instead — bind `:0`, read back what the kernel picked, drop the
/// socket — is what `cargo test`'s thread model and `nextest`'s process model both leave free at
/// the moment this returns.
#[cfg(test)]
pub(crate) fn next_ephemeral_test_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("OS should hand out an ephemeral port")
        .local_addr()
        .expect("a bound socket reports its own address")
        .port()
}

#[cfg(test)]
mod tests;
