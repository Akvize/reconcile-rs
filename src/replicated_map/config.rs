// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::net::IpAddr;
use std::time::Duration;

use gossip::auth::ClusterKey;
use ipnet::IpNet;

use crate::clock::NodeId;

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

/// Maximum number of geographical networks (CIDRs) a [`Config`] can declare. A fixed-size array
/// keeps [`Config`] `Copy`; eight networks is generous for real geographical deployments.
pub const MAX_NETS: usize = 8;

/// Construction parameters for a [`ReplicatedMap`](super::ReplicatedMap). Build with
/// [`Config::default`] and the `with_*` builders (e.g. [`with_port`](Config::with_port),
/// [`with_listen_addr`](Config::with_listen_addr), [`with_net`](Config::with_net)); every field
/// is `pub` for direct construction where that reads better.
#[derive(Clone)]
pub struct Config {
    /// UDP port to bind. `0` (the default) asks the OS for an ephemeral port; read back the
    /// actual bound port from the running [`ReplicatedMap`](super::ReplicatedMap) when it matters.
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
        }
    }
}

impl Config {
    /// Set [`port`](Self::port).
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    /// Set [`listen_addr`](Self::listen_addr).
    #[must_use]
    pub fn with_listen_addr(mut self, listen_addr: IpAddr) -> Self {
        self.listen_addr = listen_addr;
        self
    }
    /// Declare a geographical network by its CIDR — once per network, **including this node's
    /// own** (see [`nets`](Config::nets)).
    ///
    /// # Panics
    ///
    /// If more than [`MAX_NETS`] networks are declared.
    #[must_use]
    pub fn with_net(mut self, net: IpNet) -> Self {
        let slot = self
            .nets
            .iter_mut()
            .find(|slot| slot.is_none())
            .unwrap_or_else(|| panic!("at most {MAX_NETS} networks are supported"));
        *slot = Some(net);
        self
    }

    /// Declare several networks at once (see [`with_net`](Config::with_net)).
    ///
    /// # Panics
    ///
    /// If the total exceeds [`MAX_NETS`].
    #[must_use]
    pub fn with_nets(mut self, nets: &[IpNet]) -> Self {
        for &net in nets {
            self = self.with_net(net);
        }
        self
    }

    /// Set how often (in reconciliation rounds) the full anti-entropy comparison is sent to
    /// remote-network peers (default `6`). See [`remote_interval`](Config::remote_interval).
    #[must_use]
    pub fn with_remote_interval(mut self, interval: u32) -> Self {
        self.remote_interval = interval;
        self
    }

    /// Set the maximum number of peers contacted per remote network on each cross-network round
    /// (default `2`). See [`remote_fanout`](Config::remote_fanout).
    #[must_use]
    pub fn with_remote_fanout(mut self, fanout: usize) -> Self {
        self.remote_fanout = fanout;
        self
    }

    /// Set the reconciliation cadence: how long the loop waits for inbound activity before
    /// initiating a round (default 1 s). See [`reconcile_interval`](Config::reconcile_interval).
    /// Retunable at runtime via
    /// [`ReplicatedMap::set_reconcile_interval`](crate::ReplicatedMap::set_reconcile_interval).
    #[must_use]
    pub fn with_reconcile_interval(mut self, interval: Duration) -> Self {
        self.reconcile_interval = interval;
        self
    }

    /// Set the rate, in bytes per second, at which a single bulk anti-entropy value transfer to one
    /// peer is paced (default 32 MiB/s). See [`bulk_send_rate`](Config::bulk_send_rate); to disable
    /// pacing (the historical back-to-back burst), set that field to `None` directly.
    #[must_use]
    pub fn with_bulk_send_rate(mut self, bytes_per_sec: usize) -> Self {
        self.bulk_send_rate = Some(bytes_per_sec);
        self
    }

    /// Request `size` bytes for `SO_RCVBUF`; the kernel clamps to the OS maximum. Raising this
    /// with the matching sysctl is the fix for datagrams dropped during a cold sync. Set
    /// [`recv_buffer_size`](Config::recv_buffer_size) to `None` for the OS default.
    #[must_use]
    pub fn with_recv_buffer_size(mut self, size: usize) -> Self {
        self.recv_buffer_size = Some(size);
        self
    }

    /// Request `size` bytes for `SO_SNDBUF`; the kernel clamps to the OS maximum. Set
    /// [`send_buffer_size`](Config::send_buffer_size) to `None` for the OS default.
    #[must_use]
    pub fn with_send_buffer_size(mut self, size: usize) -> Self {
        self.send_buffer_size = Some(size);
        self
    }

    /// Enable per-datagram MAC authentication with a shared cluster secret, closing the
    /// unauthenticated LWW poisoning vector.
    ///
    /// Incoming datagrams are verified before deserialization and silently dropped on failure.
    /// Every node must share the key and the MAC backend feature (`mac-blake3` or `mac-hmac`);
    /// without one, construction refuses to proceed unless
    /// [`with_insecure_no_key`](Self::with_insecure_no_key) opted in explicitly.
    #[must_use]
    pub fn with_cluster_key(mut self, key: ClusterKey) -> Self {
        self.cluster_key = Some(key);
        self
    }

    /// Explicit, loudly-named opt-in to run with no [`cluster_key`](Self::cluster_key) at all.
    ///
    /// Without either this or [`with_cluster_key`](Self::with_cluster_key), construction refuses
    /// to proceed (#325): [`RandomProbe`](crate::discovery::RandomProbe) answers any host inside
    /// the configured [`nets`](Self::nets), so a stranger squatting one IP eventually receives the
    /// **entire dataset**, unauthenticated, via paced diff dumps — see README "Security model".
    /// Call this only when the network is a trusted underlay the cluster fully controls.
    #[must_use]
    pub fn with_insecure_no_key(mut self) -> Self {
        self.insecure_no_key = true;
        self
    }

    /// Guard for #325: `cluster_key: None` without the explicit `insecure_no_key` opt-in is a
    /// construction-time error, not a silent unauthenticated run. Shared by every engine
    /// constructor (`Replica`, `ReadReplicaMap`) so none of them can bypass it.
    ///
    /// # Panics
    ///
    /// If `cluster_key` is `None` and `insecure_no_key` is `false`.
    pub(crate) fn check_key_or_insecure_opt_in(&self) {
        assert!(
            self.cluster_key.is_some() || self.insecure_no_key,
            "Config::cluster_key is None: every peer this node ever discovers (any host inside \
             the configured nets) would receive the entire dataset, unauthenticated, via paced \
             diff dumps. Set Config::with_cluster_key, or opt in explicitly with \
             Config::with_insecure_no_key() if the network is a trusted underlay. See README \
             \"Security model\"."
        );
    }

    /// Set an explicit node identity for the HLC tie-break; must be distinct per node.
    #[must_use]
    pub fn with_node_id(mut self, node_id: NodeId) -> Self {
        self.node_id = Some(node_id);
        self
    }

    /// Set [`freshness_window`](Config::freshness_window). No effect unkeyed.
    #[must_use]
    pub fn with_freshness_window(mut self, window: Duration) -> Self {
        self.freshness_window = window;
        self
    }

    /// Set the maximum number of distinct peers tracked (default 1024). See
    /// [`max_peers`](Config::max_peers).
    #[must_use]
    pub fn with_max_peers(mut self, max: usize) -> Self {
        self.max_peers = max;
        self
    }

    /// Set [`max_concurrent_bulk_dumps`](Config::max_concurrent_bulk_dumps) (default 4).
    #[must_use]
    pub fn with_max_concurrent_bulk_dumps(mut self, max: usize) -> Self {
        self.max_concurrent_bulk_dumps = max;
        self
    }

    /// Encrypt datagram payloads with XChaCha20-Poly1305, reusing
    /// [`cluster_key`](Self::cluster_key) as the AEAD key — so
    /// [`with_cluster_key`](Self::with_cluster_key) is required on every node.
    ///
    /// Framed as `nonce || ciphertext || tag`, 40 bytes of overhead, verified before
    /// deserialization. The trust model is unchanged: one shared secret, so no per-peer identity
    /// and no forward secrecy.
    ///
    /// Requires the `encryption` cargo feature.
    #[cfg(feature = "encryption")]
    #[must_use]
    pub fn with_encryption(mut self) -> Self {
        self.encrypt = true;
        self
    }
}
