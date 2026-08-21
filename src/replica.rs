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
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};

use crate::clock::{Clock, Timestamp};
use crate::discovery::Discovery;
use crate::entry::{Entry, State};
use crate::transport::Transport;
use crate::FingerprintTreeMap;
use gossip::auth;
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

/// Hard cap on distinct tracked peers, owning the one admission rule both receive loops share: a
/// sender is admitted while already known, or while the count is under the cap. Sourced from
/// [`Config::max_peers`](crate::replicated_map::Config::max_peers).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerCap(usize);

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
    transport: Arc<dyn Transport>,
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
    /// When the most recently completed reconciliation round started — `None` before the first.
    /// [`SyncState`](crate::replicated_map::SyncState)'s backing store, alongside
    /// [`round`](Self::round).
    last_round_at: Arc<RwLock<Option<Instant>>>,
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

mod construct;
mod dispatch;
mod gc;
mod membership;
mod pacing;
mod read;
mod reconciliation;
mod run;
mod write;

pub(crate) use construct::check_port_is_nonzero;
pub(crate) use gc::version_hash;
pub(crate) use membership::derive_local_net;

#[cfg(test)]
pub(crate) use pacing::send_messages_paced;
pub(crate) use pacing::{send_messages_to, send_to_retry, SendPorts};

// `pub(crate)` (not private): `tests::next_ephemeral_test_port` is reused by other files' own
// test modules (replicated_set.rs, read_replica_set.rs, read_replica_map.rs,
// replicated_map's own test modules), which reach it as
// `crate::replica::tests::next_ephemeral_test_port`. This is the only test-only content this
// production file carries — a visibility marker on its own test submodule, no test code or
// symbol imported into it.
#[cfg(test)]
pub(crate) mod tests;
