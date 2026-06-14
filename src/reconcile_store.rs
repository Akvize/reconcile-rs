// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReconcileStore`], a wrapper to a key-value map
//! to enable reconciliation between different instances over a network.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::net::IpAddr;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ipnet::IpNet;
use parking_lot::{MappedRwLockReadGuard, RwLockReadGuard};
use tracing::{debug, info, instrument, warn};

use crate::bounds::{Key, Value};
use crate::clock::{Timestamp, DEFAULT_MAX_CLOCK_DRIFT_MS};
use crate::discovery::{Discovery, DnsDiscovery};
use crate::fingerprint::Fingerprint;
use crate::persistence::{DatedEntries, InMemoryPersistence, PersistedState, Persistence};
use crate::reconcilable::Projectable;
use crate::reconcile_engine::{version_hash, ReconcileEngine};
use crate::timeout_wheel::TimeoutWheel;

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// How often the background task writes a full snapshot to the persistence backend.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

/// Default cadence of the dynamic-discovery task (see [`ReconcileStore::with_discovery_interval`]).
const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(5);

/// Default number of consecutive discovery rounds a member may be absent before it is
/// decommissioned (see [`ReconcileStore::with_discovery_miss_threshold`]).
const DEFAULT_DISCOVERY_MISS_THRESHOLD: u32 = 3;

/// Default rate, in bytes per second, at which a single bulk anti-entropy value transfer to one
/// peer is metered (see [`Config::bulk_send_rate`]). 32 MiB/s keeps a cold-sync fast
/// while spacing datagrams (~2 ms apart at the 64 KiB datagram size) so the burst cannot overrun the
/// receiver's socket buffer, and so the receiver keeps being fed and does not re-issue a full diff
/// over ranges still in flight (the byte amplification this caps).
const DEFAULT_BULK_SEND_RATE: usize = 32 * 1024 * 1024;

/// Default size requested for the gossip socket's send/receive buffers (`SO_SNDBUF` / `SO_RCVBUF`),
/// in bytes (see [`Config::recv_buffer_size`]). A generous 8 MiB: the kernel silently clamps the
/// request to the OS maximum (`net.core.rmem_max` / `wmem_max` on Linux), so this is an improvement
/// on a tuned host and a harmless no-op on an untuned one. The stock OS default holds only a handful
/// of full-size datagrams, which a bulk/cold sync burst overruns — dropping datagrams in the kernel
/// before the application ever sees them.
const DEFAULT_SOCKET_BUFFER_SIZE: usize = 8 * 1024 * 1024;

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
    (Timestamp, Option<V>): Projectable,
{
    /// Internal map and hooks container.
    engine: ReconcileEngine<K, (Timestamp, Option<V>)>,
    /// Tombstone timestamps for deleted entries.
    tombstones: TimeoutWheel<K>,
    /// Durable backend. Always present (the trait is mandatory); defaults to the non-durable
    /// [`InMemoryPersistence`], swapped out via [`with_persistence`](ReconcileStore::with_persistence).
    persistence: Arc<dyn Persistence<K, V>>,
    /// Optional dynamic peer-discovery source (e.g. Kubernetes DNS). When `None` (the default),
    /// discovery falls back entirely to the per-network random probing in the engine; when set, a
    /// background task injects the discovered peers and decommissions vanished ones.
    discovery: Option<Arc<dyn Discovery>>,
    /// How often the discovery task resolves the peer set.
    discovery_interval: Duration,
    /// Consecutive missed discovery rounds before a vanished member is decommissioned.
    discovery_miss_threshold: u32,
}

impl<K, V> Clone for ReconcileStore<K, V>
where
    K: Clone + Hash + std::cmp::Eq + Send + Sync,
    (Timestamp, Option<V>): Projectable,
{
    /// Allows cloning of the `ReconcileStore` handle for lightweight sharing in hooks or tests.
    fn clone(&self) -> Self {
        ReconcileStore {
            engine: self.engine.clone(),
            tombstones: self.tombstones.clone(),
            persistence: self.persistence.clone(),
            discovery: self.discovery.clone(),
            discovery_interval: self.discovery_interval,
            discovery_miss_threshold: self.discovery_miss_threshold,
        }
    }
}

impl<K: Key, V: Value> ReconcileStore<K, V> {
    /// Create a new `ReconcileStore`, set up network and tombstones.
    pub async fn new(config: Config) -> Self {
        let svc = ReconcileStore {
            engine: ReconcileEngine::<K, (Timestamp, Option<V>)>::new(config).await,
            tombstones: TimeoutWheel::new(),
            persistence: Arc::new(InMemoryPersistence::default()),
            discovery: None,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            discovery_miss_threshold: DEFAULT_DISCOVERY_MISS_THRESHOLD,
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

    /// Register (or refresh) a known peer **at runtime**, the `&self` counterpart of
    /// [`with_seed`](Self::with_seed).
    ///
    /// This re-arms the peer-expiration window and makes the address a gossip target on the next
    /// round. It is what a dynamic discovery source feeds into the store; it never grants
    /// causal-stability membership (which a peer must still earn via an authenticated dated
    /// datagram), so injecting an unverified address can never block tombstone garbage collection.
    pub fn seed_peer(&self, peer: IpAddr) {
        self.engine.seed_peer(peer);
    }

    /// Attach an **authoritative** peer-discovery source (e.g. [`DnsDiscovery`]) that maintains the
    /// known-peer set, on top of the engine's default speculative [`RandomProbe`](crate::RandomProbe).
    ///
    /// While the store is [`run`](Self::run)ning, a background task calls
    /// [`Discovery::discover`] every
    /// [`discovery_interval`](Self::with_discovery_interval), seeds every returned address as a
    /// known peer, and — after a member has been absent for
    /// [`discovery_miss_threshold`](Self::with_discovery_miss_threshold) consecutive successful
    /// rounds — decommissions it (see [`forget_peer`](Self::forget_peer)) so its tombstones stop
    /// gating garbage collection. See [`with_dns_discovery`](Self::with_dns_discovery) for the
    /// Kubernetes-native default.
    ///
    /// The source must be [authoritative](crate::Discovery::is_authoritative): an absence is read as
    /// a vanished peer and drives decommissioning. A speculative source like
    /// [`RandomProbe`](crate::RandomProbe) belongs in the engine's per-round probe, not here.
    pub fn with_discovery(mut self, discovery: Arc<dyn Discovery>) -> Self {
        debug_assert!(
            discovery.is_authoritative(),
            "with_discovery expects an authoritative source; a speculative prober would be seeded \
             as permanent known peers and its absences would wrongly decommission members"
        );
        self.discovery = Some(discovery);
        self
    }

    /// Discover peers by resolving a DNS name — the Kubernetes-native default.
    ///
    /// Point `name` at a **headless** `Service` (`clusterIP: None`): its DNS name resolves to one
    /// address record per ready pod, so the store learns every peer with a single lookup, with no
    /// Kubernetes API client and no RBAC. `port` is the reconciliation UDP port. Shorthand for
    /// [`with_discovery`](Self::with_discovery) with a [`DnsDiscovery`].
    pub fn with_dns_discovery(self, name: impl Into<String>, port: u16) -> Self {
        self.with_discovery(Arc::new(DnsDiscovery::new(name, port)))
    }

    /// Set how often the discovery task resolves the peer set (default 5 s). Only relevant when a
    /// discovery source is configured via [`with_discovery`](Self::with_discovery).
    pub fn with_discovery_interval(mut self, interval: Duration) -> Self {
        self.discovery_interval = interval;
        self
    }

    /// Set how many consecutive successful discovery rounds a previously-seen member may be absent
    /// before it is decommissioned (default 3). A higher value tolerates longer DNS blips / rolling
    /// restarts at the cost of holding tombstones (and their GC gate) longer.
    pub fn with_discovery_miss_threshold(mut self, threshold: u32) -> Self {
        self.discovery_miss_threshold = threshold;
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
    pub fn add_pre_insert<F: Send + Sync + Fn(&K, &(Timestamp, Option<V>)) + 'static>(
        &self,
        pre_insert: F,
    ) {
        let tombstones = self.tombstones.clone();
        let wrapped_pre_insert = move |k: &K, v: &(Timestamp, Option<V>)| {
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
    /// non-commutative tie-break that caused permanent divergence.
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

    /// Drive the dynamic discovery source: inject discovered peers and decommission vanished ones.
    ///
    /// Returns immediately (a no-op) when no discovery source is configured, so the default
    /// random-probing behaviour is unchanged. Otherwise, every
    /// [`discovery_interval`](Self::with_discovery_interval):
    ///
    /// - On a **successful** resolution, every returned address (except this node's own) is seeded
    ///   as a known peer, re-arming its expiration and feeding it to the next gossip round.
    /// - A previously-seen **member** that is now absent has its miss count incremented; once it
    ///   reaches [`discovery_miss_threshold`](Self::with_discovery_miss_threshold) it is
    ///   decommissioned, releasing the causal-stability gate it held on tombstone GC.
    /// - A **failed** resolution (DNS blip) is skipped entirely — it never counts as a miss — so a
    ///   transient resolver hiccup cannot decommission a healthy peer.
    ///
    /// Only `members` are considered for decommissioning: membership is what gates tombstone GC, it
    /// is earned through a real authenticated datagram, and discovery never writes to it — so a
    /// spoofable or never-contacted address can neither block nor be the subject of GC release.
    async fn discover_periodically(&self) {
        let Some(discovery) = self.discovery.clone() else {
            return; // no discovery source: leave peer-finding to the engine's per-net probing
        };
        let own_addr = self.engine.listen_addr();
        // Consecutive missed rounds per member, and the set of addresses discovery has ever
        // reported (so we only grace-decommission members we actually discovered, never peers
        // learned by other means).
        let mut miss_counts: HashMap<IpAddr, u32> = HashMap::new();
        let mut seen_ever: HashSet<IpAddr> = HashSet::new();
        loop {
            tokio::time::sleep(self.discovery_interval).await;
            let resolved = match discovery.discover().await {
                Ok(addrs) => addrs,
                Err(err) => {
                    // Transient failure: do not touch miss counts, do not decommission anyone.
                    debug!("discovery round failed, skipping: {err}");
                    continue;
                }
            };
            let current: HashSet<IpAddr> = resolved
                .into_iter()
                .filter(|addr| *addr != own_addr)
                .collect();
            // 1) Refresh every currently-present peer.
            for addr in &current {
                seen_ever.insert(*addr);
                self.engine.seed_peer(*addr);
                miss_counts.remove(addr);
            }
            // 2) Grace-account members that were discovered before but are now absent.
            for member in self.engine.members_snapshot() {
                if member == own_addr || current.contains(&member) || !seen_ever.contains(&member) {
                    continue;
                }
                let misses = miss_counts.entry(member).or_insert(0);
                *misses += 1;
                if *misses >= self.discovery_miss_threshold {
                    info!("decommissioning vanished peer {member} after {misses} missed discovery rounds");
                    self.engine.decommission_peer(member);
                    miss_counts.remove(&member);
                    seen_ever.remove(&member);
                }
            }
        }
    }

    #[instrument(name = "reconcile.store", skip_all)]
    pub async fn run(self) {
        info!("reconcile store starting");
        let tombstones = self.clone();
        let snapshots = self.clone();
        let discovery = self.clone();
        tokio::join!(
            self.engine.run(),
            tombstones.clear_expired_tombstones(),
            snapshots.snapshot_periodically(),
            discovery.discover_periodically(),
        );
    }
}

// `get_mut` shares the same `V: Value` bound as the rest of the store: an in-place edit is
// re-stamped and broadcast (see below), and that broadcast serializes `V` through the engine's
// `Value`-bounded path, so the bound set matches `insert`/`remove` exactly.
impl<K: Key, V: Value> ReconcileStore<K, V> {
    /// Mutate the value for `k` in place, then propagate the edit like [`insert`](ReconcileStore::insert).
    ///
    /// The callback receives `Some(&mut V)` when the key is live, or `None` when it is absent or
    /// tombstoned. When it mutated a live value, the entry is then **re-stamped with a fresh Hybrid
    /// Logical Clock timestamp and broadcast** to peers, exactly as a regular `insert` would be — so
    /// an in-place edit converges across the cluster instead of remaining local. The whole
    /// read-modify-write holds the map write lock for the duration of the callback, so it is atomic
    /// against the reconciliation loop.
    pub fn get_mut<F: FnOnce(Option<&mut V>)>(&self, k: &K, callback: F) {
        use crate::reconcilable::Projectable;
        // Mint the timestamp before taking the map lock, matching the lock order of `insert`
        // (clock, then map → projection).
        let now = self.engine.clock_now();
        let mut updated: Option<(Timestamp, Option<V>)> = None;
        let mut guard = self.engine.map.write();
        guard.with_mut(k, |maybe_tv| {
            if let Some(tv) = maybe_tv {
                callback(tv.1.as_mut());
                // Re-stamp so the edit wins last-write-wins on peers and reconciles; `with_mut`
                // recomputes the dated fingerprint from the whole (timestamp, value) afterwards.
                tv.0 = now;
                updated = Some(tv.clone());
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
        drop(guard);
        // Notify peers of the re-stamped entry, just like `insert`, so the edit propagates eagerly
        // rather than being left to the next anti-entropy round (or failing to converge at all).
        if let Some(value) = updated {
            self.engine.broadcast_update(k.clone(), value);
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
    /// The geographical networks the cluster spans, each a CIDR. Empty slots are `None`.
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
    /// Maximum milliseconds a remote stamp may lead local physical time before the Hybrid Logical
    /// Clock clamps it when advancing local clock state (default [`DEFAULT_MAX_CLOCK_DRIFT_MS`],
    /// i.e. 1 hour).
    ///
    /// The clamp stops a single far-future stamp — which any sender can inject in the default
    /// unauthenticated mode — from permanently pinning every node's clock and destroying LWW
    /// recency. It bounds only how far the *local clock* chases a remote stamp; a stored value keeps
    /// its own stamp as LWW data. Lower it to tighten the bound on tolerated skew, or raise it for
    /// environments with legitimately larger clock drift. See
    /// [`with_max_clock_drift_ms`](Config::with_max_clock_drift_ms).
    pub max_clock_drift_ms: u64,
    /// Whether to encrypt datagram payloads (not just authenticate them).
    ///
    /// Only ever set through `Config::with_encryption` (available with the `encryption` cargo
    /// feature), which also requires a [`cluster_key`](Self::cluster_key). When `false` (the
    /// default), a set cluster key authenticates plaintext datagrams with a MAC; when `true`, the
    /// same key is used to authenticate *and* encrypt them with XChaCha20-Poly1305.
    pub encrypt: bool,
    /// How long the reconciliation loop waits for inbound activity before initiating a round — the
    /// effective **background** anti-entropy cadence (default 1 s). It governs only divergence
    /// detection and loss repair: local writes propagate immediately (broadcast), independent of it.
    ///
    /// A shorter interval repairs dropped/missed updates sooner, but background traffic grows roughly
    /// as `1/interval` per local peer. It is also **counterproductive below a floor** of a few × the
    /// peer RTT (and below the bulk pacing gap — see [`bulk_send_rate`](Self::bulk_send_rate)): the
    /// diff is multi-round-trip, so re-initiating before the previous round's reply lands — or
    /// between a bulk transfer's paced datagrams — only floods the single receive loop with redundant
    /// comparisons, slowing convergence rather than speeding it. Keep it ≥ a few × RTT;
    /// the 1 s default is a safe, low-overhead choice. Retunable at runtime via
    /// [`set_reconcile_interval`](crate::ReconcileStore::set_reconcile_interval).
    pub reconcile_interval: Duration,
    /// Rate, in bytes per second, at which a single **bulk** anti-entropy value transfer to one peer
    /// is metered. Defaults to 32 MiB/s; `None` disables pacing (the historical
    /// back-to-back burst).
    ///
    /// When a peer differs over a whole range — most starkly a cold/empty peer pulling the entire
    /// dataset — the holder would otherwise dump every value back-to-back in one burst. That burst
    /// overruns the receiver's socket buffer (datagrams are dropped in the kernel) and, because the
    /// receiver applies updates silently, the holder sees a lull and its own
    /// [`reconcile_interval`](Self::reconcile_interval) timer re-issues a full diff over ranges still
    /// in flight, which are then re-sent — inflating bytes transferred far beyond the dataset size.
    /// Pacing the transfer on a background task (off the receive loop) keeps it below the receiver's
    /// overrun threshold; the engine additionally sends at most one bulk transfer per peer at a time,
    /// so a re-issued diff cannot re-send in-flight ranges. Together these make a cold sync transfer
    /// ≈ the dataset size, fast, regardless of `reconcile_interval`. Only the bulk value dump is
    /// paced; small refinement comparisons, acks, and local-write broadcasts are sent immediately.
    pub bulk_send_rate: Option<usize>,
    /// Size to request for the gossip socket's receive buffer (`SO_RCVBUF`), in bytes.
    ///
    /// Defaults to 8 MiB. The kernel clamps the request to the OS
    /// maximum (`net.core.rmem_max` on Linux) and never errors for asking too much, so a large
    /// default is safe — it only ever helps. The stock OS default holds only a few full-size
    /// datagrams, so a bulk/cold-sync burst overruns it and the kernel silently drops the excess
    /// (counted as `Udp.RcvbufErrors` in `/proc/net/snmp`, invisible to the application); a
    /// multi-MiB buffer absorbs the burst instead. Set to `None` to leave the inherited OS default
    /// untouched. See [`with_recv_buffer_size`](Config::with_recv_buffer_size).
    pub recv_buffer_size: Option<usize>,
    /// Size to request for the gossip socket's send buffer (`SO_SNDBUF`), in bytes.
    ///
    /// Defaults to 8 MiB, clamped by the kernel to the OS maximum
    /// (`net.core.wmem_max` on Linux). A larger send buffer lets a bulk send burst queue in the
    /// kernel instead of failing with `EWOULDBLOCK`/`ENOBUFS` (which the engine must then retry).
    /// Set to `None` to leave the inherited OS default untouched. See
    /// [`with_send_buffer_size`](Config::with_send_buffer_size).
    pub send_buffer_size: Option<usize>,
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
            max_clock_drift_ms: DEFAULT_MAX_CLOCK_DRIFT_MS,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
            bulk_send_rate: Some(DEFAULT_BULK_SEND_RATE),
            recv_buffer_size: Some(DEFAULT_SOCKET_BUFFER_SIZE),
            send_buffer_size: Some(DEFAULT_SOCKET_BUFFER_SIZE),
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
    /// Declare a geographical network the cluster spans, by its CIDR.
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

    /// Set how far (in milliseconds) a remote stamp may lead local physical time before the Hybrid
    /// Logical Clock clamps it (default [`DEFAULT_MAX_CLOCK_DRIFT_MS`], i.e. 1 hour). See
    /// [`max_clock_drift_ms`](Config::max_clock_drift_ms).
    pub fn with_max_clock_drift_ms(mut self, max_clock_drift_ms: u64) -> Self {
        self.max_clock_drift_ms = max_clock_drift_ms;
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

    /// Set the rate, in bytes per second, at which a single bulk anti-entropy value transfer to one
    /// peer is paced (default 32 MiB/s). See [`bulk_send_rate`](Config::bulk_send_rate); to disable
    /// pacing (the historical back-to-back burst), set that field to `None` directly.
    pub fn with_bulk_send_rate(mut self, bytes_per_sec: usize) -> Self {
        self.bulk_send_rate = Some(bytes_per_sec);
        self
    }

    /// Request `size` bytes for the gossip socket's receive buffer (`SO_RCVBUF`).
    ///
    /// The kernel clamps the request to the OS maximum (`net.core.rmem_max` on Linux), so passing
    /// more than the host allows is harmless — it is simply capped. Raising this (together with the
    /// matching sysctl) is the cheap fix for datagrams dropped during bulk/cold syncs.
    /// See [`recv_buffer_size`](Config::recv_buffer_size); to leave the OS default untouched, set
    /// that field to `None` directly.
    pub fn with_recv_buffer_size(mut self, size: usize) -> Self {
        self.recv_buffer_size = Some(size);
        self
    }

    /// Request `size` bytes for the gossip socket's send buffer (`SO_SNDBUF`).
    ///
    /// Clamped by the kernel to the OS maximum (`net.core.wmem_max` on Linux). See
    /// [`send_buffer_size`](Config::send_buffer_size); to leave the OS default untouched, set that
    /// field to `None` directly.
    pub fn with_send_buffer_size(mut self, size: usize) -> Self {
        self.send_buffer_size = Some(size);
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
    /// authentication-only to authenticated **encryption**.
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
            max_clock_drift_ms: super::DEFAULT_MAX_CLOCK_DRIFT_MS,
            encrypt: false,
            reconcile_interval: Duration::from_secs(1),
            bulk_send_rate: Some(super::DEFAULT_BULK_SEND_RATE),
            recv_buffer_size: Some(super::DEFAULT_SOCKET_BUFFER_SIZE),
            send_buffer_size: Some(super::DEFAULT_SOCKET_BUFFER_SIZE),
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

    /// The causal-stability state (membership + per-tombstone acks) must survive a
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

    /// Regression: a restart must not turn a held-back tombstone into a
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
            "a fresh restart with no membership would (wrongly) GC the tombstone — the hazard this guards against"
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

    use std::sync::Mutex;

    use crate::discovery::{DiscoverFuture, Discovery};

    /// A scriptable discovery source for the grace/decommission tests. The test thread swaps the
    /// response while the discovery loop runs.
    #[derive(Clone)]
    struct FakeDiscovery {
        resp: Arc<Mutex<FakeResp>>,
    }

    #[derive(Clone)]
    enum FakeResp {
        /// A successful resolution returning this peer set.
        Present(Vec<IpAddr>),
        /// A transient failure (DNS blip).
        Blip,
    }

    impl FakeDiscovery {
        fn new(initial: FakeResp) -> Self {
            FakeDiscovery {
                resp: Arc::new(Mutex::new(initial)),
            }
        }
        fn set(&self, resp: FakeResp) {
            *self.resp.lock().unwrap() = resp;
        }
    }

    impl Discovery for FakeDiscovery {
        fn discover(&self) -> DiscoverFuture<'_> {
            let resp = self.resp.lock().unwrap().clone();
            Box::pin(async move {
                match resp {
                    FakeResp::Present(addrs) => Ok(addrs),
                    FakeResp::Blip => Err(std::io::Error::other("blip")),
                }
            })
        }
    }

    fn discovery_config() -> Config {
        // A real, bindable loopback address (the engine binds a socket in `new`) on an ephemeral
        // port. No `with_net`, mirroring the Kubernetes setup where discovery is purely DNS-driven.
        Config::default()
            .with_port(0)
            .with_listen_addr("127.0.0.1".parse().unwrap())
    }

    /// A member that vanishes from discovery for `miss_threshold` consecutive successful rounds is
    /// decommissioned; the node never decommissions itself even when absent from the result.
    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_decommissions_vanished_member_but_not_self() {
        let own: IpAddr = "127.0.0.1".parse().unwrap();
        let member: IpAddr = "127.0.0.200".parse().unwrap();

        let fake = FakeDiscovery::new(FakeResp::Present(vec![member]));
        let store = ReconcileStore::<i32, i32>::new(discovery_config())
            .await
            .with_discovery(Arc::new(fake.clone()))
            .with_discovery_interval(Duration::from_millis(20))
            .with_discovery_miss_threshold(3);

        // Seed membership as if both had been contacted via dated datagrams.
        store.engine.members.write().insert(own);
        store.engine.members.write().insert(member);

        let loop_store = store.clone();
        let handle = tokio::spawn(async move { loop_store.discover_periodically().await });

        // While the member is present in discovery it must not be decommissioned.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            store.engine.members.read().contains(&member),
            "present member was wrongly decommissioned"
        );

        // The member vanishes; after the miss threshold it is decommissioned, but self is kept.
        fake.set(FakeResp::Present(vec![]));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !store.engine.members.read().contains(&member),
            "vanished member was not decommissioned after the grace period"
        );
        assert!(
            store.engine.members.read().contains(&own),
            "node decommissioned itself"
        );

        handle.abort();
    }

    /// A transient discovery failure (DNS blip) must never decommission a member, however long it
    /// lasts; only a successful resolution that omits the member counts toward the grace threshold.
    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_blip_does_not_decommission() {
        let member: IpAddr = "127.0.0.201".parse().unwrap();

        // Report the member present once so it enters `seen_ever`, then fail forever.
        let fake = FakeDiscovery::new(FakeResp::Present(vec![member]));
        let store = ReconcileStore::<i32, i32>::new(discovery_config())
            .await
            .with_discovery(Arc::new(fake.clone()))
            .with_discovery_interval(Duration::from_millis(20))
            .with_discovery_miss_threshold(3);
        store.engine.members.write().insert(member);

        let loop_store = store.clone();
        let handle = tokio::spawn(async move { loop_store.discover_periodically().await });

        // Let the member be observed present at least once, then switch to permanent blips.
        tokio::time::sleep(Duration::from_millis(60)).await;
        fake.set(FakeResp::Blip);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            store.engine.members.read().contains(&member),
            "a transient discovery failure wrongly decommissioned a member"
        );

        // Sanity: a genuine absence still decommissions, proving the mechanism is live.
        fake.set(FakeResp::Present(vec![]));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !store.engine.members.read().contains(&member),
            "member was not decommissioned once it genuinely vanished"
        );

        handle.abort();
    }
}
