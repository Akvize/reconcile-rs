// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Assembling a [`Replica`]: the [`Clone`]/[`Deref`](std::ops::Deref) handle mechanics, port
//! validation, and the constructor family that binds or wraps a [`Transport`] and hands back a
//! wired-up engine.

use std::hash::Hash;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::SeedableRng;
use tracing::{debug, info, warn};

use ipnet::IpNet;
use parking_lot::RwLock;

use crate::bounds::{Key, Value};
use crate::clock::{Clock, HlcClock, NodeId, Timestamp};
use crate::discovery::{Discovery, RandomProbe};
use crate::entry::{Entry, State};
use crate::replicated_map::{Config, MIN_BULK_SEND_RATE};
use crate::transport::{Transport, UdpTransport};
use crate::FingerprintTreeMap;
use gossip::auth;
use gossip::replay;
use std::collections::{HashMap, HashSet};

use super::{derive_local_net, Inner, PeerCap, Replica};

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

/// Reject `config.port == 0`: gossip does no per-peer port discovery, so every outbound
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
        let clock: Arc<dyn Clock> =
            Arc::new(HlcClock::new(node_id).with_max_clock_drift(config.max_clock_drift));
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
    pub(crate) fn with_transport(config: Config, transport: Arc<dyn Transport>) -> Self {
        let node_id_is_random = config.node_id.is_none();
        let node_id = config
            .node_id
            .unwrap_or_else(|| NodeId::new(rand::random()));
        let clock: Arc<dyn Clock> =
            Arc::new(HlcClock::new(node_id).with_max_clock_drift(config.max_clock_drift));
        Self::build(config, transport, clock, node_id_is_random)
    }

    /// As [`with_transport`](Self::with_transport), but over a caller-supplied [`Clock`] too.
    /// Test-only seam: pairing an in-memory transport with a `ManualClock` makes convergence
    /// deterministic without real sockets or wall-clock time.
    #[cfg(test)]
    pub(crate) fn new_with_transport(
        config: Config,
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::build(config, transport, clock, false)
    }

    /// Bind the default [`UdpTransport`] for `config` and log the bound address.
    async fn bind_udp(config: &Config) -> io::Result<Arc<dyn Transport>> {
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
        transport: Arc<dyn Transport>,
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
                last_round_at: Arc::new(RwLock::new(None)),
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
                coalesce_window: Arc::new(RwLock::new(config.coalesce_window)),
                coalesce_pending: Arc::new(RwLock::new(HashMap::new())),
            }),
        }
    }
}
