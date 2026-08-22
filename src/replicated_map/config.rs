// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use gossip::auth::ClusterKey;
use ipnet::IpNet;

use crate::clock::{ClockDrift, NodeId, MAX_CLOCK_DRIFT};

use super::persistence::SNAPSHOT_INTERVAL;

mod builders;

/// Default metering rate of a single bulk transfer (see [`Config::bulk_send_rate`]). 32 MiB/s
/// spaces 64 KiB datagrams ~2 ms apart: fast, but under the receiver's socket-buffer overrun
/// threshold.
pub(super) const DEFAULT_BULK_SEND_RATE: usize = 32 * 1024 * 1024;

/// Floor on a configured [`Config::bulk_send_rate`]. Below this, [`pace`](crate::replica::pace)
/// holds the per-peer in-flight mark across a sleep long enough to be effectively unbounded,
/// silently wedging that peer's sync for the duration of the dump. A value under the floor is
/// clamped up to it, with a warning — see #331.
pub(crate) const MIN_BULK_SEND_RATE: usize = 1024 * 1024;

/// Default `SO_SNDBUF`/`SO_RCVBUF` request (see [`Config::recv_buffer_size`]). 8 MiB: the kernel
/// clamps to the OS maximum, so it helps on a tuned host and costs nothing on an untuned one,
/// while the stock default holds too few datagrams for a cold-sync burst.
pub(super) const DEFAULT_SOCKET_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Default cap on tracked peers (see [`Config::max_peers`]). Raise via
/// [`Config::with_max_peers`].
pub(super) const DEFAULT_MAX_PEERS: usize = 1024;

/// Default cap on concurrent paced bulk dumps (see [`Config::max_concurrent_bulk_dumps`]), which
/// bounds total in-flight snapshot memory.
pub(super) const DEFAULT_MAX_CONCURRENT_BULK_DUMPS: usize = 4;

/// Maximum number of geographical networks (CIDRs) a [`Config`] can declare, or a running node
/// can hold via [`ReplicatedMap::set_nets`](super::ReplicatedMap::set_nets)/
/// [`add_net`](super::ReplicatedMap::add_net) — one behavior for the cap everywhere it is
/// enforced. Eight networks is generous for real geographical deployments.
pub const MAX_NETS: usize = 8;

/// Why a [`Config`] operation was rejected.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The operation would exceed [`MAX_NETS`] declared networks.
    TooManyNets,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::TooManyNets => write!(f, "at most {MAX_NETS} networks are supported"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Construction parameters for a [`ReplicatedMap`](super::ReplicatedMap). Build with
/// [`Config::new`] (or [`Config::default`]) and the `with_*` builders (e.g.
/// [`with_net`](Config::with_net)); every field is `pub` for direct construction and reading
/// within this crate, but `#[non_exhaustive]` means an external crate must go through a
/// constructor and builders — one construction path, not two with different guarantees.
///
/// ```
/// use reconcile::{replicated_map::Config, ClusterKey};
///
/// let key = ClusterKey::from_hex(
///     "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
/// )
/// .unwrap();
///
/// // The `with_*` builders chain -- see README "Security model" for why a real deployment
/// // always sets a cluster key (`with_insecure_no_key()` is the explicit opt-out, not this).
/// let config = Config::new(4242)
///     .with_net("10.1.0.0/16".parse().unwrap())
///     .with_cluster_key(key);
///
/// assert_eq!(config.port, 4242);
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
    /// UDP port to bind, **and** the port this node assumes every peer listens on — gossip does
    /// no per-peer port discovery, so every node in a cluster must share one port.
    ///
    /// `0` binds to an OS-assigned ephemeral port, which is fine for receiving, but every
    /// outbound datagram to a peer is still addressed to port `0` literally (the OS-assigned port
    /// is never read back) — a node configured this way can never converge with anything, only
    /// receive nothing back.
    /// [`ReplicatedMap::new`](super::ReplicatedMap::new)/[`ReadReplicaMap::new`](crate::ReadReplicaMap::new)
    /// both refuse `port == 0` for exactly this reason; use [`Config::new`] to set a real one.
    pub port: u16,
    /// Local address to bind the UDP socket to (default `127.0.0.1`). Also determines which
    /// declared [`nets`](Self::nets) entry, if any, is this node's local network.
    pub listen_addr: IpAddr,
    /// The geographical networks the cluster spans, each a CIDR; declare them with
    /// [`with_net`](Config::with_net). Empty slots are `None`.
    ///
    /// The **local** net is whichever contains [`listen_addr`](Self::listen_addr) — none matching
    /// means the node warns and treats only itself as local. Remote-net peers are gossiped to
    /// every [`remote_interval`](Self::remote_interval) rounds, to a bounded
    /// [`remote_fanout`](Self::remote_fanout), which is what bounds WAN traffic. A peer's net comes
    /// from its IP alone, so the wire format carries no tag.
    pub nets: [Option<IpNet>; MAX_NETS],
    /// Send the full anti-entropy comparison to remote-network peers every `remote_interval`
    /// reconciliation rounds (default `6`). Local-network peers are always contacted every round.
    /// Lowering this speeds cross-network convergence (and tombstone GC) at the cost of WAN traffic.
    pub remote_interval: u32,
    /// Maximum number of peers contacted per remote network on each cross-network round (default
    /// `2`). Bounds WAN fan-out without designating any node as a relay/gateway. Raising it speeds
    /// cross-network convergence (and tombstone GC) at the cost of WAN traffic.
    pub remote_fanout: usize,
    /// Optional shared cluster secret enabling per-datagram MAC authentication.
    ///
    /// `None` is **unauthenticated**: any host reaching the port can forge updates, and
    /// [`RandomProbe`](crate::discovery::RandomProbe) answers any host inside the configured
    /// [`nets`](Self::nets) — a stranger squatting one IP eventually receives the **entire
    /// dataset**, unauthenticated, via paced diff dumps. `None` without also setting
    /// [`insecure_no_key`](Self::insecure_no_key) is refused at construction time (see
    /// [`with_insecure_no_key`](Self::with_insecure_no_key)) rather than silently running that way.
    /// When set, every node needs the same key and the same MAC backend feature.
    pub cluster_key: Option<ClusterKey>,
    /// Explicit, loudly-named opt-in to run with [`cluster_key`](Self::cluster_key) unset. Default
    /// `false`. Set only through [`with_insecure_no_key`](Self::with_insecure_no_key) — see #325 and
    /// README "Security model" for exactly what a keyless prober receives.
    pub insecure_no_key: bool,
    /// Identity of this node: the tie-break in the HLC total order. Random at startup when
    /// `None`.
    ///
    /// Two nodes must never share an id, or equal `(physical, logical)` stamps stop resolving
    /// deterministically. Set one explicitly for a stable ordering across restarts.
    pub node_id: Option<NodeId>,
    /// Whether to encrypt datagram payloads as well as authenticate them, with the same
    /// [`cluster_key`](Self::cluster_key). Set only through `Config::with_encryption`.
    pub encrypt: bool,
    /// How long the loop waits for inbound activity before initiating a round: the **background**
    /// anti-entropy cadence (default 1 s). Local writes broadcast immediately, independent of it.
    ///
    /// Floor it at roughly a few × RTT, and at or above the pacing gap between datagrams at the
    /// configured [`bulk_send_rate`](Self::bulk_send_rate) (default 32 MiB/s ⇒ ~2 ms between
    /// full-size datagrams). Below that floor, shortening the interval does **not** converge
    /// faster: the diff is multi-round-trip, so this node's own idle timer fires between a
    /// holder's paced datagrams mid-transfer and re-issues a full diff over ranges still in
    /// flight. Cold sync then gets both slower and re-amplified — well past the byte cost a
    /// single paced dump keeps it near, see [`bulk_send_rate`](Self::bulk_send_rate) — while
    /// steady-state idle chatter balloons, since background traffic grows as `1/interval` per
    /// local peer. Retunable via
    /// [`set_reconcile_interval`](crate::ReplicatedMap::set_reconcile_interval).
    pub reconcile_interval: Duration,
    /// Metering rate of a single **bulk** value transfer to one peer (default 32 MiB/s); `None`
    /// bursts back-to-back.
    ///
    /// An unpaced burst overruns the receiver's socket buffer, and the resulting lull makes this
    /// node's own [`reconcile_interval`](Self::reconcile_interval) re-issue a diff over ranges
    /// still in flight — byte amplification far past the dataset size. Pacing on a background task,
    /// plus at most one bulk transfer per peer, keeps a cold sync ≈ the dataset size — but only
    /// down to [`reconcile_interval`](Self::reconcile_interval)'s floor: below the resulting
    /// inter-datagram gap, the *receiver's* idle timer reopens the same re-initiation, since
    /// pacing only guards the sender against a *concurrent* dump. Only the bulk dump is paced;
    /// comparisons, acks and broadcasts go immediately.
    ///
    /// A nonzero value below 1 MiB/s is clamped up to it, with a warning: below that floor, the
    /// per-peer in-flight mark is held across an effectively unbounded sleep, silently wedging
    /// that peer's sync for the duration of the dump — see #331.
    pub bulk_send_rate: Option<usize>,
    /// `SO_RCVBUF` request in bytes, default 8 MiB; `None` leaves the OS default.
    ///
    /// The kernel clamps rather than failing, so a large request is safe. The stock default holds
    /// too few datagrams for a cold-sync burst, and the excess is dropped in the kernel
    /// (`Udp.RcvbufErrors` in `/proc/net/snmp`), invisible to the application.
    pub recv_buffer_size: Option<usize>,
    /// `SO_SNDBUF` request in bytes, default 8 MiB; `None` leaves the OS default.
    ///
    /// A larger buffer queues a bulk burst in the kernel instead of failing
    /// `EWOULDBLOCK`/`ENOBUFS`, which the engine would have to retry.
    pub send_buffer_size: Option<usize>,
    /// Maximum age, past or future, of a datagram's sender stamp before it is dropped as a
    /// replay. Authenticated modes only; default 5 minutes.
    ///
    /// Must tolerate real skew and jitter or legitimate traffic is dropped — [`Duration::ZERO`]
    /// accepts almost nothing. Unvalidated, because too small is stricter, never unsafe.
    pub freshness_window: Duration,
    /// Maximum number of distinct remote peers tracked (default 1024).
    ///
    /// At capacity an unknown sender's datagram is dropped before any per-sender state is
    /// allocated; tracked senders are unaffected and
    /// [`forget_peer`](crate::ReplicatedMap::forget_peer) frees a slot. Read replicas count too,
    /// so size for members *plus* replicas.
    pub max_peers: usize,
    /// Maximum concurrently active paced bulk dumps across all peers (default 4).
    ///
    /// Each holds a snapshot of the differing range for the transfer's duration, so M cold peers
    /// would otherwise cost M × dataset memory. An exhausted budget skips the dump before
    /// allocating; the peer's next diff round retries.
    pub max_concurrent_bulk_dumps: usize,
    /// How often the background task started by [`ReplicatedMap::run`](super::ReplicatedMap::run)
    /// writes a full snapshot to the persistence backend (default 5 s). Only meaningful once
    /// [`with_persistence`](super::ReplicatedMap::with_persistence) has been called — with the
    /// default in-memory backend the snapshot is taken and immediately discarded.
    ///
    /// Up to this much of the most recent writes are lost on an ungraceful restart (one that skips
    /// [`run`](super::ReplicatedMap::run)'s shutdown flush — see
    /// [`snapshot_now`](super::ReplicatedMap::snapshot_now) for a caller-triggered flush at any
    /// other time).
    pub snapshot_interval: Duration,
    /// How far a remote timestamp may lead this node's own physical clock before
    /// [`observe`](crate::Clock::observe) clamps it (default [`MAX_CLOCK_DRIFT`], one hour). A
    /// clock-conformance concern, not a store one — see [`Clock`](crate::Clock)'s docs for what a
    /// non-conformant implementation risks; this only bounds how far a *conformant* peer's clock
    /// may have skewed before this node stops trusting it verbatim.
    pub max_clock_drift: ClockDrift,
    /// How long a local write waits, batched with any other writes, before the accumulated batch
    /// is broadcast to peers as one send loop instead of one broadcast per write (#187). Default
    /// [`Duration::ZERO`]: no coalescing — every write broadcasts immediately, the historical
    /// behavior.
    ///
    /// | constraint | detail |
    /// |---|---|
    /// | latency vs window | peers observe a write up to `coalesce_window` later than with immediate broadcast; a few ms buys far fewer datagrams under a write burst |
    /// | ordering / HLC | same-key writes inside one window collapse to the greatest [`Timestamp`](crate::clock::Timestamp) via [`Entry::merge`](crate::entry::Entry::merge) — the same total order the wire protocol already resolves conflicts with; a value's own stamp is never altered, only when it reaches the wire |
    /// | anti-entropy | this delays only the **eager** push; the periodic RBSR sweep ([`reconcile_interval`](Self::reconcile_interval)) stays the correctness backstop, so a coalesced batch lost in transit still converges |
    ///
    /// Only the write that finds the pending batch empty spawns the detached flush task, so a
    /// write that joins an already-scheduled window does not itself need an ambient Tokio runtime
    /// — see [`ReplicatedMap::insert`](super::ReplicatedMap::insert)'s `# Panics` for the general
    /// rule this refines. Retunable at runtime via
    /// [`set_coalesce_window`](super::ReplicatedMap::set_coalesce_window).
    pub coalesce_window: Duration,
}

impl fmt::Debug for Config {
    /// Redacts [`cluster_key`](Self::cluster_key): prints `Some(<redacted>)`/`None`, never the
    /// key material, so an accidental `{:?}` in a log statement cannot leak it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("listen_addr", &self.listen_addr)
            .field("nets", &self.nets)
            .field("remote_interval", &self.remote_interval)
            .field("remote_fanout", &self.remote_fanout)
            .field(
                "cluster_key",
                &self.cluster_key.as_ref().map(|_| "<redacted>"),
            )
            .field("insecure_no_key", &self.insecure_no_key)
            .field("node_id", &self.node_id)
            .field("encrypt", &self.encrypt)
            .field("reconcile_interval", &self.reconcile_interval)
            .field("bulk_send_rate", &self.bulk_send_rate)
            .field("recv_buffer_size", &self.recv_buffer_size)
            .field("send_buffer_size", &self.send_buffer_size)
            .field("freshness_window", &self.freshness_window)
            .field("max_peers", &self.max_peers)
            .field("max_concurrent_bulk_dumps", &self.max_concurrent_bulk_dumps)
            .field("snapshot_interval", &self.snapshot_interval)
            .field("max_clock_drift", &self.max_clock_drift)
            .field("coalesce_window", &self.coalesce_window)
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 0,
            listen_addr: "127.0.0.1".parse().unwrap(),
            nets: [None; MAX_NETS],
            remote_interval: 6,
            remote_fanout: 2,
            cluster_key: None,
            insecure_no_key: false,
            node_id: None,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
            bulk_send_rate: Some(DEFAULT_BULK_SEND_RATE),
            recv_buffer_size: Some(DEFAULT_SOCKET_BUFFER_SIZE),
            send_buffer_size: Some(DEFAULT_SOCKET_BUFFER_SIZE),
            freshness_window: gossip::replay::FRESHNESS_WINDOW_DEFAULT,
            max_peers: DEFAULT_MAX_PEERS,
            max_concurrent_bulk_dumps: DEFAULT_MAX_CONCURRENT_BULK_DUMPS,
            snapshot_interval: SNAPSHOT_INTERVAL,
            max_clock_drift: MAX_CLOCK_DRIFT,
            coalesce_window: Duration::ZERO,
        }
    }
}
