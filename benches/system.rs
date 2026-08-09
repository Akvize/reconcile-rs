// Copyright 2023 Developers of the reconcile project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! System-level, end-to-end benchmarks driving the **public** `ReplicatedMap` API (point-read
//! latency vs `HashMap`/`BTreeMap`, per-entry memory footprint, bulk-load throughput, cold anti-
//! entropy convergence between two in-process nodes, gossip fan-out and propagation latency as
//! node count grows, and durable-snapshot reload). Unlike the `bench` target, these reach no crate
//! internals, so they need no feature gate.
//!
//! Reproduction and interpretation are documented in `benches/README.md`. Not run in CI (only
//! compile-checked); run locally with `cargo bench --bench system`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hint::black_box;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    Throughput,
};
use tokio::runtime::Runtime;

use reconcile::{
    replicated_map::Config, Entry, FileSnapshot, Hlc, InMemoryNetwork, InMemoryTransport,
    LogicalCounter, NodeId, PersistedState, Persistence, PhysicalTime, ReplicatedMap, State,
    Timestamp, Transport,
};

/// Dataset sizes swept by the size-parameterised benchmarks (log scale).
const SIZES: &[usize] = &[10, 100, 1_000, 10_000, 100_000];

/// Deterministic `(key, value)` corpus: sequential keys keep every backend's layout comparable.
fn corpus(n: usize) -> Vec<(u32, u32)> {
    (0..n as u32)
        .map(|k| (k, k.wrapping_mul(2_654_435_761)))
        .collect()
}

fn log_group<'a>(
    c: &'a mut Criterion,
    name: &str,
) -> criterion::BenchmarkGroup<'a, criterion::measurement::WallTime> {
    let mut group = c.benchmark_group(name);
    group.plot_config(PlotConfiguration::default().summary_scale(AxisScale::Logarithmic));
    group
}

/// An in-process, peerless store loaded with `kvs` (ephemeral port, so many can coexist).
fn loaded_store(rt: &Runtime, kvs: &[(u32, u32)]) -> ReplicatedMap<u32, u32> {
    rt.block_on(async {
        let store = ReplicatedMap::<u32, u32>::new(
            Config::default()
                .with_port(0)
                .with_listen_addr("127.0.0.1".parse().unwrap())
                .with_net("127.0.0.1/8".parse().unwrap()),
        )
        .await
        .expect("bind failed");
        store.insert_bulk(kvs);
        store
    })
}

/// Point-read latency: `ReplicatedMap::get` against std collections at the same sizes.
fn point_read(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "point_read");
    for &size in SIZES {
        let kvs = corpus(size);
        let probe = kvs[size / 2].0;
        let store = loaded_store(&rt, &kvs);
        let hashmap: HashMap<u32, u32> = kvs.iter().copied().collect();
        let btreemap: BTreeMap<u32, u32> = kvs.iter().copied().collect();

        group.bench_with_input(BenchmarkId::new("ReplicatedMap", size), &size, |b, _| {
            b.iter(|| black_box(store.get(black_box(&probe)).map(|g| *g)));
        });
        group.bench_with_input(BenchmarkId::new("HashMap", size), &size, |b, _| {
            b.iter(|| black_box(hashmap.get(black_box(&probe)).copied()));
        });
        group.bench_with_input(BenchmarkId::new("BTreeMap", size), &size, |b, _| {
            b.iter(|| black_box(btreemap.get(black_box(&probe)).copied()));
        });
    }
    group.finish();
}

/// Bulk-load throughput: `insert_bulk` of `N` fresh entries into an empty store.
fn bulk_load(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "bulk_load");
    for &size in SIZES {
        let kvs = corpus(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            // `insert_bulk` spawns a (peerless, no-op) broadcast, so the measured op needs a runtime
            // context; `block_on` here runs sequentially after the setup's, never nested inside it.
            b.iter_batched(
                || loaded_store(&rt, &[]),
                |store| rt.block_on(async { store.insert_bulk(black_box(&kvs)) }),
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// One-shot report of the fixed per-entry footprint of the dated cell vs the value-only mirror
/// projection, across value payload sizes. Printed rather than timed (it is a compile-time fact).
fn memory_footprint(c: &mut Criterion) {
    fn report<const N: usize>() {
        let dated = std::mem::size_of::<Entry<Timestamp, [u8; N]>>();
        let light = std::mem::size_of::<State<[u8; N]>>();
        println!(
            "[memory] value=[u8; {N}]: dated Entry<Timestamp, V> = {dated} B/entry, \
             value-only State<V> = {light} B/entry, saved = {} B/entry",
            dated.saturating_sub(light)
        );
    }
    report::<8>();
    report::<64>();
    report::<256>();

    // A trivial timed anchor so the report participates in a normal `cargo bench` run.
    c.bench_function("memory_footprint::size_of", |b| {
        b.iter(|| black_box(std::mem::size_of::<Entry<Timestamp, [u8; 64]>>()));
    });
}

/// A fresh, unique loopback address pair for one cold-sync iteration (avoids rebind collisions
/// across Criterion's repeated samples). Both peers share the port; only the address differs.
fn fresh_pair() -> (IpAddr, IpAddr) {
    static N: AtomicU32 = AtomicU32::new(0);
    let i = N.fetch_add(1, Ordering::Relaxed);
    let hi = ((i >> 7) & 0xff) as u8;
    let lo = ((i & 0x7f) as u8) * 2 + 1; // odd octet for peer A, +1 (even) for peer B
    (
        format!("127.1.{hi}.{lo}").parse().unwrap(),
        format!("127.1.{hi}.{}", lo + 1).parse().unwrap(),
    )
}

/// Cold-sync: how long an empty node takes to converge with a full one purely via anti-entropy.
///
/// Peer A is pre-loaded **before it has any peer**, so nothing is broadcast eagerly; peer B (empty)
/// seeds A and pulls the whole dataset through the range-diff protocol. We time from spawning the
/// run loops until B's fingerprint matches A's.
fn cold_sync(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let port = 9_500;
    let mut group = log_group(c, "cold_sync");
    // Convergence dominates wall time, so keep the dataset and sample count modest.
    for &size in &[1_000usize, 10_000, 100_000] {
        let kvs = corpus(size);
        group.throughput(Throughput::Elements(size as u64));
        group.sample_size(10);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let (addr_a, addr_b) = fresh_pair();
                        let cfg = |addr: IpAddr| {
                            Config::default()
                                .with_port(port)
                                .with_listen_addr(addr)
                                .with_net("127.0.0.1/8".parse().unwrap())
                        };
                        // A is loaded with no peer declared, so `insert_bulk` broadcasts to nobody.
                        let a = ReplicatedMap::<u32, u32>::new(cfg(addr_a))
                            .await
                            .expect("bind A");
                        a.insert_bulk(&kvs);
                        let target = a.fingerprint(..);
                        let b_store = ReplicatedMap::<u32, u32>::new(cfg(addr_b))
                            .await
                            .expect("bind B")
                            .with_seed(addr_a);

                        let start = Instant::now();
                        let ta = tokio::spawn(a.clone().run());
                        let tb = tokio::spawn(b_store.clone().run());
                        while b_store.fingerprint(..) != target {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        total += start.elapsed();
                        ta.abort();
                        tb.abort();
                    }
                    total
                })
            });
        });
    }
    group.finish();
}

/// A [`Transport`] wrapping [`InMemoryTransport`] that tallies every datagram this node *sends*.
/// Broadcast fan-out is a send-side cost (`Replica::broadcast` in `src/replica.rs` iterates every
/// known peer with no bound), so only sends are counted — receipts aren't needed by either
/// benchmark below.
struct CountingTransport {
    inner: InMemoryTransport,
    datagrams_sent: Arc<AtomicU64>,
    bytes_sent: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl Transport for CountingTransport {
    type Addr = SocketAddr;

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf).await
    }

    async fn send_to(&self, buf: &[u8], dst: &SocketAddr) -> io::Result<usize> {
        let n = self.inner.send_to(buf, dst).await?;
        self.datagrams_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// A [`CountingTransport`]'s counters, readable after the transport has been moved into a store.
#[derive(Clone, Default)]
struct TrafficCounters {
    datagrams_sent: Arc<AtomicU64>,
    bytes_sent: Arc<AtomicU64>,
}

/// `n` distinct loopback addresses for one mesh. Each call uses a fresh third octet, so meshes
/// built by separate calls never collide even if their `InMemoryNetwork`s somehow did.
fn mesh_addrs(n: usize) -> Vec<IpAddr> {
    static BLOCK: AtomicU32 = AtomicU32::new(1);
    let block = BLOCK.fetch_add(1, Ordering::Relaxed);
    assert!(n < 255, "mesh_addrs: {n} nodes don't fit in one /24 octet");
    (0..n as u32)
        .map(|i| format!("127.3.{block}.{}", i + 1).parse().unwrap())
        .collect()
}

/// `n` in-process nodes on a fresh [`InMemoryNetwork`], **full-mesh-seeded**: every node is handed
/// every other node's address via `seed_peer` up front, rather than left to learn peers through
/// gossip. That isolates the fan-out/propagation cost under test from peer-discovery convergence
/// time, which is a separate concern already covered by `cold_sync`. Each node's send traffic is
/// counted; the returned `Vec<TrafficCounters>` shares index with the stores.
fn build_mesh(n: usize, port: u16) -> (Vec<ReplicatedMap<u32, u32>>, Vec<TrafficCounters>) {
    let network = InMemoryNetwork::new();
    let addrs = mesh_addrs(n);
    let mut stores = Vec::with_capacity(n);
    let mut counters = Vec::with_capacity(n);
    for &addr in &addrs {
        let traffic = TrafficCounters::default();
        let transport = CountingTransport {
            inner: network.bind(SocketAddr::new(addr, port)),
            datagrams_sent: Arc::clone(&traffic.datagrams_sent),
            bytes_sent: Arc::clone(&traffic.bytes_sent),
        };
        let config = Config::default()
            .with_port(port)
            .with_listen_addr(addr)
            .with_net("127.0.0.1/8".parse().unwrap());
        stores.push(ReplicatedMap::<u32, u32>::new_with_transport(
            config,
            Arc::new(transport),
        ));
        counters.push(traffic);
    }
    for (i, store) in stores.iter().enumerate() {
        for (j, &addr) in addrs.iter().enumerate() {
            if i != j {
                store.seed_peer(addr);
            }
        }
    }
    (stores, counters)
}

/// Cooperatively yields until `counter` reaches `target`, instead of sleeping: in-process delivery
/// has no I/O latency to wait out, only scheduler turns.
async fn wait_for(counter: &AtomicU64, target: u64) {
    for _ in 0..1_000_000 {
        if counter.load(Ordering::Relaxed) >= target {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("gossip benchmark: counter did not reach {target} in time");
}

/// Node counts for `gossip_fanout`: send-side only, no running receive loop, so this stays cheap
/// out to a few hundred simulated peers.
const FANOUT_NODE_COUNTS: &[usize] = &[2, 4, 8, 16, 32, 64, 128];

/// Node counts for `gossip_propagation`: every node runs a live receive/reconcile loop on the same
/// Tokio runtime, so this is kept deliberately smaller than `FANOUT_NODE_COUNTS` — past a few dozen
/// simulated peers sharing one process, the benchmark increasingly measures its own scheduler and
/// lock contention (`peers`/`map` `RwLock`s) rather than the protocol's real scaling behavior. See
/// `benches/README.md` for this and the other caveats specific to these two benchmarks.
const PROPAGATION_NODE_COUNTS: &[usize] = &[2, 4, 8, 16, 32];

/// Gossip fan-out: bytes/datagrams *one node* sends for a single write, as peer count `N` grows.
///
/// `Replica::broadcast` (`src/replica.rs`) sends every local write to **all** known peers with no
/// bound — only the separate, periodic WAN anti-entropy round is capped by `remote_fanout`. This
/// benchmark quantifies that O(N) per-node cost directly instead of leaving it as a documented but
/// unmeasured claim (`SOTA.md` §1.2, issue #174): both the deterministic traffic count (printed,
/// like `memory_footprint` — it's a fixed message size × (N-1) peers, not a noisy statistic) and
/// the timed wall-clock cost of one node's send loop as N grows.
fn gossip_fanout(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "gossip_fanout");
    for &n in FANOUT_NODE_COUNTS {
        let (stores, counters) = build_mesh(n, 9_600);
        let origin = &stores[0];
        let origin_counters = &counters[0];

        rt.block_on(async {
            let before = origin_counters.datagrams_sent.load(Ordering::Relaxed);
            origin.insert(u32::MAX, 0);
            wait_for(&origin_counters.datagrams_sent, before + (n as u64 - 1)).await;
            let datagrams = origin_counters.datagrams_sent.load(Ordering::Relaxed) - before;
            let bytes = origin_counters.bytes_sent.load(Ordering::Relaxed);
            println!(
                "[gossip_fanout] N={n}: one write -> {datagrams} datagrams / {bytes} B sent by the \
                 origin node alone; O(N) per-node fan-out means ~{cluster_datagrams} datagrams \
                 cluster-wide if every node writes once",
                cluster_datagrams = datagrams * n as u64,
            );
        });

        group.sample_size(10.max(200 / n).min(100));
        group.throughput(Throughput::Elements((n - 1) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut key = 0u32;
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        key = key.wrapping_add(1);
                        let before = origin_counters.datagrams_sent.load(Ordering::Relaxed);
                        let start = Instant::now();
                        origin.insert(black_box(key), key);
                        wait_for(&origin_counters.datagrams_sent, before + (n as u64 - 1)).await;
                        total += start.elapsed();
                    }
                    total
                })
            });
        });
    }
    group.finish();
}

/// Write→read propagation latency: wall time from a write on one node to **every** other node
/// observing it, as peer count `N` grows.
///
/// Unlike `gossip_fanout`, every node here runs its real receive/reconcile loop throughout, so this
/// exercises the full send + receive + apply path the way a reader actually experiences it — the
/// steady-state counterpart to `cold_sync`'s from-scratch convergence.
fn gossip_propagation(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "gossip_propagation");
    for &n in PROPAGATION_NODE_COUNTS {
        let (stores, _counters) = build_mesh(n, 9_700);
        let run_tasks: Vec<_> = rt.block_on(async {
            stores
                .iter()
                .cloned()
                .map(|store| tokio::spawn(store.run()))
                .collect::<Vec<_>>()
        });

        group.sample_size(10);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut key = 0u32;
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        key = key.wrapping_add(1);
                        let start = Instant::now();
                        stores[0].insert(black_box(key), key);
                        while !stores[1..n].iter().all(|s| s.get(&key).is_some()) {
                            tokio::task::yield_now().await;
                        }
                        total += start.elapsed();
                    }
                    total
                })
            });
        });

        for task in run_tasks {
            task.abort();
        }
    }
    group.finish();
}

/// Durable rejoin: time to reload a persisted snapshot of `N` entries from disk (the cost a
/// restarting node pays before rejoining the cluster). The snapshot is written once in setup.
fn durable_rejoin(c: &mut Criterion) {
    let mut group = log_group(c, "durable_rejoin");
    for &size in SIZES {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = FileSnapshot::new(dir.path().join("reconcile.snapshot"));
        let state: PersistedState<u32, u32> = PersistedState {
            entries: (0..size as u32)
                .map(|k| {
                    (
                        k,
                        Entry::present(
                            Timestamp::new(
                                Hlc::new(
                                    PhysicalTime::from_millis(k as u64),
                                    LogicalCounter::new(0),
                                ),
                                NodeId::new(0),
                            ),
                            k,
                        ),
                    )
                })
                .collect(),
            members: HashSet::new(),
            tombstone_acks: HashMap::new(),
        };
        Persistence::<u32, u32>::save(&snapshot, &state).expect("save");

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| black_box(Persistence::<u32, u32>::load(&snapshot).expect("load")));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    point_read,
    bulk_load,
    memory_footprint,
    cold_sync,
    gossip_fanout,
    gossip_propagation,
    durable_rejoin
);
criterion_main!(benches);
