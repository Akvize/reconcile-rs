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
use std::sync::Arc;
use std::time::Instant;

use ipnet::IpNet;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::SeedableRng;
use tracing::{debug, warn};

use crate::bounds::{Key, Value};
use crate::entry::State;
use crate::replica::PeerCap;
use crate::replicated_map::Config;
use crate::transport::{Transport, UdpTransport};
use crate::FingerprintTreeMap;
use gossip::auth;
use gossip::gen_ip::net_of;
use gossip::replay;

mod membership;
mod read;
mod write;

type OnUpdateCallback<K, V> = Box<dyn Send + Sync + Fn(&K, &State<V>)>;

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
    transport: Arc<dyn Transport>,
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

/// #294: `ReadReplicaMap` mints no timestamps and runs no bulk-transfer/cross-net-throttle
/// machinery, so several `Config` fields that matter to a dated [`ReplicatedMap`] have no effect
/// here. A non-default value silently doing nothing is a trap — warn once, at construction,
/// rather than leave it silent.
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
             See #294.",
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
    /// ```
    /// use std::sync::Arc;
    /// use reconcile::{replicated_map::Config, InMemoryNetwork, ReadReplicaMap};
    ///
    /// let network = InMemoryNetwork::new();
    /// let transport = Arc::new(network.bind("127.0.0.1:8304".parse().unwrap()));
    /// let read_replica = ReadReplicaMap::<String, String>::new_with_transport(
    ///     Config::default().with_insecure_no_key(),
    ///     transport,
    /// );
    ///
    /// // Read-only: nothing arrives until it reconciles with a dated peer (module docs).
    /// assert!(read_replica.is_empty());
    /// assert!(read_replica.get(&"a".to_string()).is_none());
    /// ```
    pub fn new_with_transport(config: Config, transport: Arc<dyn Transport>) -> Self {
        Self::build(config, transport)
    }

    /// Assemble a read replica from an already-constructed [`Transport`]. Pure wiring — no I/O.
    /// Panics rather than being fallible; see [`new_with_transport`](Self::new_with_transport).
    /// The fallible socket bind lives in [`new`](Self::new).
    fn build(config: Config, transport: Arc<dyn Transport>) -> Self {
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
}

#[cfg(test)]
mod tests;
