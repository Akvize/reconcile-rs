use crate::clock::NodeId;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::persistence::{PersistedState, Persistence};
use crate::replica::version_hash;
use crate::{
    replicated_map::{Config, ConfigError, MemberPresence, MAX_NETS},
    FileSnapshot, ReplicatedMap,
};

/// A config bound to a fresh port on loopback (#293: port `0` is refused, so
/// [`next_ephemeral_test_port`](crate::replica::next_ephemeral_test_port) stands in for it),
/// so persistence tests can construct stores without colliding on a fixed port.
fn ephemeral_config() -> Config {
    Config {
        port: crate::replica::next_ephemeral_test_port(),
        listen_addr: "127.0.0.1".parse().unwrap(),
        nets: [None; MAX_NETS],
        remote_interval: 6,
        remote_fanout: 2,
        cluster_key: None,
        insecure_no_key: true,
        node_id: None,
        encrypt: false,
        reconcile_interval: Duration::from_secs(1),
        bulk_send_rate: Some(super::DEFAULT_BULK_SEND_RATE),
        recv_buffer_size: Some(super::DEFAULT_SOCKET_BUFFER_SIZE),
        send_buffer_size: Some(super::DEFAULT_SOCKET_BUFFER_SIZE),
        freshness_window: gossip::replay::FRESHNESS_WINDOW_DEFAULT,
        max_peers: super::DEFAULT_MAX_PEERS,
        max_concurrent_bulk_dumps: super::DEFAULT_MAX_CONCURRENT_BULK_DUMPS,
    }
}

/// #325: a `Config` with neither a cluster key nor the explicit insecure opt-in must refuse to
/// build at all, rather than silently running unauthenticated — the whole point of the guard.
#[tokio::test]
#[should_panic(expected = "Config::cluster_key is None")]
async fn missing_key_and_no_insecure_opt_in_panics_at_construction() {
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("OS should hand out an ephemeral port")
        .local_addr()
        .expect("a bound socket reports its own address")
        .port();
    let config = Config::default().with_port(port);
    let _ = ReplicatedMap::<i32, i32>::new(config).await;
}

/// #293: `Config::port == 0` ("let the OS pick") can never converge — gossip addresses every
/// outbound datagram to this port, so there is no per-peer discovery to learn what the OS chose.
/// `ReplicatedMap::new` must refuse this before ever touching a socket.
#[tokio::test]
async fn zero_port_is_rejected_before_binding() {
    let config = Config::default().with_port(0).with_insecure_no_key();
    let err = match ReplicatedMap::<i32, i32>::new(config).await {
        Ok(_) => panic!("port 0 should be rejected"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("must be nonzero"),
        "unexpected error: {err}"
    );
}

/// `ConfigError::TooManyNets`'s `Display` text is user-facing — assert its actual content, not
/// merely that formatting it doesn't panic.
#[test]
fn config_error_too_many_nets_display_names_the_limit() {
    assert_eq!(
        ConfigError::TooManyNets.to_string(),
        format!("at most {MAX_NETS} networks are supported")
    );
}

/// `Config`'s hand-written `Debug` impl exists to redact `cluster_key` — assert it actually
/// hides the key material and still reports `Some`/`None` correctly either way.
#[test]
fn config_debug_redacts_cluster_key_but_not_its_presence() {
    let key_bytes = [0xABu8; 32];
    let with_key = ephemeral_config().with_cluster_key(gossip::auth::ClusterKey::new(key_bytes));
    let debug = format!("{with_key:?}");
    assert!(
        debug.contains("<redacted>"),
        "expected the key material to be redacted: {debug}"
    );
    assert!(
        !debug.contains("ab, ab, ab") && !debug.contains("171, 171, 171"),
        "raw key bytes must not appear in Debug output: {debug}"
    );

    let without_key = ephemeral_config();
    let debug = format!("{without_key:?}");
    assert!(
        debug.contains("cluster_key: None"),
        "expected an explicit None, got: {debug}"
    );
}

/// #293: `set_nets` enforces the same [`MAX_NETS`] cap `Config::with_net`/`try_with_net` do at
/// construction time, just at runtime — both the accepting and the rejecting side need coverage.
#[tokio::test]
async fn set_nets_enforces_max_nets_at_runtime() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();

    let within_cap: Vec<_> = (0..MAX_NETS)
        .map(|i| format!("127.0.0.0/{}", 8 + (i % 24)).parse().unwrap())
        .collect();
    store
        .set_nets(&within_cap)
        .expect("exactly MAX_NETS networks should be accepted");

    let over_cap: Vec<_> = (0..=MAX_NETS)
        .map(|i| format!("127.0.0.0/{}", 8 + (i % 24)).parse().unwrap())
        .collect();
    assert_eq!(
        store.set_nets(&over_cap),
        Err(ConfigError::TooManyNets),
        "MAX_NETS + 1 networks should be rejected"
    );
}

/// D4: the node id is settable through `Config` and readable back off the store, and the
/// value reported is the one the clock actually stamps onto minted timestamps.
#[tokio::test]
async fn node_id_is_readable_and_matches_the_minted_stamp() {
    let store =
        ReplicatedMap::<i32, i32>::new(ephemeral_config().with_node_id(NodeId::new(0xABCD)))
            .await
            .unwrap();
    assert_eq!(store.node_id(), NodeId::new(0xABCD));
    store.insert(1, 1);
    let stamp = store.engine.map.read().get(&1).unwrap().stamp;
    assert_eq!(stamp.node_id(), store.node_id());
}

/// D2: two stores wired to a caller-supplied `Transport` converge over it, with no UDP socket
/// bound anywhere. This is the public seam, exercised exactly as a downstream crate would.
#[tokio::test]
async fn stores_converge_over_an_injected_transport() {
    use std::net::SocketAddr;
    use std::time::Instant;

    use crate::transport::InMemoryNetwork;

    let net = InMemoryNetwork::new();
    let port = 5100u16;
    let a_ip: IpAddr = "127.0.0.4".parse().unwrap();
    let b_ip: IpAddr = "127.0.0.5".parse().unwrap();
    let cfg = |ip: IpAddr, id: u64| {
        ephemeral_config()
            .with_listen_addr(ip)
            .with_port(port)
            .with_node_id(NodeId::new(id))
            .with_reconcile_interval(Duration::from_millis(5))
    };
    let a = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(a_ip, 1),
        Arc::new(net.bind(SocketAddr::new(a_ip, port))),
    );
    let b = ReplicatedMap::<i32, i32>::new_with_transport(
        cfg(b_ip, 2),
        Arc::new(net.bind(SocketAddr::new(b_ip, port))),
    );
    // Seed each as the other's gossip peer: the in-memory fabric has no discovery.
    a.engine.peers.write().insert(b_ip, Instant::now());
    b.engine.peers.write().insert(a_ip, Instant::now());
    a.insert(7, 42);

    let ta = tokio::spawn(a.clone().run());
    let tb = tokio::spawn(b.clone().run());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut converged = false;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if b.get(&7).as_deref() == Some(&42) {
            converged = true;
            break;
        }
    }
    ta.abort();
    tb.abort();
    assert!(
        converged,
        "B never learned A's write over the injected transport"
    );
}

#[tokio::test]
async fn tombstones_expiration() {
    // A dedicated port and /32 net keep a concurrent test's discovery from injecting here.
    let config = Config::default()
        .with_port(8090)
        .with_listen_addr("127.0.0.45".parse().unwrap())
        .with_net("127.0.0.45/32".parse().unwrap())
        .with_insecure_no_key();
    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed")
        .with_tombstone_timeout(Duration::from_millis(1));

    // No `run()`: its periodic GC would race these assertions.

    // `remove` inserts a tombstone rather than deleting the key outright.
    store.remove(&0);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(store.tombstones.expired(), vec![0]);
    assert_eq!(store.tombstones.remove(&0), Some(0));
    assert_eq!(store.tombstones.remove(&0), None);
}

/// The instant derived from a peer-controlled tombstone stamp must be bounded, and the
/// stored stamp must come through byte-identical.
mod tombstone_expiry_bound {
    use super::*;
    use crate::clock::{Hlc, LogicalCounter, PhysicalTime};
    use crate::entry::Entry;
    use crate::replicated_map::TOMBSTONE_STAMP_DRIFT_BUDGET;
    use chrono::Utc;

    /// Plant a tombstone carrying exactly `physical_ms` through the hook, and return the
    /// instant the wheel recorded for it.
    async fn plant(physical_ms: u64) -> (ReplicatedMap<i32, i32>, chrono::DateTime<Utc>) {
        let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
            .await
            .expect("bind failed");
        let stamp = crate::clock::Timestamp::new(
            Hlc::new(
                PhysicalTime::from_millis(physical_ms),
                LogicalCounter::new(7),
            ),
            NodeId::new(0xBEEF),
        );
        store
            .engine
            .just_insert_bulk(&[(1, Entry::tombstone(stamp))]);

        // The stored stamp must be untouched.
        assert_eq!(
            store.engine.map.read().get(&1).unwrap().stamp,
            stamp,
            "the stored stamp must be exactly as received — only the expiry instant is bounded"
        );

        let when = store
            .tombstones
            .instant_of(&1)
            .expect("tombstone was not tracked");
        (store, when)
    }

    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn cap_ms() -> i64 {
        now_ms() + TOMBSTONE_STAMP_DRIFT_BUDGET.millis() as i64
    }

    /// Regime 1 — an ordinary stamp is used verbatim, exactly as before the bound existed.
    /// This is what keeps honest replicas agreeing on when a tombstone ages out.
    #[tokio::test]
    async fn a_normal_stamp_is_used_verbatim() {
        let physical = now_ms() as u64 - 1_000;
        let (_store, when) = plant(physical).await;
        assert_eq!(when.timestamp_millis(), physical as i64);
    }

    /// Regime 2: far future, inside chrono's range. Must land on the cap — converting it
    /// exactly would date the tombstone past every plausible expiry.
    #[tokio::test]
    async fn a_far_future_representable_stamp_is_capped() {
        // ~10 000 years ahead: inside chrono's ceiling, so a lossless conversion is the hazard.
        let physical = now_ms() as u64 + 10_000 * 365 * 24 * 3_600_000;
        let (_store, when) = plant(physical).await;
        assert!(
            when.timestamp_millis() <= cap_ms(),
            "instant {when} escaped the cap"
        );
        assert!(
            when.timestamp_millis() >= now_ms() - 1_000,
            "a capped instant must stay in the near future, not fall into the past"
        );
    }

    /// Regime 3: above `i64::MAX`. Must land on the same cap, never a pre-1970 date.
    #[tokio::test]
    async fn a_stamp_above_i64_max_is_bounded_not_wrapped() {
        let (_store, when) = plant(u64::MAX).await;
        assert!(
            when.timestamp_millis() > 0,
            "stamp wrapped to a pre-epoch instant: {when}"
        );
        assert!(
            when.timestamp_millis() <= cap_ms(),
            "instant {when} escaped the cap"
        );
    }

    /// A hostile tombstone's expiry deadline must be a finite operator-controlled horizon
    /// (`now + budget + timeout`), not a date the peer picked. Bounding makes expiry
    /// reachable, not immediate — hence asserting on the deadline.
    #[tokio::test]
    async fn a_capped_tombstone_has_a_finite_expiry_horizon() {
        let timeout = Duration::from_secs(60);
        let physical = now_ms() as u64 + 10_000 * 365 * 24 * 3_600_000;
        let (_store, when) = plant(physical).await;

        let deadline = when.timestamp_millis() + timeout.as_millis() as i64;
        assert!(
            deadline <= cap_ms() + timeout.as_millis() as i64,
            "expiry deadline {deadline} is beyond now + budget + timeout"
        );
        // For contrast, the deadline an unbounded conversion would produce.
        let unbounded = physical as i64 + timeout.as_millis() as i64;
        assert!(
            unbounded > deadline + 100 * 365 * 24 * 3_600_000,
            "the unbounded deadline should be astronomically later than the bounded one"
        );
    }
}

/// A durable backend must let a restarted store recover both live values and tombstones, with
/// identical timestamps (hence an identical fingerprint).
#[tokio::test]
async fn persistence_roundtrip_recovers_entries_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    store.insert(1, 11); // live value
    store.insert(2, 22);
    store.remove(&2); // tombstone
    let expected = store.fingerprint(..);
    store.snapshot(); // force a durable write

    // A brand-new store recovers the previous state from the same file.
    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
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

/// A [`Persistence`] backend that fails `load` with a given `io::ErrorKind` a fixed number of
/// times before succeeding with `Ok(None)` — simulates a transient environmental failure (a
/// not-yet-mounted volume, a momentary permission error) that clears up on its own.
struct FlakyLoad {
    kind: std::io::ErrorKind,
    failures_remaining: std::sync::atomic::AtomicU32,
}

impl<K: Send + Sync + 'static, V: Send + Sync + 'static> Persistence<K, V> for FlakyLoad {
    fn load(&self) -> std::io::Result<Option<PersistedState<K, V>>> {
        use std::sync::atomic::Ordering;
        if self
            .failures_remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(std::io::Error::new(
                self.kind,
                "simulated transient failure",
            ));
        }
        Ok(None)
    }
    fn save(&self, _state: &PersistedState<K, V>) -> std::io::Result<()> {
        Ok(())
    }
}

/// Doubles from `LOAD_RETRY_BASE_DELAY` each attempt, 1-indexed: attempt 1 is the base delay
/// itself, not one doubling of it.
#[test]
fn backoff_delay_doubles_from_the_base() {
    assert_eq!(super::backoff_delay(1), super::LOAD_RETRY_BASE_DELAY);
    assert_eq!(super::backoff_delay(2), super::LOAD_RETRY_BASE_DELAY * 2);
    assert_eq!(super::backoff_delay(3), super::LOAD_RETRY_BASE_DELAY * 4);
    assert_eq!(super::backoff_delay(4), super::LOAD_RETRY_BASE_DELAY * 8);
}

/// A transient load failure (anything but `InvalidData`) must be retried, not turned
/// into an immediate crash — a slow-mounting volume at boot must not crash-loop the process.
#[tokio::test]
async fn transient_load_failure_is_retried_not_fatal() {
    let backend = Arc::new(FlakyLoad {
        kind: std::io::ErrorKind::PermissionDenied,
        failures_remaining: std::sync::atomic::AtomicU32::new(super::LOAD_RETRY_ATTEMPTS - 1),
    });
    // Must not panic: the store construction below succeeds once the backend stops failing,
    // within the retry budget.
    let _store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(backend);
}

/// A load failure that exhausts the retry budget still panics — retrying is a bounded
/// mitigation for a transient hiccup, not a way to silently start fresh forever.
#[tokio::test]
#[should_panic(expected = "failed to load persisted state after")]
async fn load_failure_beyond_retry_budget_still_panics() {
    let backend = Arc::new(FlakyLoad {
        kind: std::io::ErrorKind::PermissionDenied,
        failures_remaining: std::sync::atomic::AtomicU32::new(super::LOAD_RETRY_ATTEMPTS),
    });
    let _store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(backend);
}

/// `InvalidData` (corrupt or incompatible format) must panic **immediately**, with no
/// retry — corruption does not clear up on its own, and retrying would only delay the loud
/// failure the doc comment promises.
#[tokio::test]
#[should_panic(expected = "persisted state is corrupt or from an incompatible format")]
async fn invalid_data_panics_without_retrying() {
    let backend = Arc::new(FlakyLoad {
        kind: std::io::ErrorKind::InvalidData,
        failures_remaining: std::sync::atomic::AtomicU32::new(1),
    });
    let start = std::time::Instant::now();
    let _store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(backend);
    // Unreachable on panic, but documents intent: this must not have gone through even one
    // retry backoff.
    assert!(start.elapsed() < super::LOAD_RETRY_BASE_DELAY);
}

/// `snapshot` clones the map in `SNAPSHOT_CHUNK_SIZE`-entry chunks, releasing and
/// re-acquiring the read lock between them. Insert enough entries to force several chunk
/// boundaries and confirm every one of them still round-trips — the chunking must not drop,
/// duplicate, or reorder entries relative to the previous whole-map-under-one-lock snapshot.
#[tokio::test]
async fn snapshot_across_multiple_chunks_recovers_every_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");
    let n = super::SNAPSHOT_CHUNK_SIZE * 2 + 17; // spans three chunks, last one partial

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    for k in 0..n as i32 {
        store.just_insert(k, k * 2);
    }
    let expected = store.fingerprint(..);
    store.snapshot();

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
        .with_persistence(Arc::new(FileSnapshot::new(&path)));
    assert_eq!(
        restarted.fingerprint(..),
        expected,
        "chunked snapshot must recover every entry across chunk boundaries"
    );
    for k in 0..n as i32 {
        assert_eq!(restarted.get(&k).as_deref(), Some(&(k * 2)));
    }
}

/// The causal-stability state (membership + per-tombstone acks) must survive a
/// restart, otherwise GC gating is lost.
#[tokio::test]
async fn restart_preserves_membership_and_acks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");
    let peer: IpAddr = "127.0.0.99".parse().unwrap();

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
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

    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
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

/// A restart must not turn a held-back tombstone into a collectable one: recovered
/// membership must keep gating GC.
#[tokio::test]
async fn restart_keeps_tombstone_gc_gated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.bin");
    let peer: IpAddr = "127.0.0.98".parse().unwrap();

    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
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
    let fresh = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed");
    fresh.insert(1, 11);
    fresh.remove(&1);
    let fresh_version = fresh.engine.map.read().get(&1).map(version_hash).unwrap();
    assert!(
        fresh.engine.is_tombstone_stable(&1, fresh_version),
        "a fresh restart with no membership would (wrongly) GC the tombstone — the hazard this guards against"
    );

    // The recovered store keeps the tombstone gated, preventing resurrection.
    let restarted = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .expect("bind failed")
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

use crate::discovery::{DiscoverFuture, Discovery, DiscoveryKind};

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

    fn kind(&self) -> DiscoveryKind {
        DiscoveryKind::Authoritative
    }
}

/// A discovery source that never lies about its kind — used to prove `with_discovery` rejects
/// a speculative source unconditionally, not only under `debug_assertions`.
struct SpeculativeDiscovery;

impl Discovery for SpeculativeDiscovery {
    fn discover(&self) -> DiscoverFuture<'_> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn kind(&self) -> DiscoveryKind {
        DiscoveryKind::Speculative
    }
}

/// The guard must be `assert!`, not `debug_assert!` — a no-op in `--release` would let a
/// speculative source through, whose absences then wrongly decommission live members and
/// release the causal-stability GC gate.
#[tokio::test]
#[should_panic(expected = "with_discovery expects an authoritative source")]
async fn with_discovery_rejects_a_speculative_source() {
    let store = ReplicatedMap::<i32, i32>::new(discovery_config())
        .await
        .expect("bind failed");
    let _ = store.with_discovery(Arc::new(SpeculativeDiscovery));
}

fn discovery_config() -> Config {
    // A real, bindable loopback address (the engine binds a socket in `new`) on an ephemeral
    // port. No `with_net`, mirroring the Kubernetes setup where discovery is purely DNS-driven.
    Config::default()
        .with_port(crate::replica::next_ephemeral_test_port())
        .with_listen_addr("127.0.0.1".parse().unwrap())
        .with_insecure_no_key()
}

/// A member that vanishes from discovery for `miss_threshold` consecutive successful rounds is
/// decommissioned; the node never decommissions itself even when absent from the result.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_decommissions_vanished_member_but_not_self() {
    let own: IpAddr = "127.0.0.1".parse().unwrap();
    let member: IpAddr = "127.0.0.200".parse().unwrap();

    let fake = FakeDiscovery::new(FakeResp::Present(vec![member]));
    let store = ReplicatedMap::<i32, i32>::new(discovery_config())
        .await
        .expect("bind failed")
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
    let store = ReplicatedMap::<i32, i32>::new(discovery_config())
        .await
        .expect("bind failed")
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

#[test]
fn member_presence_starts_present_and_requires_the_miss_threshold() {
    let mut state = MemberPresence::default();
    assert!(!state.eligible_for_decommission(1, Duration::ZERO, false));
    state.mark_missed();
    assert!(state.eligible_for_decommission(1, Duration::ZERO, false));
}

#[test]
fn member_presence_reappearance_resets_the_absence_clock_and_counter() {
    let mut state = MemberPresence::default();
    state.mark_missed();
    state.mark_missed();
    state.mark_seen();
    // A single miss after reappearing must not already clear a 2-miss threshold.
    state.mark_missed();
    assert!(!state.eligible_for_decommission(2, Duration::ZERO, false));
}

#[test]
fn member_presence_pending_tombstone_acks_require_the_wall_time_floor() {
    let mut state = MemberPresence::default();
    state.mark_missed();
    state.mark_missed();
    // Below miss_threshold: never eligible, regardless of pending acks or floor.
    assert!(!state.eligible_for_decommission(3, Duration::ZERO, true));

    state.mark_missed();
    // At the threshold, pending acks, and a floor nowhere near elapsed: held back.
    assert!(!state.eligible_for_decommission(3, Duration::from_secs(3600), true));
    // Same absence with no pending acks: the fast path is unaffected by the floor.
    assert!(state.eligible_for_decommission(3, Duration::from_secs(3600), false));
    // A zero floor is cleared instantly even with pending acks.
    assert!(state.eligible_for_decommission(3, Duration::ZERO, true));
}

/// A member with an unacknowledged tombstone survives past `miss_threshold` and is
/// decommissioned only once its absence clears the wall-time floor
/// (`ARCHITECTURE.md` §5 invariant 6).
#[tokio::test(flavor = "multi_thread")]
async fn pending_tombstone_acks_hold_decommission_past_the_miss_threshold() {
    let member: IpAddr = "127.0.0.210".parse().unwrap();

    let fake = FakeDiscovery::new(FakeResp::Present(vec![member]));
    let store = ReplicatedMap::<i32, i32>::new(discovery_config())
        .await
        .expect("bind failed")
        .with_discovery(Arc::new(fake.clone()))
        .with_discovery_interval(Duration::from_millis(15))
        .with_discovery_miss_threshold(2)
        .with_discovery_decommission_floor(Duration::from_millis(300));
    store.engine.members.write().insert(member);

    // A local tombstone this member has never acknowledged.
    store.just_insert(1, 11);
    store.just_remove(&1);

    let loop_store = store.clone();
    let handle = tokio::spawn(async move { loop_store.discover_periodically().await });

    // Let the member be observed present at least once, so it is registered as discovered by
    // this source (and thus eligible for grace-decommissioning) before it vanishes.
    tokio::time::sleep(Duration::from_millis(60)).await;

    // The member vanishes. It must clear the miss threshold quickly but stay a member — the
    // floor has not elapsed yet.
    fake.set(FakeResp::Present(vec![]));
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        store.engine.members.read().contains(&member),
        "member with a pending tombstone ack was decommissioned before the wall-time floor \
         elapsed"
    );

    // Once the floor elapses, decommissioning proceeds.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(
        !store.engine.members.read().contains(&member),
        "member was never decommissioned even after the wall-time floor elapsed"
    );

    handle.abort();
}

/// Reappearing before the floor elapses resets the absence clock: a brief DNS blip followed by
/// recovery must never decommission the member, however close to the floor the first absence
/// got.
#[tokio::test(flavor = "multi_thread")]
async fn reappearance_resets_the_floor_for_a_member_with_pending_acks() {
    let member: IpAddr = "127.0.0.211".parse().unwrap();

    let fake = FakeDiscovery::new(FakeResp::Present(vec![member]));
    let store = ReplicatedMap::<i32, i32>::new(discovery_config())
        .await
        .expect("bind failed")
        .with_discovery(Arc::new(fake.clone()))
        .with_discovery_interval(Duration::from_millis(15))
        .with_discovery_miss_threshold(2)
        .with_discovery_decommission_floor(Duration::from_millis(300));
    store.engine.members.write().insert(member);
    store.just_insert(1, 11);
    store.just_remove(&1);

    let loop_store = store.clone();
    let handle = tokio::spawn(async move { loop_store.discover_periodically().await });

    // Absent for a while (well past miss_threshold, short of the floor), then returns.
    fake.set(FakeResp::Present(vec![]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    fake.set(FakeResp::Present(vec![member]));
    tokio::time::sleep(Duration::from_millis(60)).await;

    // Vanishes again; if the clock had not reset, the combined absence would already exceed
    // the floor.
    fake.set(FakeResp::Present(vec![]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        store.engine.members.read().contains(&member),
        "reappearance did not reset the wall-time floor's absence clock"
    );

    handle.abort();
}

// ----- Observe-on-load (HLC monotonicity across restarts) tests -----

/// Loading persisted state must advance the clock past the maximum persisted stamp, so the
/// first post-restart write outranks every pre-restart one.
#[tokio::test]
async fn restart_clock_advanced_past_persisted_max_stamp() {
    use crate::clock::{Hlc, LogicalCounter, ManualClock, NodeId, PhysicalTime};
    use crate::persistence::{InMemoryPersistence, PersistedState};

    // Build a ManualClock starting at (physical=0, logical=0).
    let clock = Arc::new(ManualClock::new(NodeId::new(1)));

    // Craft a PersistedState whose only entry carries a stamp well ahead of the clock.
    // physical=100 is in the "future" relative to the ManualClock's (0, 0) starting state.
    let persisted_stamp = crate::clock::Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(100), LogicalCounter::new(0)),
        NodeId::new(1),
    );
    let backend = Arc::new(InMemoryPersistence::<i32, i32>::new());
    backend
        .save(&PersistedState::from(vec![(
            42,
            crate::entry::Entry::present(persisted_stamp, 999),
        )]))
        .unwrap();

    // Create a store with the ManualClock and load the persisted state.
    let store = ReplicatedMap::<i32, i32>::new_with_clock(
        ephemeral_config().with_node_id(NodeId::new(1)),
        clock,
    )
    .await
    .expect("bind failed")
    .with_persistence(backend);

    // Insert a new value; the minted timestamp must be strictly greater than persisted_stamp.
    store.insert(99, 1);

    // Read the stored timestamp for key 99 via the internal map.
    let minted_stamp = store
        .engine
        .map
        .read()
        .get(&99)
        .map(|entry| entry.stamp)
        .expect("key 99 must be present after insert");

    assert!(
        minted_stamp > persisted_stamp,
        "post-restart write timestamp {minted_stamp:?} is not strictly greater than the \
         persisted max {persisted_stamp:?}; the clock was not advanced on load"
    );
}

/// Same, where the maximum persisted stamp is on a **tombstone**: a post-restart insert must
/// outrank it, or a peer still holding the tombstone re-applies it via LWW.
#[tokio::test]
async fn restart_insert_beats_persisted_tombstone() {
    use crate::clock::{Hlc, LogicalCounter, ManualClock, NodeId, PhysicalTime};
    use crate::persistence::{InMemoryPersistence, PersistedState};

    // ManualClock starting at (physical=0, logical=0).
    let clock = Arc::new(ManualClock::new(NodeId::new(2)));

    // A tombstone with a stamp in the "future" relative to the cold clock.
    let tombstone_stamp = crate::clock::Timestamp::new(
        Hlc::new(PhysicalTime::from_millis(200), LogicalCounter::new(0)),
        NodeId::new(2),
    );
    let backend = Arc::new(InMemoryPersistence::<i32, i32>::new());
    backend
        .save(&PersistedState::from(vec![(
            7,
            crate::entry::Entry::tombstone(tombstone_stamp), // tombstone
        )]))
        .unwrap();

    let store = ReplicatedMap::<i32, i32>::new_with_clock(
        ephemeral_config().with_node_id(NodeId::new(2)),
        clock,
    )
    .await
    .expect("bind failed")
    .with_persistence(backend);

    // The tombstone was recovered (key 7 is absent from the live view).
    assert!(
        store.get(&7).is_none(),
        "tombstone was not recovered: expected key 7 to be absent after loading"
    );

    // Insert a fresh value for key 7 and inspect the minted timestamp.
    store.insert(7, 42);
    let minted_stamp = store
        .engine
        .map
        .read()
        .get(&7)
        .map(|entry| entry.stamp)
        .expect("key 7 must be present after insert");

    // The minted stamp must be strictly greater than the tombstone's stamp. Without this,
    // a peer that still holds the tombstone would win the LWW merge during anti-entropy
    // and silently re-apply the tombstone, undoing the fresh insert.
    assert!(
        minted_stamp > tombstone_stamp,
        "post-restart insert timestamp {minted_stamp:?} is not strictly greater than the \
         persisted tombstone stamp {tombstone_stamp:?}; the clock was not advanced on load, \
         so a peer reconciling with this node could resurrect the tombstone via LWW"
    );
}

/// `new` must return `Err`, not panic, when the port is already in use.
#[tokio::test]
async fn new_returns_err_on_bind_failure() {
    // Occupy a loopback port chosen by the OS (bind to :0, then read it back) so this test
    // cannot collide with a fixed port already taken by the parallel suite or a stray process.
    let holder =
        std::net::UdpSocket::bind("127.0.0.50:0").expect("pre-condition: a port must be free");
    let busy = holder
        .local_addr()
        .expect("holder must report its bound address");

    let config = Config::default()
        .with_port(busy.port())
        .with_listen_addr(busy.ip())
        .with_insecure_no_key();
    let result = ReplicatedMap::<i32, i32>::new(config).await;
    let err = result
        .err()
        .expect("expected Err when the bind address is already in use");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AddrInUse,
        "bind failure should surface as AddrInUse, got {err:?}"
    );
}

// ── per-peer cap ──────────────────────────────────────────────────────────

/// The default cap must be 1024.
#[test]
fn peer_cap_default_is_1024() {
    assert_eq!(Config::default().max_peers, 1024);
}

/// A raw payload with one `ComparisonItem`: a dated message, so the receive path would add
/// the sender to `members` unless the cap fires first. Starts with the wire-version byte
/// every datagram carries, unauthenticated included — `Authenticator::Disabled` no
/// longer passes bytes through unversioned.
fn dated_comparison_payload() -> Vec<u8> {
    use crate::replica::Message;
    use crate::FingerprintTreeMap;
    use bincode::{DefaultOptions, Serializer};
    use serde::Serialize as _;

    let tree = FingerprintTreeMap::<i32, (crate::clock::Timestamp, Option<i32>)>::new();
    let segments = rbsr::initial_ranges(&tree);
    let mut buf = vec![gossip::auth::WIRE_VERSION];
    for seg in segments {
        Message::<
            i32,
            (crate::clock::Timestamp, Option<i32>),
            (crate::clock::Timestamp, Option<i32>),
        >::ComparisonItem(seg)
        .serialize(&mut Serializer::new(&mut buf, DefaultOptions::new()))
        .expect("serializing ComparisonItem into a Vec cannot fail");
    }
    buf
}

/// When the membership set is at capacity, datagrams from a completely unknown sender are
/// dropped silently: the sender is not added to `members`, `peers`, or the replay filter.
/// Known members (already in `members`) are unaffected at cap.
#[tokio::test(flavor = "multi_thread")]
async fn peer_cap_blocks_unknown_sender_at_capacity() {
    let port = 9801u16;
    let target_addr: std::net::IpAddr = "127.0.0.180".parse().unwrap();
    let peer1: std::net::IpAddr = "127.0.0.181".parse().unwrap();
    let peer2: std::net::IpAddr = "127.0.0.182".parse().unwrap();
    let newcomer: std::net::IpAddr = "127.0.0.183".parse().unwrap();

    let config = Config::default()
        .with_port(port)
        .with_listen_addr(target_addr)
        .with_net("127.0.0.180/30".parse().unwrap())
        .with_max_peers(2)
        .with_insecure_no_key();

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    // Seed two known members directly (simulates two peers that already completed a
    // dated handshake with this store before the test window).
    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);

    let task = tokio::spawn(store.clone().run());

    // Give the run loop time to enter its receive wait.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send a valid dated payload from the newcomer's IP several times to ensure at least one
    // reaches the receive loop; all must be dropped by the cap.
    let payload = dated_comparison_payload();
    let sender = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(newcomer, 0))
        .await
        .expect("bind sender");
    for _ in 0..5 {
        let _ = sender
            .send_to(&payload, std::net::SocketAddr::new(target_addr, port))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Give the receive loop time to process any remaining datagrams.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The newcomer must not appear in any map regardless of how many datagrams arrived.
    assert!(
        !store.engine.members.read().contains(&newcomer),
        "capped-out sender must not be added to members"
    );
    assert!(
        !store.engine.peers.read().contains_key(&newcomer),
        "capped-out sender must not be added to peers"
    );

    task.abort();
}

/// A sender that is already a member continues to be processed normally when the cap is reached.
#[tokio::test(flavor = "multi_thread")]
async fn peer_cap_allows_known_member_at_capacity() {
    let port = 9802u16;
    let target_addr: std::net::IpAddr = "127.0.0.184".parse().unwrap();
    let peer1: std::net::IpAddr = "127.0.0.185".parse().unwrap();
    let peer2: std::net::IpAddr = "127.0.0.186".parse().unwrap();

    let config = Config::default()
        .with_port(port)
        .with_listen_addr(target_addr)
        .with_net("127.0.0.184/30".parse().unwrap())
        .with_max_peers(2)
        .with_insecure_no_key();

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);

    let task = tokio::spawn(store.clone().run());

    // Give the run loop time to enter its receive wait.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send a valid dated payload FROM a known member (peer1), retrying until accepted.
    let payload = dated_comparison_payload();
    let sender = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(peer1, 0))
        .await
        .expect("bind sender");

    let mut peers_refreshed = false;
    for _ in 0..50 {
        let _ = sender
            .send_to(&payload, std::net::SocketAddr::new(target_addr, port))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if store.engine.peers.read().contains_key(&peer1) {
            peers_refreshed = true;
            break;
        }
    }

    // Known member must still be present (not evicted).
    assert!(
        store.engine.members.read().contains(&peer1),
        "known member must not be evicted by the cap"
    );
    // The peers entry is (re-)inserted when the datagram is processed.
    assert!(
        peers_refreshed,
        "known member's peers entry must be refreshed normally"
    );

    task.abort();
}

/// Decommissioning a member frees its slot, letting a new sender through the cap.
#[tokio::test]
async fn decommission_frees_peer_cap_slot() {
    let peer1: std::net::IpAddr = "127.1.0.1".parse().unwrap();
    let peer2: std::net::IpAddr = "127.1.0.2".parse().unwrap();
    let newcomer: std::net::IpAddr = "127.1.0.3".parse().unwrap();

    let config = Config::default()
        .with_port(crate::replica::next_ephemeral_test_port())
        .with_listen_addr("127.0.0.1".parse().unwrap())
        .with_max_peers(2)
        .with_insecure_no_key();

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    // Seed two members (simulates two peers that completed a dated exchange).
    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);
    assert_eq!(store.engine.members.read().len(), 2);

    // At cap: a newcomer would be blocked.
    assert!(
        store.engine.members.read().len() >= 2,
        "precondition: at cap"
    );
    assert!(
        !store.engine.members.read().contains(&newcomer),
        "precondition: newcomer not yet in members"
    );

    // Decommission peer2, freeing one slot.
    store.forget_peer(peer2);
    assert_eq!(
        store.engine.members.read().len(),
        1,
        "one member after decommission"
    );
    assert!(
        !store.engine.members.read().contains(&peer2),
        "peer2 must be removed from members"
    );
    // The peers gossip-routing entry must also be gone (so capacity is visible to the
    // cap check path, and decommission does not leave a ghost entry in the peers map).
    assert!(
        !store.engine.peers.read().contains_key(&peer2),
        "peer2 must be removed from peers by decommission"
    );

    // After decommission: members.len() == 1 < max_peers == 2.
    // The cap check allows the newcomer through: current_len < 2.
    let current_len = store.engine.members.read().len();
    let is_known = store.engine.members.read().contains(&newcomer);
    assert!(
        is_known || current_len < 2,
        "newcomer must not be capped out (members.len={current_len}) after decommission freed a slot"
    );
}

/// In authenticated mode, datagrams from a capped-out sender must not create a replay-filter
/// entry.  The cap check runs before `replay_filter.check_and_record` for unknown senders.
#[tokio::test(flavor = "multi_thread")]
async fn peer_cap_no_replay_entry_for_capped_sender() {
    let port = 9804u16;
    let target_addr: std::net::IpAddr = "127.0.0.192".parse().unwrap();
    let peer1: std::net::IpAddr = "127.0.0.193".parse().unwrap();
    let peer2: std::net::IpAddr = "127.0.0.194".parse().unwrap();
    let newcomer: std::net::IpAddr = "127.0.0.195".parse().unwrap();
    let cluster_key = [0x55u8; 32];

    let config = Config::default()
        .with_port(port)
        .with_listen_addr(target_addr)
        .with_net("127.0.0.192/30".parse().unwrap())
        .with_cluster_key(gossip::auth::ClusterKey::new(cluster_key))
        .with_max_peers(2);

    let store = ReplicatedMap::<i32, i32>::new(config)
        .await
        .expect("bind failed");

    store.engine.members.write().insert(peer1);
    store.engine.members.write().insert(peer2);

    let task = tokio::spawn(store.clone().run());

    // Craft a sealed (authenticated) dated datagram from the newcomer's IP.
    let payload = dated_comparison_payload();
    let counter = gossip::replay::SenderCounter::new();
    let sealed =
        gossip::auth::Authenticator::new(Some(gossip::auth::ClusterKey::new(cluster_key)), false)
            .seal(counter.next_seq(), counter.next_stamp(), &payload);

    let sender = tokio::net::UdpSocket::bind(std::net::SocketAddr::new(newcomer, 0))
        .await
        .expect("bind sender");
    sender
        .send_to(&sealed, std::net::SocketAddr::new(target_addr, port))
        .await
        .expect("send");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The cap fired before the replay filter: no replay entry must have been created.
    assert_eq!(
        store.engine.replay_filter_len(),
        0,
        "no replay-filter entry must be created for a capped-out sender"
    );

    task.abort();
}

/// `get_cloned` must not hold the read lock past its return, so a write immediately
/// following it (the `get`-then-`insert` pattern `get`'s own guard would self-deadlock on)
/// completes without hanging.
#[tokio::test]
async fn get_cloned_does_not_hold_the_lock_across_a_following_write() {
    let store = ReplicatedMap::<i32, i32>::new(ephemeral_config())
        .await
        .unwrap();
    store.insert(1, 10);

    let value = store.get_cloned(&1);
    assert_eq!(value, Some(10));
    // If `get_cloned` still held the read lock here, this write lock acquisition would hang
    // forever instead of returning.
    store.insert(1, 20);

    assert_eq!(store.get_cloned(&1), Some(20));
    assert_eq!(store.get_cloned(&2), None);
}
