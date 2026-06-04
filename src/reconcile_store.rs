// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReconcileStore`], a wrapper to a key-value map
//! to enable reconciliation between different instances over a network.

use std::fmt::Debug;
use std::hash::Hash;
use std::net::IpAddr;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ipnet::IpNet;
use parking_lot::{MappedRwLockReadGuard, RwLockReadGuard};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{info, instrument, warn};

use crate::fingerprint::Fingerprint;
use crate::hlc::Hlc;
use crate::persistence::{DatedEntries, InMemoryPersistence, PersistedState, Persistence};
use crate::reconcilable::Projectable;
use crate::reconcile_engine::{version_hash, ReconcileEngine};
use crate::timeout_wheel::TimeoutWheel;

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// How often the background task writes a full snapshot to the persistence backend.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

/// Core service wrapping a key-value map to enable reconciliation between different instances over a network.
///
/// The store also keeps track of the addresses of other instances.
///
/// Provides wrappers for its underlying [`HRTree`](crate::HRTree)'s insertion and deletion methods,
/// as well as its main service method: `run()`,
/// which must be called to actually synchronize with peers.
///
/// Known peers can optionally be provided using the [`with_seed`](ReconcileStore::with_seed) method. In
/// any case, the store will periodically look for new peers by sampling a random address from
/// the given peer network.
pub struct ReconcileStore<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
    (Hlc, Option<V>): Projectable,
{
    /// Internal map and hooks container.
    engine: ReconcileEngine<K, (Hlc, Option<V>)>,
    /// Tombstone timestamps for deleted entries.
    tombstones: TimeoutWheel<K>,
    /// Durable backend. Always present (the trait is mandatory); defaults to the non-durable
    /// [`InMemoryPersistence`], swapped out via [`with_persistence`](ReconcileStore::with_persistence).
    persistence: Arc<dyn Persistence<K, V>>,
}

impl<K, V> Clone for ReconcileStore<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
    (Hlc, Option<V>): Projectable,
{
    /// Allows cloning of the `ReconcileStore` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        ReconcileStore {
            engine: self.engine.clone(),
            tombstones: self.tombstones.clone(),
            persistence: self.persistence.clone(),
        }
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + PartialEq + Send + Serialize + Sync + 'static,
    > ReconcileStore<K, V>
{
    /// Create a new `ReconcileStore`, set up network and tombstones.
    pub async fn new(config: Config) -> Self {
        let svc = ReconcileStore {
            engine: ReconcileEngine::<K, (Hlc, Option<V>)>::new(config).await,
            tombstones: TimeoutWheel::new(),
            persistence: Arc::new(InMemoryPersistence::default()),
        };
        svc.add_pre_insert(|_, _| {});
        svc
    }

    /// Plug in a durable persistence backend, **loading any previously saved state first**.
    ///
    /// Every store always owns a backend; by default that is the non-durable
    /// [`InMemoryPersistence`], so a process restart starts empty. Call this with a durable
    /// backend (e.g. [`FileSnapshot`](crate::FileSnapshot)) between
    /// [`new`](ReconcileStore::new) and [`run`](ReconcileStore::run) to recover the previous
    /// state — entries, tombstones, and the causal-stability membership/acks — **before** the
    /// node rejoins the gossip protocol, so a restart does not look like a fresh, empty replica.
    ///
    /// Loaded entries are replayed through the pre-insert hook, which preserves each tombstone's
    /// original deletion timestamp and rebuilds the tombstone-expiry wheel.
    ///
    /// # Panics
    ///
    /// Panics if the backend fails to load (e.g. a corrupt snapshot file): recovering from a
    /// damaged durable state is a deliberate, explicit decision rather than a silent fresh start.
    pub fn with_persistence(mut self, backend: Arc<dyn Persistence<K, V>>) -> Self {
        if let Some(state) = backend.load().expect("failed to load persisted state") {
            *self.engine.members.write() = state.members;
            *self.engine.tombstone_acks.write() = state.tombstone_acks;
            // Replay through the wrapped pre-insert hook so the tombstone wheel is rebuilt with the
            // original deletion timestamps (do NOT route through the public insert helpers, which
            // would overwrite timestamps with `Utc::now()`).
            self.engine.just_insert_bulk(&state.entries);
        }
        self.persistence = backend;
        self
    }

    /// Capture the full store state and hand it to the persistence backend.
    fn snapshot(&self) {
        let entries: DatedEntries<K, V> = self
            .engine
            .map
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let state = PersistedState {
            entries,
            members: self.engine.members.read().clone(),
            tombstone_acks: self.engine.tombstone_acks.read().clone(),
        };
        if let Err(err) = self.persistence.save(&state) {
            warn!("failed to persist reconcile store snapshot: {err}");
        }
    }

    /// Periodically snapshot the full store state to the persistence backend.
    async fn snapshot_periodically(&self) {
        loop {
            tokio::time::sleep(SNAPSHOT_INTERVAL).await;
            self.snapshot();
        }
    }

    /// Provides the address of a known peer to the store
    ///
    /// This is optional, but reduces the time to connect to existing peers
    pub fn with_seed(self, peer: IpAddr) -> Self {
        let now = Instant::now();
        self.engine.peers.write().insert(peer, now);
        self
    }

    /// Set a specific expiry timeout to handle tombstones.
    /// The default value is 60 seconds.
    pub fn with_tombstone_timeout(mut self, tombstone_timeout: Duration) -> Self {
        self.tombstones = self.tombstones.with_timeout(tombstone_timeout);
        self
    }

    /// Register a pre-insert hook.
    ///
    /// The hook is invoked **before** inserting each key/value pair into the internal map.
    /// Calling this does **not** consume the `ReconcileStore` instance; you can call it multiple times.
    ///
    /// # Deadlock Safety
    ///
    /// Hooks are executed outside of the map’s write lock, so calling back into any insert
    /// method from within a hook will not block or deadlock.
    pub fn add_pre_insert<F: Send + Sync + Fn(&K, &(Hlc, Option<V>)) + 'static>(
        &self,
        pre_insert: F,
    ) {
        let tombstones = self.tombstones.clone();
        let wrapped_pre_insert = move |k: &K, v: &(Hlc, Option<V>)| {
            pre_insert(k, v);
            if v.1.is_some() {
                tombstones.remove(k);
            } else {
                // The timeout wheel ages tombstones by wall-clock time; use the HLC's
                // physical component so all replicas expire a tombstone at the same logical
                // wall time. GC is in any case gated on causal stability.
                let when =
                    DateTime::from_timestamp_millis(v.0.wall_ms() as i64).unwrap_or_else(Utc::now);
                tombstones.insert(k.clone(), when);
            }
        };
        // Swap in the new hook
        *self.engine.pre_insert.write() = Box::new(wrapped_pre_insert);
    }

    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.fingerprint(range)
    }

    /// Fingerprint of the **value-only projection** over a range — the timestamp-less counterpart of
    /// [`fingerprint`](Self::fingerprint).
    ///
    /// A [`ReconcileMirror`](crate::mirror::ReconcileMirror) that has converged with this
    /// store computes the same value over the same range, even though it never stores timestamps.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.value_fingerprint(range)
    }

    pub fn get(&self, k: &K) -> Option<MappedRwLockReadGuard<'_, V>> {
        let guard = self.engine.map.read();
        RwLockReadGuard::try_map(guard, |map| {
            map.get(k).and_then(|(_, ref opt)| opt.as_ref())
        })
        .ok()
    }

    /// Insert a single key/value pair, running the pre-insert hook first.
    ///
    /// # Behavior
    ///
    /// 1. Calls the registered `pre_insert` hook outside of any locks.
    /// 2. Acquires the write lock on the map, performs the insertion, then drops the lock.
    ///
    /// Returns the overwritten value if the key already existed.
    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        let ret = self
            .engine
            .just_insert(key, (self.engine.clock_now(), Some(value)));
        ret.and_then(|t| t.1)
    }

    /// Fully-qualified insert: just_insert + async broadcast.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self
            .engine
            .insert(key, (self.engine.clock_now(), Some(value)));
        ret.and_then(|t| t.1)
    }

    /// Bulk-insert multiple key/value pairs with hook invocation.
    ///
    /// # Behavior
    ///
    /// 1. Runs the pre-insert hook for each entry (outside any lock).
    /// 2. Acquires the write lock once and inserts all entries.
    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        self.engine.just_insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| (k.clone(), (self.engine.clock_now(), Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-insert + async broadcast.
    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.engine.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| (k.clone(), (self.engine.clock_now(), Some(v.clone()))))
                .collect::<Vec<_>>(),
        );
    }

    pub fn just_remove(&self, key: &K) -> Option<V> {
        let ret = self
            .engine
            .just_insert(key.clone(), (self.engine.clock_now(), None));
        ret.and_then(|t| t.1)
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let ret = self
            .engine
            .insert(key.clone(), (self.engine.clock_now(), None));
        ret.and_then(|t| t.1)
    }

    pub fn just_remove_bulk(&self, keys: &[K]) {
        self.engine.just_insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), (self.engine.clock_now(), None)))
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-remove: stamps a fresh Hybrid Logical Clock timestamp for each key and
    /// broadcasts the resulting tombstones.
    ///
    /// Unlike the previous physical-clock API, callers no longer supply timestamps: a
    /// caller-chosen `DateTime` could collide with another replica's and trigger the
    /// non-commutative tie-break that caused permanent divergence (issue #110).
    pub fn remove_bulk(&self, keys: &[K]) {
        self.engine.insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), (self.engine.clock_now(), None)))
                .collect::<Vec<_>>(),
        );
    }

    pub async fn start_reconciliation(&self) {
        let mut buf = Vec::new();
        self.engine.start_reconciliation(&mut buf).await;
    }

    /// Permanently forget a peer (e.g. a replica that has been decommissioned and will never
    /// return), so that tombstones are no longer held back waiting for its acknowledgment
    /// before garbage collection.
    ///
    /// This is the escape hatch for the causal-stability contract: a tombstone is normally
    /// retained until every peer this node has ever communicated with has acknowledged it.
    /// A replica that is permanently gone must be decommissioned, otherwise its tombstones are
    /// retained forever.
    pub fn forget_peer(&self, peer: IpAddr) {
        self.engine.decommission_peer(peer);
    }

    /// (runtime) Replace the declared geographical networks, retuning the gossip topology **without
    /// recreating the node**. The local network is re-derived automatically (whichever new net
    /// contains the listen address). See [`Config::nets`].
    ///
    /// Topology is per-node and coordination-free (the wire format carries no network tag), and is
    /// safe to change live: anti-entropy *repair* of already-known peers is **not** gated on net
    /// membership, so reshaping the topology never orphans a real peer from repair — it only changes
    /// which addresses are probed for discovery and the local/remote throttle split. Worst case is
    /// suboptimal WAN traffic, never silent divergence.
    pub fn set_nets(&self, nets: &[IpNet]) {
        self.engine.set_nets(nets);
    }

    /// (runtime) Declare an additional network (e.g. opening a new region). Idempotent; returns
    /// `false` (and logs) if the [`MAX_NETS`] cap is already reached. The local network is re-derived.
    pub fn add_net(&self, net: IpNet) -> bool {
        self.engine.add_net(net)
    }

    /// (runtime) Stop declaring a network (e.g. decommissioning a region). Returns whether it was
    /// present. Known peers keep being repaired regardless (see [`set_nets`](Self::set_nets)); to
    /// also reclaim a region quickly elsewhere, prefer adding the new net **before** removing the old
    /// so discovery keeps the cluster well-connected throughout the migration.
    pub fn remove_net(&self, net: IpNet) -> bool {
        self.engine.remove_net(net)
    }

    /// The currently declared networks.
    pub fn nets(&self) -> Vec<IpNet> {
        self.engine.nets()
    }

    /// The current local network (the declared net containing the listen address, else the host
    /// route — see [`Config::nets`]).
    pub fn local_net(&self) -> IpNet {
        self.engine.local_net()
    }

    /// (runtime) Retune how often (in rounds) remote-network peers are reconciled. See
    /// [`Config::remote_interval`].
    pub fn set_remote_interval(&self, interval: u32) {
        self.engine.set_remote_interval(interval);
    }

    /// (runtime) Retune the bounded number of peers contacted per remote network each cross-network
    /// round. See [`Config::remote_fanout`].
    pub fn set_remote_fanout(&self, fanout: usize) {
        self.engine.set_remote_fanout(fanout);
    }

    /// (runtime) Retune the tombstone expiry timeout in place, visible to all clones. The runtime
    /// counterpart of the [`with_tombstone_timeout`](Self::with_tombstone_timeout) builder.
    pub fn set_tombstone_timeout(&self, timeout: Duration) {
        self.tombstones.set_timeout(timeout);
    }

    /// (runtime) Retune the reconciliation cadence in place. See [`Config::reconcile_interval`].
    pub fn set_reconcile_interval(&self, interval: Duration) {
        self.engine.set_reconcile_interval(interval);
    }

    /// Garbage-collect tombstones, **gated on causal stability**.
    ///
    /// A tombstone is removed from the map only once it is both older than the configured
    /// timeout *and* acknowledged by every replica this node has ever communicated with (or
    /// those replicas have been decommissioned via [`forget_peer`](Self::forget_peer)). This
    /// prevents the classic tombstone-resurrection hazard: a replica partitioned for longer
    /// than the timeout cannot resurrect a deleted value on its return, because the tombstone
    /// is kept until that replica has seen it.
    async fn clear_expired_tombstones(&self) {
        loop {
            for key in self.tombstones.expired() {
                // Version token of the tombstone actually stored, matched against peer acks.
                let version = self.engine.map.read().get(&key).map(version_hash);
                let Some(version) = version else {
                    // The key is no longer present (overwritten or already removed): stop
                    // tracking it.
                    self.tombstones.remove(&key);
                    continue;
                };
                if self.engine.is_tombstone_stable(&key, version) {
                    self.tombstones.remove(&key);
                    // Remove from the dated map *and* the value-only projection together.
                    self.engine.gc_remove(&key);
                    self.engine.forget_tombstone(&key);
                }
                // Otherwise keep the tombstone and re-check on a later iteration.
            }
            tokio::time::sleep(TOMBSTONE_CLEARING).await;
        }
    }

    #[instrument(name = "reconcile.store", skip_all)]
    pub async fn run(self) {
        info!("reconcile store starting");
        let tombstones = self.clone();
        let snapshots = self.clone();
        tokio::join!(
            self.engine.run(),
            tombstones.clear_expired_tombstones(),
            snapshots.snapshot_periodically(),
        );
    }
}

impl<
        K: Clone + Debug + DeserializeOwned + Hash + Ord + Send + Serialize + Sync + 'static,
        V: Clone + DeserializeOwned + Hash + Send + Serialize + Sync + 'static,
    > ReconcileStore<K, V>
{
    pub fn get_mut<F: FnOnce(Option<&mut V>)>(&self, k: &K, callback: F) {
        use crate::reconcilable::Projectable;
        let mut guard = self.engine.map.write();
        guard.with_mut(k, |maybe_tv| {
            if let Some((_, v)) = maybe_tv {
                callback(v.as_mut());
            } else {
                callback(None);
            }
        });
        // The in-place mutation bypassed `insert`, so refresh the value-only projection for this
        // key directly (lock order map → projection, as everywhere else) to keep it in sync.
        if let Some(tv) = guard.get(k) {
            let projected = tv.project();
            self.engine.projection.write().insert(k.clone(), projected);
        }
    }
}

/// Maximum number of geographical networks (CIDRs) a [`Config`] can declare. A fixed-size array
/// keeps [`Config`] `Copy`; eight networks is generous for real geographical deployments.
pub const MAX_NETS: usize = 8;

#[derive(Clone, Copy)]
pub struct Config {
    pub port: u16,
    pub listen_addr: IpAddr,
    /// The geographical networks the cluster spans, each a CIDR (issue #53). Empty slots are `None`.
    ///
    /// Declare every network with [`with_net`](Config::with_net); a *network* is just an address
    /// range, sized to your topology (a whole cloud region, an availability zone, or a single
    /// subnet). This node's **local** network is whichever declared net contains
    /// [`listen_addr`](Self::listen_addr); all others are **remote**. (If none contains it, the node
    /// logs a loud warning and treats only itself as local — see [`ReconcileStore::new`].)
    ///
    /// A node auto-discovers peers in every network (one random probe per network per round) but
    /// gossips the full anti-entropy comparison to local-network peers every round and to
    /// remote-network peers only every [`remote_interval`](Self::remote_interval) rounds, to a
    /// bounded [`remote_fanout`](Self::remote_fanout) subset — so cross-network (WAN) traffic stays
    /// bounded. A peer's network is derived purely from its IP (`IpNet::contains`), so the wire
    /// format is unchanged. With no network declared, the node uses the loopback default and behaves
    /// like a flat single-CIDR cluster (the historical behaviour).
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
    /// When `None` (the default), the protocol is **unauthenticated**: any host able to send a
    /// UDP datagram to the port can forge updates and poison the cluster. When set, every node in
    /// the cluster must use the **same** 32-byte key (and the same MAC backend feature). See
    /// [`Config::with_cluster_key`].
    pub cluster_key: Option<[u8; 32]>,
    /// Identity of this node, used as the deterministic tie-break in the Hybrid Logical
    /// Clock total order. When `None` (the default), a random id is generated at startup.
    ///
    /// Two nodes must not share the same id, or conflicts between equal `(wall, counter)`
    /// timestamps would no longer be resolved deterministically. A random 64-bit id makes
    /// collisions negligible; set an explicit id only when you need a stable, reproducible
    /// ordering (e.g. in tests).
    pub node_id: Option<u64>,
    /// Whether to encrypt datagram payloads (not just authenticate them).
    ///
    /// Only ever set through `Config::with_encryption` (available with the `encryption` cargo
    /// feature), which also requires a [`cluster_key`](Self::cluster_key). When `false` (the
    /// default), a set cluster key authenticates plaintext datagrams with a MAC; when `true`, the
    /// same key is used to authenticate *and* encrypt them with XChaCha20-Poly1305.
    pub encrypt: bool,
    /// How long the reconciliation loop waits for inbound activity before initiating a round — the
    /// effective gossip cadence (default 1 s). A shorter interval converges faster at the cost of
    /// more traffic. Retunable at runtime via
    /// [`set_reconcile_interval`](crate::ReconcileStore::set_reconcile_interval).
    pub reconcile_interval: Duration,
    // may include other options in the future: tombstone_ttl, metrics, etc.
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
            node_id: None,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
        }
    }
}
impl Config {
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    pub fn with_listen_addr(mut self, listen_addr: IpAddr) -> Self {
        self.listen_addr = listen_addr;
        self
    }
    /// Declare a geographical network the cluster spans, by its CIDR (issue #53).
    ///
    /// Call once per network — **including this node's own**. The node's local network is whichever
    /// declared net contains [`listen_addr`](Config::listen_addr); the rest are remote and gossiped
    /// to over WAN-bounded, geography-aware anti-entropy (see [`nets`](Config::nets)). With none
    /// declared, the node uses the loopback default (a flat single-CIDR cluster).
    ///
    /// # Panics
    ///
    /// Panics if more than [`MAX_NETS`] networks are declared.
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
    /// Panics if the total number of networks exceeds [`MAX_NETS`].
    pub fn with_nets(mut self, nets: &[IpNet]) -> Self {
        for &net in nets {
            self = self.with_net(net);
        }
        self
    }

    /// Set how often (in reconciliation rounds) the full anti-entropy comparison is sent to
    /// remote-network peers (default `6`). See [`remote_interval`](Config::remote_interval).
    pub fn with_remote_interval(mut self, interval: u32) -> Self {
        self.remote_interval = interval;
        self
    }

    /// Set the maximum number of peers contacted per remote network on each cross-network round
    /// (default `2`). See [`remote_fanout`](Config::remote_fanout).
    pub fn with_remote_fanout(mut self, fanout: usize) -> Self {
        self.remote_fanout = fanout;
        self
    }

    /// Set the reconciliation cadence: how long the loop waits for inbound activity before
    /// initiating a round (default 1 s). See [`reconcile_interval`](Config::reconcile_interval).
    /// Retunable at runtime via
    /// [`ReconcileStore::set_reconcile_interval`](crate::ReconcileStore::set_reconcile_interval).
    pub fn with_reconcile_interval(mut self, interval: Duration) -> Self {
        self.reconcile_interval = interval;
        self
    }

    /// Enable per-datagram MAC authentication using a shared 32-byte cluster secret.
    ///
    /// Every outgoing datagram is then framed with a keyed MAC over its payload, and every
    /// incoming datagram is verified **before** deserialization; datagrams with a missing or
    /// invalid tag are silently dropped. This closes the unauthenticated last-write-wins
    /// poisoning vector.
    ///
    /// All nodes in the same cluster **must** share the identical key and be built with the same
    /// MAC backend feature (`mac-blake3`, the default, or `mac-hmac`). Without a key, the store
    /// logs a loud warning at startup and runs unauthenticated.
    pub fn with_cluster_key(mut self, key: [u8; 32]) -> Self {
        self.cluster_key = Some(key);
        self
    }

    /// Set an explicit node identity for the Hybrid Logical Clock tie-break.
    ///
    /// Each node in a cluster must use a distinct id. Leave unset to use a random id (the
    /// default); set it for a stable, reproducible conflict ordering.
    pub fn with_node_id(mut self, node_id: u64) -> Self {
        self.node_id = Some(node_id);
        self
    }

    /// Encrypt datagram payloads with XChaCha20-Poly1305, upgrading the keyed protocol from
    /// authentication-only to authenticated **encryption** (issue #96).
    ///
    /// This reuses the shared [`cluster_key`](Self::cluster_key) as the AEAD key, so
    /// [`with_cluster_key`](Self::with_cluster_key) must also be set on every node; a node
    /// without a key will not converge with encrypted peers. Each datagram is framed as
    /// `nonce || ciphertext || tag`, adding 40 bytes of overhead, and is decrypted-and-verified
    /// before deserialization; datagrams that fail are silently dropped.
    ///
    /// The trust model is unchanged from the MAC mode: a single shared secret, so any holder can
    /// read and write. It provides confidentiality and integrity but **not** per-peer identity,
    /// forward secrecy, or replay protection — those would require a handshake (TLS/Noise), which
    /// is intentionally out of scope.
    ///
    /// Requires the `encryption` cargo feature.
    #[cfg(feature = "encryption")]
    pub fn with_encryption(mut self) -> Self {
        self.encrypt = true;
        self
    }
}

#[cfg(test)]
mod reconcile_store_tests {
    use std::net::IpAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::reconcile_engine::version_hash;
    use crate::{
        reconcile_store::{Config, MAX_NETS},
        FileSnapshot, ReconcileStore,
    };

    /// A config bound to an ephemeral UDP port on loopback, so persistence tests can construct
    /// stores without colliding on a fixed port.
    fn ephemeral_config() -> Config {
        Config {
            port: 0,
            listen_addr: "127.0.0.1".parse().unwrap(),
            nets: [None; MAX_NETS],
            remote_interval: 6,
            remote_fanout: 2,
            cluster_key: None,
            node_id: None,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn tombstones_expiration() {
        // A dedicated port (and a singleton /32 net) isolates this test from the integration
        // tests: peer discovery probes random addresses in the net on this port, so an overlapping
        // port lets a concurrently-running test's node inject updates here.
        let config = Config::default()
            .with_port(8090)
            .with_listen_addr("127.0.0.45".parse().unwrap())
            .with_net("127.0.0.45/32".parse().unwrap());
        let store = ReconcileStore::<i32, i32>::new(config)
            .await
            .with_tombstone_timeout(Duration::from_millis(1));

        // This test exercises the tombstone TimeoutWheel directly; it deliberately does *not*
        // spawn `run()`, whose periodic causal-stability-gated GC would itself remove the
        // expired tombstone and race these assertions.

        // insert a tombstone
        store.remove(&0);
        tokio::time::sleep(Duration::from_millis(10)).await; // await its expiration
                                                             // The tombstone should be expired by now
        assert_eq!(store.tombstones.expired(), vec![0]);
        assert_eq!(store.tombstones.remove(&0), Some(0));
        assert_eq!(store.tombstones.remove(&0), None);
    }

    /// A durable backend must let a restarted store recover both live values and tombstones, with
    /// identical timestamps (hence an identical fingerprint).
    #[tokio::test]
    async fn persistence_roundtrip_recovers_entries_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");

        let store = ReconcileStore::<i32, i32>::new(ephemeral_config())
            .await
            .with_persistence(Arc::new(FileSnapshot::new(&path)));
        store.insert(1, 11); // live value
        store.insert(2, 22);
        store.remove(&2); // tombstone
        let expected = store.fingerprint(..);
        store.snapshot(); // force a durable write

        // A brand-new store recovers the previous state from the same file.
        let restarted = ReconcileStore::<i32, i32>::new(ephemeral_config())
            .await
            .with_persistence(Arc::new(FileSnapshot::new(&path)));
        assert_eq!(restarted.get(&1).as_deref(), Some(&11));
        assert!(restarted.get(&2).is_none(), "tombstone was not recovered");
        assert_eq!(
            restarted.fingerprint(..),
            expected,
            "recovered state must hash identically (timestamps preserved)"
        );
        // The recovered tombstone is back in the expiry wheel (replayed through the hook).
        assert!(restarted.tombstones.remove(&2).is_some());
    }

    /// The causal-stability state from issue #109 (membership + per-tombstone acks) must survive a
    /// restart, otherwise GC gating is lost.
    #[tokio::test]
    async fn restart_preserves_membership_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        let peer: IpAddr = "127.0.0.99".parse().unwrap();

        let store = ReconcileStore::<i32, i32>::new(ephemeral_config())
            .await
            .with_persistence(Arc::new(FileSnapshot::new(&path)));
        store.engine.members.write().insert(peer);
        store.insert(5, 55);
        store.remove(&5); // tombstone
        store
            .engine
            .tombstone_acks
            .write()
            .entry(5)
            .or_default()
            .insert(peer, 123);
        store.snapshot();

        let restarted = ReconcileStore::<i32, i32>::new(ephemeral_config())
            .await
            .with_persistence(Arc::new(FileSnapshot::new(&path)));
        assert!(
            restarted.engine.members.read().contains(&peer),
            "membership set was not restored"
        );
        assert_eq!(
            restarted
                .engine
                .tombstone_acks
                .read()
                .get(&5)
                .and_then(|acks| acks.get(&peer)),
            Some(&123),
            "tombstone acknowledgments were not restored"
        );
    }

    /// Regression for issue #122 ↔ #109: a restart must not turn a held-back tombstone into a
    /// collectable (and thus resurrectable) one. A fresh, empty store would treat the tombstone as
    /// causally stable (no members ⇒ GC allowed); a store that recovered its membership must still
    /// gate GC because the unacknowledged peer is restored too.
    #[tokio::test]
    async fn restart_keeps_tombstone_gc_gated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.bin");
        let peer: IpAddr = "127.0.0.98".parse().unwrap();

        let store = ReconcileStore::<i32, i32>::new(ephemeral_config())
            .await
            .with_persistence(Arc::new(FileSnapshot::new(&path)));
        store.engine.members.write().insert(peer);
        store.insert(1, 11);
        store.remove(&1); // tombstone, never acknowledged by `peer`

        let version = store.engine.map.read().get(&1).map(version_hash).unwrap();
        assert!(
            !store.engine.is_tombstone_stable(&1, version),
            "precondition: tombstone is gated before restart"
        );
        store.snapshot();

        // Sanity check the hazard: a *fresh* store (no recovered membership) would consider the
        // same tombstone stable and collect it.
        let fresh = ReconcileStore::<i32, i32>::new(ephemeral_config()).await;
        fresh.insert(1, 11);
        fresh.remove(&1);
        let fresh_version = fresh.engine.map.read().get(&1).map(version_hash).unwrap();
        assert!(
            fresh.engine.is_tombstone_stable(&1, fresh_version),
            "a fresh restart with no membership would (wrongly) GC the tombstone — the hazard #122 describes"
        );

        // The recovered store keeps the tombstone gated, preventing resurrection.
        let restarted = ReconcileStore::<i32, i32>::new(ephemeral_config())
            .await
            .with_persistence(Arc::new(FileSnapshot::new(&path)));
        assert!(restarted.get(&1).is_none(), "tombstone was not recovered");
        let version = restarted
            .engine
            .map
            .read()
            .get(&1)
            .map(version_hash)
            .unwrap();
        assert!(
            !restarted.engine.is_tombstone_stable(&1, version),
            "restart dropped causal-stability state: tombstone would be GC'd and could resurrect"
        );
    }
}
