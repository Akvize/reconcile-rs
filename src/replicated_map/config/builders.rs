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

use crate::clock::{ClockDrift, NodeId};

use super::{Config, ConfigError};

impl Config {
    /// The documented default constructor: `port` is the one setting every node in a cluster
    /// must agree on (see [`port`](Self::port)'s docs for why `0` can never converge). Equivalent
    /// to `Config::default().with_port(port)`.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Config::default().with_port(port)
    }

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
    /// If more than [`MAX_NETS`](super::MAX_NETS) networks are declared. See [`try_with_net`](Self::try_with_net)
    /// for a non-panicking alternative — the same [`MAX_NETS`](super::MAX_NETS) cap
    /// [`ReplicatedMap::set_nets`](super::super::ReplicatedMap::set_nets)/
    /// [`add_net`](super::super::ReplicatedMap::add_net) enforce at runtime.
    #[must_use]
    pub fn with_net(self, net: IpNet) -> Self {
        match self.try_with_net(net) {
            Ok(config) => config,
            Err(err) => panic!("{err}"),
        }
    }

    /// As [`with_net`](Self::with_net), but returns a [`ConfigError`] instead of panicking past
    /// [`MAX_NETS`](super::MAX_NETS).
    ///
    /// # Errors
    ///
    /// If more than [`MAX_NETS`](super::MAX_NETS) networks are declared.
    pub fn try_with_net(mut self, net: IpNet) -> Result<Self, ConfigError> {
        let slot = self
            .nets
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(ConfigError::TooManyNets)?;
        *slot = Some(net);
        Ok(self)
    }

    /// Declare several networks at once (see [`with_net`](Config::with_net)).
    ///
    /// # Panics
    ///
    /// If the total exceeds [`MAX_NETS`](super::MAX_NETS).
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

    /// Set [`snapshot_interval`](Config::snapshot_interval) (default 5 s).
    #[must_use]
    pub fn with_snapshot_interval(mut self, interval: Duration) -> Self {
        self.snapshot_interval = interval;
        self
    }

    /// Set [`max_clock_drift`](Config::max_clock_drift) (default
    /// [`MAX_CLOCK_DRIFT`](crate::clock::MAX_CLOCK_DRIFT)).
    #[must_use]
    pub fn with_max_clock_drift(mut self, drift: ClockDrift) -> Self {
        self.max_clock_drift = drift;
        self
    }

    /// Set [`coalesce_window`](Config::coalesce_window) (default [`Duration::ZERO`], i.e. no
    /// coalescing). Retunable at runtime via
    /// [`ReplicatedMap::set_coalesce_window`](crate::ReplicatedMap::set_coalesce_window).
    #[must_use]
    pub fn with_coalesce_window(mut self, window: Duration) -> Self {
        self.coalesce_window = window;
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
