// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReplicatedMap`], a wrapper to a key-value map
//! to enable reconciliation between different instances over a network.

use std::hash::Hash;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bounds::{Key, Value};
use crate::clock::NodeId;
use crate::discovery::Discovery;
use crate::persistence::{InMemoryPersistence, Persistence};
use crate::replica::Replica;
use crate::timeout_wheel::TimeoutWheel;
use crate::transport::Transport;

mod config;
mod discovery;
mod membership;
mod mutate;
mod persistence;
mod read;
mod write;

pub(crate) use config::MIN_BULK_SEND_RATE;
pub use config::{Config, ConfigError, MAX_NETS};
#[cfg(test)]
pub(crate) use discovery::MemberPresence;

/// Default cadence of the dynamic-discovery task (see [`ReplicatedMap::with_discovery_interval`]).
const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(5);

/// Default number of consecutive discovery rounds a member may be absent before it is
/// decommissioned (see [`ReplicatedMap::with_discovery_miss_threshold`]).
const DEFAULT_DISCOVERY_MISS_THRESHOLD: u32 = 3;

/// Default wall-time floor a member with pending unacknowledged tombstones must be absent for
/// before decommissioning (see [`ReplicatedMap::with_discovery_decommission_floor`]). Ten minutes
/// is far above any DNS blip, so it engages only against a sustainedly wrong resolver.
const DEFAULT_DISCOVERY_DECOMMISSION_FLOOR: Duration = Duration::from_secs(600);

/// Core service wrapping a key-value map, reconciled with peers over the network.
///
/// Wraps its [`FingerprintTreeMap`](crate::FingerprintTreeMap)'s insertion and deletion; `run()`
/// must be called to synchronize. Peers come from [`with_seed`](ReplicatedMap::with_seed) and from
/// periodic probing of the declared networks.
pub struct ReplicatedMap<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    /// Internal map and hooks container.
    engine: Replica<K, V>,
    /// Tombstone timestamps for deleted entries.
    tombstones: TimeoutWheel<K>,
    /// Durable backend. Always present (the trait is mandatory); defaults to the non-durable
    /// [`InMemoryPersistence`], swapped out via [`with_persistence`](ReplicatedMap::with_persistence).
    persistence: Arc<dyn Persistence<K, V>>,
    /// Optional dynamic peer-discovery source (e.g. Kubernetes DNS). When `None` (the default),
    /// discovery falls back entirely to the per-network random probing in the engine; when set, a
    /// background task injects the discovered peers and decommissions vanished ones.
    discovery: Option<Arc<dyn Discovery>>,
    /// How often the discovery task resolves the peer set.
    discovery_interval: Duration,
    /// Consecutive missed discovery rounds before a vanished member with no pending unacknowledged
    /// tombstones is decommissioned (the fast path).
    discovery_miss_threshold: u32,
    /// Minimum continuous wall-time absence a member with pending unacknowledged tombstones must
    /// additionally clear before it is decommissioned (see
    /// [`with_discovery_decommission_floor`](Self::with_discovery_decommission_floor)).
    discovery_decommission_floor: Duration,
}

impl<K, V> Clone for ReplicatedMap<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
{
    /// Allows cloning of the `ReplicatedMap` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        ReplicatedMap {
            engine: self.engine.clone(),
            tombstones: self.tombstones.clone(),
            persistence: self.persistence.clone(),
            discovery: self.discovery.clone(),
            discovery_interval: self.discovery_interval,
            discovery_miss_threshold: self.discovery_miss_threshold,
            discovery_decommission_floor: self.discovery_decommission_floor,
        }
    }
}

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Create a `ReplicatedMap`, binding the gossip UDP socket.
    ///
    /// # Errors
    ///
    /// If the socket cannot be bound to `(config.listen_addr, config.port)`.
    ///
    /// ```
    /// use reconcile::{replicated_map::Config, ReplicatedMap};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> std::io::Result<()> {
    /// let store = ReplicatedMap::<String, i32>::new(Config::new(8080).with_insecure_no_key()).await?;
    ///
    /// store.insert("a".to_string(), 1);
    /// assert_eq!(store.get_cloned(&"a".to_string()), Some(1));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(config: Config) -> io::Result<Self> {
        Ok(Self::from_engine(Replica::<K, V>::new(config).await?))
    }

    /// Create a `ReplicatedMap` over a caller-supplied [`Transport`] instead of the default UDP
    /// one — a different datagram transport, or a lossy one to test convergence under adversity.
    ///
    /// Infallible: the caller has already done the one fallible step, binding. An unreliable
    /// transport cannot violate an invariant, since the protocol already assumes loss, duplication
    /// and reordering — unlike an injected [`Clock`](crate::Clock)
    /// ([`new_with_clock`](Self::new_with_clock)'s docs cover what a non-conformant one breaks).
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use reconcile::{replicated_map::Config, InMemoryNetwork, ReplicatedMap};
    /// let network = InMemoryNetwork::new();
    /// let transport = Arc::new(network.bind("127.0.0.1:8080".parse().unwrap()));
    /// let store = ReplicatedMap::<String, String>::new_with_transport(
    ///     Config::default().with_insecure_no_key(),
    ///     transport,
    /// );
    /// ```
    pub fn new_with_transport(
        config: Config,
        transport: Arc<dyn Transport<Addr = SocketAddr>>,
    ) -> Self {
        Self::from_engine(Replica::<K, V>::with_transport(config, transport))
    }

    /// Create a `ReplicatedMap` over the default UDP transport, but a caller-supplied
    /// [`Clock`](crate::Clock) instead of the default `HlcClock` — e.g. to drive deterministic
    /// ordering in a dependent crate's own tests, the way
    /// [`InMemoryNetwork`](crate::InMemoryNetwork) does for `Transport`.
    ///
    /// # Risks of a non-conformant `Clock`
    ///
    /// The store trusts `clock` completely: every write and every received timestamp is ordered
    /// only by what it returns, and nothing here re-derives or cross-checks physical time. This is
    /// **not** like [`new_with_transport`](Self::new_with_transport)'s transport swap, where an
    /// unreliable transport cannot violate an invariant because the protocol already assumes loss,
    /// duplication and reordering — a broken `Clock` can. Concretely, the naive implementation
    /// (read the wall clock, stamp `logical = 0`) compiles and type-checks cleanly:
    ///
    /// - **Non-monotonic `now()`.** Two same-millisecond local writes to a key can mint an equal
    ///   `(physical, logical, node_id)`. `Entry::merge`'s strict `>` (`ARCHITECTURE.md` §5
    ///   invariant 2) then keeps each replica's own value regardless of merge order — the
    ///   fingerprints never agree and the anti-entropy round re-exchanges that key forever.
    /// - **`observe(t)` not chased by a later `now() > t`.** A causally-later local write can end
    ///   up ordered *before* the remote write it was caused by.
    /// - **A clamping `observe_trusted`.** A backward wall-clock step across a restart (NTP
    ///   correction, VM pause, manual clock set) leaves the post-restart clock below this node's
    ///   own already-persisted stamps, and only a clamp-free `observe_trusted` restores it — a
    ///   clamped one silently shadows this node's own pre-restart writes.
    ///
    /// None of this panics, errors, or logs by default — it surfaces as writes that mysteriously
    /// do not stick, or a cluster that never converges. There is no way to gate this at the type
    /// level: monotonicity is a runtime property of an arbitrary implementation, not something
    /// expressible in [`Clock`](crate::Clock)'s signature. **Run
    /// [`assert_conformance`](crate::clock::assert_conformance) over `clock` before passing it
    /// here** — its own documentation goes through each failure mode in more detail.
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use reconcile::clock::{assert_conformance, Clock, NodeId, Timestamp};
    /// # use reconcile::{replicated_map::Config, ReplicatedMap};
    /// # struct MyClock;
    /// # impl Clock for MyClock {
    /// #     fn now(&self) -> Timestamp { unimplemented!() }
    /// #     fn node_id(&self) -> NodeId { NodeId::new(1) }
    /// #     fn observe(&self, _: Timestamp) {}
    /// #     fn observe_trusted(&self, _: Timestamp) {}
    /// # }
    /// # async fn example() -> std::io::Result<()> {
    /// let clock = Arc::new(MyClock);
    /// assert_conformance(&*clock); // panics here, not after it has shipped, if `clock` is broken
    /// let store =
    ///     ReplicatedMap::<String, String>::new_with_clock(Config::default().with_insecure_no_key(), clock)
    ///         .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new_with_clock(
        config: Config,
        clock: Arc<dyn crate::clock::Clock>,
    ) -> io::Result<Self> {
        Ok(Self::from_engine(
            Replica::<K, V>::new_with_clock(config, clock).await?,
        ))
    }

    /// Wrap a constructed engine in the store's own bookkeeping (tombstone wheel, persistence,
    /// discovery defaults). The single place those defaults are spelled out, so the constructors
    /// above cannot drift apart.
    fn from_engine(engine: Replica<K, V>) -> Self {
        let svc = ReplicatedMap {
            engine,
            tombstones: TimeoutWheel::new(),
            persistence: Arc::new(InMemoryPersistence::default()),
            discovery: None,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            discovery_miss_threshold: DEFAULT_DISCOVERY_MISS_THRESHOLD,
            discovery_decommission_floor: DEFAULT_DISCOVERY_DECOMMISSION_FLOOR,
        };
        svc.set_pre_insert(|_, _| {});
        svc
    }

    /// This node's HLC identity: the `node_id` on every [`Timestamp`](crate::clock::Timestamp) it mints.
    ///
    /// Random per construction unless pinned with [`Config::with_node_id`].
    pub fn node_id(&self) -> NodeId {
        self.engine.node_id()
    }

    /// Provides the address of a known peer to the store
    ///
    /// This is optional, but reduces the time to connect to existing peers
    pub fn with_seed(self, peer: IpAddr) -> Self {
        let now = Instant::now();
        self.engine.peers.write().insert(peer, now);
        self
    }

    /// Register or refresh a known peer at runtime — the `&self` counterpart of
    /// [`with_seed`](Self::with_seed), and what a discovery source feeds in.
    ///
    /// Re-arms the peer-expiration window and makes the address a gossip target. Never grants
    /// causal-stability membership (`ARCHITECTURE.md` §5 invariant 6).
    pub fn seed_peer(&self, peer: IpAddr) {
        self.engine.seed_peer(peer);
    }
}

#[cfg(test)]
mod tests;
