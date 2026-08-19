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
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::bounds::{Key, Value};
use crate::clock::{Clock, HlcClock, NodeId, Timestamp};
use crate::discovery::{Discovery, RandomProbe};
use crate::entry::{Entry, State};
use crate::replicated_map::{Config, MIN_BULK_SEND_RATE};
use crate::transport::{Transport, UdpTransport};
use crate::FingerprintTreeMap;
use gossip::auth;
use gossip::gen_ip::{host_net, net_of};
use gossip::replay;
use rbsr::RangeAggregate;

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

impl<K: Key + Hash, V: Value> Replica<K, V> {
    /// Create an engine over the default [`UdpTransport`].
    ///
    /// # Errors
    ///
    /// If the socket cannot be bound to `(config.listen_addr, config.port)`.
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
}

mod dispatch;
mod gc;
mod membership;
mod pacing;
mod read;
mod reconciliation;
mod run;
mod write;

#[cfg(test)]
pub(crate) use pacing::send_messages_paced;
pub(crate) use pacing::{send_messages_to, send_to_retry, SendPorts};

#[cfg(test)]
mod tests;
