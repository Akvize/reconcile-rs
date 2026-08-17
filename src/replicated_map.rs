// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Provides the [`ReplicatedMap`], a wrapper to a key-value map
//! to enable reconciliation between different instances over a network.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::io;
use std::net::IpAddr;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gossip::auth::ClusterKey;
use ipnet::IpNet;
use parking_lot::RwLockReadGuard;
use tracing::{debug, info, instrument, warn};

use crate::bounds::{Key, Value};
use crate::clock::{BoundedInstant, ClockDrift, NodeId, StampBound, Timestamp, MAX_CLOCK_DRIFT};
use crate::discovery::{Discovery, DiscoveryKind, DnsDiscovery};
use crate::entry::Entry;
use crate::persistence::{DatedEntries, InMemoryPersistence, PersistedState, Persistence};
use crate::replica::{version_hash, Replica};
use crate::timeout_wheel::TimeoutWheel;
use crate::transport::Transport;
use crate::value_ref::ValueRef;
use rsos::Fingerprint;

const TOMBSTONE_CLEARING: Duration = Duration::from_secs(1);

/// How far a **stored** tombstone stamp may lead this node's physical time before the instant
/// derived from it — never the stamp itself — is capped.
///
/// The same budget as the clock's far-future clamp ([`MAX_CLOCK_DRIFT`]), which is not reachable
/// through the [`Clock`](crate::clock::Clock) port; if it ever reaches `Config`, this follows it.
const TOMBSTONE_STAMP_DRIFT_BUDGET: ClockDrift = MAX_CLOCK_DRIFT;

/// How often the background task writes a full snapshot to the persistence backend.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

/// Attempts [`with_persistence`](ReplicatedMap::with_persistence) makes to load persisted state
/// before giving up.
const LOAD_RETRY_ATTEMPTS: u32 = 5;

/// Base delay before the first load retry; each subsequent attempt doubles it (see
/// [`backoff_delay`]) — 100 ms, 200 ms, 400 ms, 800 ms, under 2 s of total backoff across
/// [`LOAD_RETRY_ATTEMPTS`].
const LOAD_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

/// Delay before retry `attempt` (1-indexed): `LOAD_RETRY_BASE_DELAY` doubled `attempt - 1` times.
fn backoff_delay(attempt: u32) -> Duration {
    LOAD_RETRY_BASE_DELAY * 2u32.pow(attempt - 1)
}

/// Entries cloned per map read-lock acquisition while building a snapshot (`Self::snapshot`).
///
/// Cloning the whole map under one continuous read lock stalls every writer for as long as the
/// clone takes — proportional to map size, unbounded. Chunking bounds a single stall to
/// the time to clone this many entries, releasing the lock between chunks so a waiting writer can
/// interleave. The resulting snapshot is not a single linearizable instant — later chunks can
/// reflect writes concurrent with earlier ones — but that is no different from what the gossip
/// protocol itself already reconciles range-by-range, and each individual entry is still read
/// atomically (`ARCHITECTURE.md` §5 invariant 8's per-key LWW model needs no more).
const SNAPSHOT_CHUNK_SIZE: usize = 4096;

/// Default cadence of the dynamic-discovery task (see [`ReplicatedMap::with_discovery_interval`]).
const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(5);

/// Default number of consecutive discovery rounds a member may be absent before it is
/// decommissioned (see [`ReplicatedMap::with_discovery_miss_threshold`]).
const DEFAULT_DISCOVERY_MISS_THRESHOLD: u32 = 3;

/// Default wall-time floor a member with pending unacknowledged tombstones must be absent for
/// before decommissioning (see [`ReplicatedMap::with_discovery_decommission_floor`]). Ten minutes
/// is far above any DNS blip, so it engages only against a sustainedly wrong resolver.
const DEFAULT_DISCOVERY_DECOMMISSION_FLOOR: Duration = Duration::from_secs(600);

/// Default metering rate of a single bulk transfer (see [`Config::bulk_send_rate`]). 32 MiB/s
/// spaces 64 KiB datagrams ~2 ms apart: fast, but under the receiver's socket-buffer overrun
/// threshold.
const DEFAULT_BULK_SEND_RATE: usize = 32 * 1024 * 1024;

/// Floor on a configured [`Config::bulk_send_rate`]. Below this, [`pace`](crate::replica::pace)
/// holds the per-peer in-flight mark across a sleep long enough to be effectively unbounded,
/// silently wedging that peer's sync for the duration of the dump. A value under the floor is
/// clamped up to it, with a warning — see #331.
pub(crate) const MIN_BULK_SEND_RATE: usize = 1024 * 1024;

/// Default `SO_SNDBUF`/`SO_RCVBUF` request (see [`Config::recv_buffer_size`]). 8 MiB: the kernel
/// clamps to the OS maximum, so it helps on a tuned host and costs nothing on an untuned one,
/// while the stock default holds too few datagrams for a cold-sync burst.
const DEFAULT_SOCKET_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Default cap on tracked peers (see [`Config::max_peers`]). Raise via
/// [`Config::with_max_peers`].
const DEFAULT_MAX_PEERS: usize = 1024;

/// Default cap on concurrent paced bulk dumps (see [`Config::max_concurrent_bulk_dumps`]), which
/// bounds total in-flight snapshot memory.
const DEFAULT_MAX_CONCURRENT_BULK_DUMPS: usize = 4;

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

/// Per-member discovery-absence tracking for [`ReplicatedMap::discover_periodically`].
///
/// [`Absent`](Self::Absent) owns the miss counter and the instant the absence began as one unit,
/// so the two cannot desync.
#[derive(Clone, Copy, Debug, Default)]
enum MemberPresence {
    #[default]
    Present,
    Absent {
        since: Instant,
        misses: u32,
    },
}

impl MemberPresence {
    /// Record that this member was present in the current discovery round.
    fn mark_seen(&mut self) {
        *self = MemberPresence::Present;
    }

    /// Record that this member was missing from the current discovery round, starting the absence
    /// clock on the first miss and incrementing the counter on every subsequent one.
    fn mark_missed(&mut self) {
        *self = match *self {
            MemberPresence::Present => MemberPresence::Absent {
                since: Instant::now(),
                misses: 1,
            },
            MemberPresence::Absent { since, misses } => MemberPresence::Absent {
                since,
                misses: misses + 1,
            },
        };
    }

    /// Whether this absence warrants decommissioning: at `miss_threshold` misses, immediately
    /// without a pending unacknowledged tombstone, otherwise only past `floor` — which is what
    /// keeps a flaky resolver from releasing the GC gate early.
    fn eligible_for_decommission(
        &self,
        miss_threshold: u32,
        floor: Duration,
        pending_tombstone_acks: bool,
    ) -> bool {
        let MemberPresence::Absent { since, misses } = *self else {
            return false;
        };
        if misses < miss_threshold {
            return false;
        }
        !pending_tombstone_acks || since.elapsed() >= floor
    }
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
    pub fn new_with_transport(config: Config, transport: Arc<dyn Transport>) -> Self {
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

    /// This node's HLC identity: the `node_id` on every [`Timestamp`] it mints.
    ///
    /// Random per construction unless pinned with [`Config::with_node_id`].
    pub fn node_id(&self) -> NodeId {
        self.engine.node_id()
    }

    /// Plug in a durable persistence backend, **loading any previously saved state first**.
    ///
    /// Call between [`new`](ReplicatedMap::new) and [`run`](ReplicatedMap::run), so entries,
    /// tombstones and the causal-stability membership are recovered before the node rejoins gossip.
    /// Loaded entries replay through the pre-insert hook, preserving each tombstone's deletion
    /// timestamp and rebuilding the expiry wheel.
    ///
    /// # Panics
    ///
    /// If the backend fails to load: a damaged durable state must be an explicit decision, never a
    /// silent fresh start. A *transient* failure (anything other than
    /// [`InvalidData`](io::ErrorKind::InvalidData) — a not-yet-mounted volume, a momentary
    /// permission or I/O hiccup) is retried up to `LOAD_RETRY_ATTEMPTS` (5) times with exponential
    /// backoff before this panics, so a slow-starting environment does not crash-loop on every
    /// restart attempt; a decode/format error ([`InvalidData`](io::ErrorKind::InvalidData)) is
    /// never transient and panics immediately, unretried.
    pub fn with_persistence(mut self, backend: Arc<dyn Persistence<K, V>>) -> Self {
        // A random node id changes every restart, so the LWW tie-break is stable only within one
        // process lifetime — durable state wants an explicit `Config::with_node_id`.
        if self.engine.node_id_is_random() {
            warn!(
                "persistence is enabled but no stable node_id was configured \
                 (Config::with_node_id was not called). The node id is randomly generated on \
                 every start, so this node's LWW conflict-resolution identity changes across \
                 restarts. Conflicts between a pre-restart write and a post-restart write from \
                 the same node are resolved non-deterministically. Set a stable, unique \
                 Config::with_node_id to preserve consistent LWW ordering across restarts."
            );
        }
        let loaded = {
            let mut attempt = 0u32;
            loop {
                match backend.load() {
                    Ok(state) => break state,
                    Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                        panic!("persisted state is corrupt or from an incompatible format, refusing to silently start fresh: {err}");
                    }
                    Err(err) if attempt + 1 < LOAD_RETRY_ATTEMPTS => {
                        attempt += 1;
                        let delay = backoff_delay(attempt);
                        warn!(
                            "transient failure loading persisted state (attempt {attempt}/{LOAD_RETRY_ATTEMPTS}): \
                             {err}; retrying in {delay:?}"
                        );
                        std::thread::sleep(delay);
                    }
                    Err(err) => {
                        panic!(
                            "failed to load persisted state after {LOAD_RETRY_ATTEMPTS} attempts: {err}"
                        );
                    }
                }
            }
        };
        if let Some(state) = loaded {
            *self.engine.members.write() = state.members;
            *self.engine.tombstone_acks.write() = state.tombstone_acks;
            // Advance past every persisted stamp, or a fresh write can lose LWW to this node's
            // own older value after a backward clock step. Trusted path: these stamps are
            // self-authored, and the clamp would refuse to chase them in exactly that scenario.
            for (_, entry) in &state.entries {
                self.engine.clock_observe_trusted(entry.stamp);
            }
            // Replay through the wrapped hook: the public insert helpers would re-stamp.
            self.engine.just_insert_bulk(&state.entries);
        }
        self.persistence = backend;
        self
    }

    /// Capture the full store state and hand it to the persistence backend.
    ///
    /// Clones the map in [`SNAPSHOT_CHUNK_SIZE`]-entry chunks, releasing the read lock between
    /// chunks, rather than holding it for one continuous `O(map size)` clone — see
    /// [`SNAPSHOT_CHUNK_SIZE`]'s doc for why a non-instantaneous snapshot is an acceptable
    /// trade-off here.
    fn snapshot(&self) {
        let mut entries: DatedEntries<K, V> = Vec::new();
        let mut cursor: Option<K> = None;
        loop {
            let guard = self.engine.map.read();
            let chunk: Vec<(K, Entry<Timestamp, V>)> = match &cursor {
                None => guard
                    .range(..)
                    .take(SNAPSHOT_CHUNK_SIZE)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                Some(last) => guard
                    .range((Bound::Excluded(last.clone()), Bound::Unbounded))
                    .take(SNAPSHOT_CHUNK_SIZE)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };
            drop(guard);
            let Some((last_key, _)) = chunk.last() else {
                break;
            };
            cursor = Some(last_key.clone());
            entries.extend(chunk);
        }
        let state = PersistedState::new(
            entries,
            self.engine.members.read().clone(),
            self.engine.tombstone_acks.read().clone(),
        );
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

    /// Register or refresh a known peer at runtime — the `&self` counterpart of
    /// [`with_seed`](Self::with_seed), and what a discovery source feeds in.
    ///
    /// Re-arms the peer-expiration window and makes the address a gossip target. Never grants
    /// causal-stability membership (`ARCHITECTURE.md` §5 invariant 6).
    pub fn seed_peer(&self, peer: IpAddr) {
        self.engine.seed_peer(peer);
    }

    /// Attach an **authoritative** peer-discovery source that maintains the known-peer set, on top
    /// of the default speculative [`RandomProbe`](crate::RandomProbe).
    ///
    /// While [`run`](Self::run)ning, a background task discovers every
    /// [`discovery_interval`](Self::with_discovery_interval), seeds each address, and
    /// decommissions a member absent for
    /// [`discovery_miss_threshold`](Self::with_discovery_miss_threshold) rounds, releasing the GC
    /// gate it held.
    ///
    /// The source must be [`Authoritative`](crate::DiscoveryKind::Authoritative): absence here
    /// drives decommissioning.
    ///
    /// # Panics
    ///
    /// Panics — in release builds too, not only under `debug_assertions` — if `discovery.kind()`
    /// is [`Speculative`](crate::DiscoveryKind::Speculative). A speculative source's absences must
    /// never decommission a live member: that would release the causal-stability GC gate
    /// (`ARCHITECTURE.md` §5 invariant 6) on a member that never actually left.
    pub fn with_discovery(mut self, discovery: Arc<dyn Discovery>) -> Self {
        assert!(
            matches!(discovery.kind(), DiscoveryKind::Authoritative),
            "with_discovery expects an authoritative source; a speculative prober would be seeded \
             as permanent known peers and its absences would wrongly decommission members"
        );
        self.discovery = Some(discovery);
        self
    }

    /// Discover peers by resolving a DNS name — [`with_discovery`](Self::with_discovery) with a
    /// [`DnsDiscovery`].
    ///
    /// Point `name` at a **headless** `Service` (`clusterIP: None`): one address record per ready
    /// pod, no API client and no RBAC.
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

    /// Set the wall-time floor a member **with pending unacknowledged tombstones** must be
    /// continuously absent for before decommissioning (default 10 minutes).
    ///
    /// The fast path — no pending acks — is unaffected. The floor is what keeps a spoofed or
    /// flaky resolver from releasing the GC gate on a tombstone a healthy member never acked, and
    /// so from letting that member resurrect the value. Raising it bounds the attacker further and
    /// holds tombstones longer during a genuine outage.
    pub fn with_discovery_decommission_floor(mut self, floor: Duration) -> Self {
        self.discovery_decommission_floor = floor;
        self
    }

    /// Set a specific expiry timeout to handle tombstones.
    /// The default value is 60 seconds.
    pub fn with_tombstone_timeout(mut self, tombstone_timeout: Duration) -> Self {
        self.tombstones = self.tombstones.with_timeout(tombstone_timeout);
        self
    }

    /// Set the pre-insert hook, invoked before each key/value pair reaches the map. This is a
    /// setter: a second call replaces the first, it does not add to it.
    ///
    /// Also fires once per entry on process restart when persistence is enabled (see
    /// [`with_persistence`](Self::with_persistence)), replaying the full persisted dataset
    /// through the hook — a hook that assumes it only sees genuinely new state must account for
    /// this.
    ///
    /// Hooks run outside the map's write lock, so a hook may call back into an insert method.
    pub fn set_pre_insert<F: Send + Sync + Fn(&K, &Entry<Timestamp, V>) + 'static>(
        &self,
        pre_insert: F,
    ) {
        let tombstones = self.tombstones.clone();
        let wrapped_pre_insert = move |k: &K, v: &Entry<Timestamp, V>| {
            pre_insert(k, v);
            if v.value().is_some() {
                tombstones.remove(k);
            } else {
                // `v.stamp` is peer-controlled on a socket unauthenticated by default, so the
                // instant handed to the wheel is bounded — the stamp itself is LWW data and is
                // never rewritten. Beyond the budget the instant becomes replica-dependent, which
                // costs nothing: expiry timing is already local and GC is gated on causal
                // stability besides.
                let bounded = BoundedInstant::from_stored_stamp(
                    v.stamp.physical(),
                    TOMBSTONE_STAMP_DRIFT_BUDGET,
                );
                match bounded.bound() {
                    StampBound::Verbatim => {}
                    StampBound::Capped => {
                        warn!(
                            key = ?k,
                            stamp_physical_ms = v.stamp.physical().millis(),
                            stamp_node_id = v.stamp.node_id().get(),
                            budget_ms = TOMBSTONE_STAMP_DRIFT_BUDGET.millis(),
                            bounded_instant = %bounded.instant(),
                            "tombstone stamp leads local physical time by more than the drift \
                             budget; bounding its expiry instant to the cap (the stored stamp is \
                             unchanged). A peer is planting far-future stamps."
                        );
                        crate::observability::record_tombstone_stamp_bounded("capped");
                    }
                    StampBound::Unrepresentable => {
                        warn!(
                            key = ?k,
                            stamp_physical_ms = v.stamp.physical().millis(),
                            stamp_node_id = v.stamp.node_id().get(),
                            budget_ms = TOMBSTONE_STAMP_DRIFT_BUDGET.millis(),
                            "tombstone expiry cap is not a representable wall-clock instant; \
                             ageing the tombstone from now instead (the stored stamp is unchanged)"
                        );
                        crate::observability::record_tombstone_stamp_bounded("unrepresentable");
                    }
                }
                tombstones.insert(k.clone(), bounded.instant());
            }
        };
        *self.engine.pre_insert.write() = Box::new(wrapped_pre_insert);
    }

    /// Fingerprint of the live entries (value **and** timestamp) over `range`: `O(range size)`,
    /// used as the anti-entropy comparison value — equal fingerprints on both peers mean equal
    /// content over the range. See [`value_fingerprint`](Self::value_fingerprint) for the
    /// timestamp-less counterpart.
    pub fn fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.fingerprint(range)
    }

    /// Fingerprint of the **value-only projection** over a range: the timestamp-less counterpart
    /// of [`fingerprint`](Self::fingerprint), which a converged
    /// [`ReadReplicaMap`](crate::read_replica_map::ReadReplicaMap) reproduces.
    pub fn value_fingerprint<R: RangeBounds<K>>(&self, range: R) -> Fingerprint {
        self.engine.value_fingerprint(range)
    }

    /// # Deadlock
    ///
    /// The returned guard holds the map **read** lock for as long as it is alive. Calling any
    /// write method (`insert`, `remove`, `get_mut`, `update`, …) — which takes the **write** lock
    /// — while the guard from an earlier `get` on the same thread is still in scope self-deadlocks
    /// (`parking_lot`'s `RwLock` is not reentrant, and blocks with no timeout rather than
    /// panicking):
    ///
    /// ```ignore
    /// if let Some(v) = map.get(&k) {
    ///     map.insert(k, new_value); // deadlocks: `v` is still borrowing the read lock
    /// }
    /// ```
    ///
    /// Prefer [`get_cloned`](Self::get_cloned), which drops the lock before returning, as the
    /// default read when the value will be compared against or fed into a subsequent write.
    pub fn get(&self, k: &K) -> Option<ValueRef<'_, V>> {
        let guard = self.engine.map.read();
        RwLockReadGuard::try_map(guard, |map| map.get(k).and_then(|entry| entry.value()))
            .ok()
            .map(ValueRef)
    }

    /// Clone of the live value for `k`, or `None`. Unlike [`get`](Self::get), the read lock is
    /// released before this returns, so the result can be safely followed by a write on the same
    /// thread — this is the documented default read for that pattern. Still racy against a
    /// concurrent write between the read and the write; use [`update`](Self::update) instead when
    /// the write must be atomic with the read.
    pub fn get_cloned(&self, k: &K) -> Option<V> {
        self.get(k).map(|v| v.clone())
    }

    /// The number of **live** entries. `O(n)`, and smaller than the raw map size: tombstones
    /// linger until causal-stability-gated GC reclaims them.
    pub fn len(&self) -> usize {
        self.engine
            .map
            .read()
            .iter()
            .filter(|(_, entry)| !entry.is_tombstone())
            .count()
    }

    /// Whether the store holds no live entry. `O(n)` worst case, but returns as soon as it finds a
    /// live value. A store that holds only tombstones is empty.
    pub fn is_empty(&self) -> bool {
        !self
            .engine
            .map
            .read()
            .iter()
            .any(|(_, entry)| !entry.is_tombstone())
    }

    /// Whether `k` maps to a live value (a tombstoned key reads as absent).
    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    /// The smallest live key and its value, or `None` if the store holds no live entry. `O(log n)`,
    /// worse if the smallest raw key is tombstoned (`O(n)` if every entry is).
    pub fn first_key_value(&self) -> Option<(K, V)> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .find(|(_, entry)| !entry.is_tombstone())
            .map(|(k, entry)| (k.clone(), entry.value().expect("checked above").clone()))
    }

    /// The largest live key and its value, or `None` if the store holds no live entry. Same
    /// complexity as [`first_key_value`](Self::first_key_value).
    pub fn last_key_value(&self) -> Option<(K, V)> {
        let guard = self.engine.map.read();
        let mut index = guard.len();
        while index > 0 {
            index -= 1;
            let key = guard.select(index).clone();
            if let Some(value) = guard.get(&key).and_then(|entry| entry.value()) {
                return Some((key, value.clone()));
            }
        }
        None
    }

    /// Call `f` for every live entry, in key order, under the map read lock. Do not block or call
    /// back into the store from `f`.
    ///
    /// # Deadlock
    ///
    /// `f` runs while the map read lock is held. Calling a write method (`insert`, `get_mut`,
    /// …) from `f` self-deadlocks — see [`get`](Self::get)'s `# Deadlock` section.
    pub fn for_each<F: FnMut(&K, &V)>(&self, mut f: F) {
        let guard = self.engine.map.read();
        for (k, entry) in guard.iter() {
            if let Some(value) = entry.value() {
                f(k, value);
            }
        }
    }

    /// Call `f` for every live entry whose key falls in `range`, in key order. Mirrors the
    /// [`fingerprint`](Self::fingerprint) range signature; same locking discipline as
    /// [`for_each`](Self::for_each).
    ///
    /// # Deadlock
    ///
    /// Same hazard as [`for_each`](Self::for_each) — see [`get`](Self::get)'s `# Deadlock`
    /// section.
    pub fn for_each_in_range<R: RangeBounds<K>, F: FnMut(&K, &V)>(&self, range: R, mut f: F) {
        let guard = self.engine.map.read();
        for (k, entry) in guard.range(range) {
            if let Some(value) = entry.value() {
                f(k, value);
            }
        }
    }

    /// Snapshot all live entries into an owned `Vec`, in key order. Clones under the read lock;
    /// prefer [`for_each`](Self::for_each) to avoid the copy for large scans.
    pub fn to_vec(&self) -> Vec<(K, V)> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .filter_map(|(k, entry)| entry.value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// Snapshot the live entries whose keys fall in `range` into an owned `Vec`, in key order.
    pub fn range_to_vec<R: RangeBounds<K>>(&self, range: R) -> Vec<(K, V)> {
        let guard = self.engine.map.read();
        guard
            .range(range)
            .filter_map(|(k, entry)| entry.value().map(|value| (k.clone(), value.clone())))
            .collect()
    }

    /// The keys of all live entries, in key order. Thin owned convenience over [`to_vec`](Self::to_vec).
    pub fn keys(&self) -> Vec<K> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .filter_map(|(k, entry)| entry.value().map(|_| k.clone()))
            .collect()
    }

    /// The values of all live entries, in key order.
    pub fn values(&self) -> Vec<V> {
        let guard = self.engine.map.read();
        guard
            .iter()
            .filter_map(|(_, entry)| entry.value().cloned())
            .collect()
    }

    /// Insert one pair — hook outside the lock, then insert under it — returning the overwritten
    /// value.
    ///
    /// Local-only and off the published API: [`load_bulk`](Self::load_bulk) for no-broadcast
    /// seeding, [`insert`](Self::insert) for a propagating write.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_insert(&self, key: K, value: V) -> Option<V> {
        let ret = self
            .engine
            .just_insert(key, Entry::present(self.engine.clock_now(), value));
        ret.and_then(|t| t.state.into())
    }

    /// Fully-qualified insert: `just_insert` plus an async broadcast.
    ///
    /// # Value-size ceiling
    ///
    /// A single encoded `(key, entry)` must fit `65507 - authentication overhead` bytes: the send
    /// path packs messages into datagrams but never fragments one. Above that the key **never
    /// converges on any peer**, visible only as a `warn!` on the send path. Stay well clear of the
    /// ceiling, and of the MTU.
    ///
    /// # Panics
    ///
    /// The broadcast is dispatched on a detached `tokio::spawn`ed task, which panics with "there
    /// is no reactor running" unless called from inside a Tokio runtime (`#[tokio::main]`,
    /// `#[tokio::test]`, or an explicit `Runtime::block_on`/`Handle::enter`). This holds for every
    /// write method on this type.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let ret = self
            .engine
            .insert(key, Entry::present(self.engine.clock_now(), value));
        ret.and_then(|t| t.state.into())
    }

    /// Bulk-insert with hooks — every hook outside any lock, then one write lock for all entries.
    ///
    /// Local-only and off the published API; [`load_bulk`](Self::load_bulk) is the public
    /// no-broadcast seeding path.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_insert_bulk(&self, key_values: &[(K, V)]) {
        self.load_bulk(key_values);
    }

    /// Bulk-insert + async broadcast.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn insert_bulk(&self, key_values: &[(K, V)]) {
        self.engine.insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Entry::present(self.engine.clock_now(), v.clone()),
                    )
                })
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-insert **locally, without broadcasting** — the one deliberate no-broadcast write on
    /// the public API, for seeding a large dataset without a broadcast storm.
    ///
    /// Entries are stamped and hooked as usual, and propagate on the next anti-entropy round.
    ///
    /// Deliberately **not** subject to [`insert`](Self::insert)'s Tokio-runtime panic: this is the
    /// one write path that never broadcasts.
    pub fn load_bulk(&self, key_values: &[(K, V)]) {
        self.engine.just_insert_bulk(
            &key_values
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Entry::present(self.engine.clock_now(), v.clone()),
                    )
                })
                .collect::<Vec<_>>(),
        );
    }

    /// Local-only single removal; off the published API (test/`reconcile_internal_testing` only). Use
    /// [`remove`](Self::remove) for a propagating deletion.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_remove(&self, key: &K) -> Option<V> {
        let ret = self
            .engine
            .just_insert(key.clone(), Entry::tombstone(self.engine.clock_now()));
        ret.and_then(|t| t.state.into())
    }

    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn remove(&self, key: &K) -> Option<V> {
        let ret = self
            .engine
            .insert(key.clone(), Entry::tombstone(self.engine.clock_now()));
        ret.and_then(|t| t.state.into())
    }

    /// Local-only bulk removal; off the published API (test/`reconcile_internal_testing` only). Use
    /// [`remove_bulk`](Self::remove_bulk) for propagating deletions.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn just_remove_bulk(&self, keys: &[K]) {
        self.engine.just_insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), Entry::tombstone(self.engine.clock_now())))
                .collect::<Vec<_>>(),
        );
    }

    /// Bulk-remove: a fresh HLC stamp per key, broadcast as tombstones.
    ///
    /// Callers cannot supply the timestamp: a chosen `DateTime` can collide with another
    /// replica's and make the tie-break non-commutative.
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn remove_bulk(&self, keys: &[K]) {
        self.engine.insert_bulk(
            &keys
                .iter()
                .map(|k| (k.clone(), Entry::tombstone(self.engine.clock_now())))
                .collect::<Vec<_>>(),
        );
    }

    /// Collect the live keys currently satisfying `select`, holding the map read lock only for
    /// the scan (dropped before any deletion). Shared by [`clear`](Self::clear),
    /// [`retain`](Self::retain), and [`delete_range`](Self::delete_range).
    fn live_keys_where<P: FnMut(&K, &V) -> bool>(&self, mut select: P) -> Vec<K> {
        let guard = self.engine.map.read();
        guard
            .range(..)
            .filter_map(|(k, entry)| {
                entry
                    .value()
                    .and_then(|value| select(k, value).then(|| k.clone()))
            })
            .collect()
    }

    /// Delete every live entry, as broadcast tombstones (so the deletion reconciles to peers
    /// rather than mutating the map only locally). Tombstoned keys are reclaimed later by
    /// causal-stability GC. A no-op if the store holds no live entry.
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// the store is non-empty; a no-op call never spawns).
    pub fn clear(&self) {
        let keys = self.live_keys_where(|_, _| true);
        if !keys.is_empty() {
            self.remove_bulk(&keys);
        }
    }

    /// Delete every live entry for which `keep` returns `false`, as broadcast tombstones. Keys
    /// where `keep` returns `true` are retained. The predicate runs under the read lock; keep it
    /// cheap and side-effect free.
    ///
    /// # Deadlock
    ///
    /// `keep` runs while the map read lock is held. Calling a write method from `keep`
    /// self-deadlocks — see [`get`](Self::get)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// at least one entry is removed; a no-op call never spawns).
    pub fn retain<P: FnMut(&K, &V) -> bool>(&self, mut keep: P) {
        let keys = self.live_keys_where(|k, v| !keep(k, v));
        if !keys.is_empty() {
            self.remove_bulk(&keys);
        }
    }

    /// Delete every live entry whose key falls in `range`, as broadcast tombstones. Mirrors the
    /// [`fingerprint`](Self::fingerprint) range signature.
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// the range is non-empty; a no-op call never spawns).
    pub fn delete_range<R: RangeBounds<K>>(&self, range: R) {
        let keys: Vec<K> = {
            let guard = self.engine.map.read();
            guard
                .range(range)
                .filter_map(|(k, entry)| entry.value().map(|_| k.clone()))
                .collect()
        };
        if !keys.is_empty() {
            self.remove_bulk(&keys);
        }
    }

    /// Run one round of anti-entropy against the configured peers: compare fingerprints, exchange
    /// any differing ranges, and merge what comes back. Normally driven on
    /// [`reconcile_interval`](Config::reconcile_interval) by the background task spawned at
    /// construction; exposed for callers that want to force an out-of-band round (e.g. in tests).
    pub async fn start_reconciliation(&self) {
        let mut buf = Vec::new();
        self.engine.start_reconciliation(&mut buf).await;
    }

    /// Permanently forget a peer, so tombstones stop waiting for its acknowledgment.
    ///
    /// The escape hatch from the causal-stability gate: a replica that is never coming back must
    /// be decommissioned, or its tombstones are retained forever.
    pub fn forget_peer(&self, peer: IpAddr) {
        self.engine.decommission_peer(peer);
    }

    /// The current membership set: peers that have sent a dated, authenticated datagram, and that
    /// gate tombstone GC.
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn members_snapshot(&self) -> std::collections::HashSet<std::net::IpAddr> {
        self.engine.members_snapshot()
    }

    /// Number of entries in the peers gossip-routing map.
    ///
    /// Exposed for integration-test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn peers_map_len(&self) -> usize {
        self.engine.peers_map_len()
    }

    /// Number of entries in the per-peer replay filter.
    ///
    /// Exposed for integration-test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn replay_filter_len(&self) -> usize {
        self.engine.replay_filter_len()
    }

    /// Number of keys currently tracked in the tombstone-acknowledgment map.
    ///
    /// Exposed for integration-test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn tombstone_acks_len(&self) -> usize {
        self.engine.tombstone_acks_len()
    }

    /// Number of bulk dump tasks currently in flight across all peers.
    ///
    /// Exposed for integration-test assertions under the `reconcile_internal_testing` cfg (#330).
    #[cfg(any(test, reconcile_internal_testing))]
    pub fn bulk_dumps_in_flight_count(&self) -> usize {
        self.engine.bulk_dumps_in_flight_count()
    }

    /// (runtime) Replace the declared geographical networks, re-deriving the local one. See
    /// [`Config::nets`].
    ///
    /// # Errors
    ///
    /// If `nets` exceeds [`MAX_NETS`] — the same cap `Config::with_net`/`try_with_net` enforce at
    /// construction time (#293).
    ///
    /// Safe live otherwise: topology is per-node and carries no wire tag, and repair of known
    /// peers is not gated on net membership, so the worst case is suboptimal WAN traffic, never
    /// divergence.
    pub fn set_nets(&self, nets: &[IpNet]) -> Result<(), ConfigError> {
        self.engine.set_nets(nets)
    }

    /// (runtime) Declare an additional network (e.g. opening a new region). Idempotent; returns
    /// `false` (and logs) if the [`MAX_NETS`] cap is already reached. The local network is re-derived.
    #[must_use]
    pub fn add_net(&self, net: IpNet) -> bool {
        self.engine.add_net(net)
    }

    /// (runtime) Stop declaring a network, returning whether it was present. Known peers keep
    /// being repaired; add a replacement net before removing the old one to keep discovery
    /// connected through a migration.
    #[must_use]
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

    /// Garbage-collect tombstones, **gated on causal stability** (`ARCHITECTURE.md` §5
    /// invariant 6): older than the timeout *and* acknowledged by every replica this node has
    /// communicated with, or decommissioned via [`forget_peer`](Self::forget_peer).
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
    /// A no-op with no source configured.
    ///
    /// - A **successful** resolution seeds every returned address as a known peer.
    /// - An absent **member** accrues a miss; at
    ///   [`discovery_miss_threshold`](Self::with_discovery_miss_threshold) it is decommissioned per
    ///   [`MemberPresence::eligible_for_decommission`], releasing its GC gate.
    /// - A **failed** resolution is skipped entirely, never counted as a miss.
    ///
    /// Only `members` are decommissioned: discovery never writes membership, so a spoofable
    /// address can neither block nor release GC (`ARCHITECTURE.md` §5 invariant 6).
    async fn discover_periodically(&self) {
        let Some(discovery) = self.discovery.clone() else {
            return; // no discovery source: leave peer-finding to the engine's per-net probing
        };
        let own_addr = self.engine.listen_addr();
        // Presence state per address discovery has ever reported (so we only grace-decommission
        // members we actually discovered, never peers learned by other means).
        let mut presence: HashMap<IpAddr, MemberPresence> = HashMap::new();
        loop {
            tokio::time::sleep(self.discovery_interval).await;
            let resolved = match discovery.discover().await {
                Ok(addrs) => addrs,
                Err(err) => {
                    // Transient failure: do not touch presence state, do not decommission anyone.
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
                presence.entry(*addr).or_default().mark_seen();
                self.engine.seed_peer(*addr);
            }
            // 2) Grace-account members that were discovered before but are now absent.
            for member in self.engine.members_snapshot() {
                if member == own_addr || current.contains(&member) {
                    continue;
                }
                let Some(state) = presence.get_mut(&member) else {
                    continue; // never discovered by this source: not ours to decommission
                };
                state.mark_missed();
                let pending = self.engine.has_pending_tombstone_acks(member);
                if state.eligible_for_decommission(
                    self.discovery_miss_threshold,
                    self.discovery_decommission_floor,
                    pending,
                ) {
                    info!(
                        "decommissioning vanished peer {member} \
                         (pending_tombstone_acks={pending})"
                    );
                    self.engine.decommission_peer(member);
                    presence.remove(&member);
                }
            }
        }
    }

    /// Drive the gossip and reconciliation loops forever. Never returns and cannot fail: send
    /// errors are logged and counted.
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

impl<K: Key + Hash, V: Value> ReplicatedMap<K, V> {
    /// Mutate the value for `k` in place, then propagate like [`insert`](ReplicatedMap::insert).
    ///
    /// The callback sees `Some(&mut V)` for a live key, `None` for an absent or tombstoned one; a
    /// mutated entry is re-stamped and broadcast. Holds the write lock for the whole
    /// read-modify-write, so it is atomic against the reconciliation loop.
    ///
    /// # Deadlock
    ///
    /// `callback` runs while the map **write** lock is held. Calling any read or write method
    /// (`get`, `insert`, `for_each`, another `get_mut`, …) from `callback` self-deadlocks — see
    /// [`get`](Self::get)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// the callback mutates a live entry).
    pub fn get_mut<F: FnOnce(Option<&mut V>)>(&self, k: &K, callback: F) {
        // Mint the timestamp before taking the map lock, matching the lock order of `insert`
        // (clock, then map → projection).
        let now = self.engine.clock_now();
        let mut updated: Option<Entry<Timestamp, V>> = None;
        let mut guard = self.engine.map.write();
        guard.with_mut(k, |maybe_entry| {
            if let Some(entry) = maybe_entry {
                callback(entry.value_mut());
                entry.stamp = now;
                updated = Some(entry.clone());
            } else {
                callback(None);
            }
        });
        // The mutation bypassed `insert`: refresh the projection (lock order map → projection).
        if let Some(entry) = guard.get(k) {
            let projected = entry.project();
            self.engine.projection.write().insert(k.clone(), projected);
        }
        drop(guard);
        if let Some(value) = updated {
            self.engine.broadcast_update(k.clone(), value);
        }
    }

    /// Mutate `k` in place **only when live**, re-stamping and broadcasting; returns whether it
    /// was. Atomic against the reconciliation loop.
    ///
    /// The shared core of [`update`](Self::update) and [`upsert`](Self::upsert).
    fn mutate_live<F: FnOnce(&mut V)>(&self, k: &K, callback: F) -> bool {
        // Mint the timestamp before taking the map lock, matching the lock order of `insert`.
        let now = self.engine.clock_now();
        let mut updated: Option<Entry<Timestamp, V>> = None;
        let mut guard = self.engine.map.write();
        guard.with_mut(k, |maybe_entry| {
            if let Some(entry) = maybe_entry {
                if let Some(value) = entry.value_mut() {
                    callback(value);
                    entry.stamp = now;
                    updated = Some(entry.clone());
                }
            }
        });
        if updated.is_some() {
            if let Some(entry) = guard.get(k) {
                let projected = entry.project();
                self.engine.projection.write().insert(k.clone(), projected);
            }
        }
        drop(guard);
        if let Some(value) = updated {
            self.engine.broadcast_update(k.clone(), value);
            true
        } else {
            false
        }
    }

    /// Atomically mutate the live value for `k`, then re-stamp and broadcast; returns whether the
    /// key was live. The race-free replacement for a `get`-then-`insert`.
    ///
    /// # Deadlock
    ///
    /// `f` runs while the map write lock is held — same hazard as
    /// [`get_mut`](Self::get_mut)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// `k` is live).
    #[must_use]
    pub fn update<F: FnOnce(&mut V)>(&self, k: &K, f: F) -> bool {
        self.mutate_live(k, f)
    }

    /// Update the live value for `k` with `f`, or insert `default` if it is absent or tombstoned.
    ///
    /// The update branch is atomic; the insert branch behaves like [`insert`](Self::insert).
    ///
    /// # Deadlock
    ///
    /// `f` runs while the map write lock is held on the update branch — same hazard as
    /// [`get_mut`](Self::get_mut)'s `# Deadlock` section.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime.
    pub fn upsert<F: FnOnce(&mut V)>(&self, k: K, default: V, f: F) {
        if !self.mutate_live(&k, f) {
            self.insert(k, default);
        }
    }

    /// Return the live value for `k`, inserting (and broadcasting) `f()` first if it is
    /// absent/tombstoned. Under last-write-wins, two nodes racing to insert converge by timestamp
    /// order; this node returns the value it observed/created.
    ///
    /// # Panics
    ///
    /// See [`insert`](Self::insert) — the broadcast requires an ambient Tokio runtime (only when
    /// `k` is absent/tombstoned).
    pub fn get_or_insert_with<F: FnOnce() -> V>(&self, k: &K, f: F) -> V {
        if let Some(value) = self.get(k) {
            return value.clone();
        }
        let value = f();
        self.insert(k.clone(), value.clone());
        value
    }
}

/// Maximum number of geographical networks (CIDRs) a [`Config`] can declare, or a running node
/// can hold via [`ReplicatedMap::set_nets`]/[`add_net`](ReplicatedMap::add_net) — one behavior for
/// the cap everywhere it is enforced (#293). Eight networks is generous for real geographical
/// deployments.
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

/// Construction parameters for a [`ReplicatedMap`]. Build with [`Config::new`] (or
/// [`Config::default`]) and the `with_*` builders (e.g. [`with_net`](Config::with_net)); every
/// field is `pub` for direct construction and reading within this crate, but `#[non_exhaustive]`
/// means an external crate must go through a constructor and builders — one construction path,
/// not two with different guarantees (#293).
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
    /// UDP port to bind, **and** the port this node assumes every peer listens on — gossip does
    /// no per-peer port discovery, so every node in a cluster must share one port.
    ///
    /// `0` binds to an OS-assigned ephemeral port, which is fine for receiving, but every
    /// outbound datagram to a peer is still addressed to port `0` literally (the OS-assigned port
    /// is never read back) — a node configured this way can never converge with anything, only
    /// receive nothing back. [`ReplicatedMap::new`]/[`ReadReplicaMap::new`](crate::ReadReplicaMap::new)
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
        }
    }
}
impl Config {
    /// The documented default constructor (#293): `port` is the one setting every node in a
    /// cluster must agree on (see [`port`](Self::port)'s docs for why `0` can never converge).
    /// Equivalent to `Config::default().with_port(port)`.
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
    /// If more than [`MAX_NETS`] networks are declared. See [`try_with_net`](Self::try_with_net)
    /// for a non-panicking alternative — the same [`MAX_NETS`] cap
    /// [`ReplicatedMap::set_nets`]/[`add_net`](ReplicatedMap::add_net) enforce at runtime (#293).
    #[must_use]
    pub fn with_net(self, net: IpNet) -> Self {
        match self.try_with_net(net) {
            Ok(config) => config,
            Err(err) => panic!("{err}"),
        }
    }

    /// As [`with_net`](Self::with_net), but returns a [`ConfigError`] instead of panicking past
    /// [`MAX_NETS`].
    ///
    /// # Errors
    ///
    /// If more than [`MAX_NETS`] networks are declared.
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

#[cfg(test)]
mod replicated_map_tests;
