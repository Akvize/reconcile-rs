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
//! node count grows, convergence under injected RTT and loss, and durable-snapshot reload). Unlike
//! the `bench` target, these reach no crate internals, so they need no feature gate.
//!
//! The `*_rtt` lanes answer the round-trip question: every other benchmark here runs at RTT ≈ 0,
//! which prices bytes and zeroes round-trips — the axis RBSR is worst on. They run over the seeded
//! delay/loss decorator in `benches/netem/mod.rs`, whose module docs carry the model and the
//! `turmoil` evaluation.
//!
//! Reproduction and interpretation are documented in `benches/README.md`. Not run in CI (only
//! compile-checked); run locally with `cargo bench --bench system`.

use std::collections::{BTreeMap, HashMap};
use std::hint::black_box;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{
    criterion_group, criterion_main, AxisScale, BenchmarkId, Criterion, PlotConfiguration,
    SamplingMode, Throughput,
};
use tokio::runtime::Runtime;

use reconcile::{
    replicated_map::Config, Entry, FileSnapshot, Hlc, InMemoryNetwork, InMemoryTransport,
    LogicalCounter, NodeId, PersistedState, Persistence, PhysicalTime, ReplicatedMap, State,
    Timestamp, Transport,
};

// The netem decorator lives in a subdirectory so cargo's bench auto-discovery (which picks up
// `benches/*.rs` and `benches/*/main.rs`) does not mistake it for a fourth `harness = false`
// target. `tests/netem.rs` includes the same file, and is where it is tested.
#[path = "netem/mod.rs"]
mod netem;

use netem::{Link, Netem, NetemTransport, Probability, Rtt, Seed};

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

/// A fresh port per call — `Config::port` must be nonzero — so many [`loaded_store`]s can
/// coexist.
fn next_bench_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(22_000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// An in-process, peerless store loaded with `kvs`.
fn loaded_store(rt: &Runtime, kvs: &[(u32, u32)]) -> ReplicatedMap<u32, u32> {
    rt.block_on(async {
        let store = ReplicatedMap::<u32, u32>::new(
            Config::default()
                .with_port(next_bench_port())
                .with_listen_addr("127.0.0.1".parse().unwrap())
                .with_net("127.0.0.1/8".parse().unwrap())
                .with_insecure_no_key(),
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
///
/// 64 pairs per third octet, 256 third octets. The pair occupies host octets `2p+1` and `2p+2`, so
/// `p` has to stay under 127 for the *second* of them to remain an octet: a wider mask overflows on
/// the 128th call, which the release profile's `overflow-checks = true` (`Cargo.toml`) turns into a
/// panic partway through the benchmark rather than a wrong address.
fn fresh_pair() -> (IpAddr, IpAddr) {
    static N: AtomicU32 = AtomicU32::new(0);
    let i = N.fetch_add(1, Ordering::Relaxed);
    let hi = ((i >> 6) & 0xff) as u8;
    let lo = ((i & 0x3f) as u8) * 2 + 1; // odd octet for peer A, +1 (even) for peer B
    (
        format!("127.1.{hi}.{lo}").parse().unwrap(),
        format!("127.1.{hi}.{}", lo + 1).parse().unwrap(),
    )
}

/// Cold-sync: how long an empty node takes to converge with a full one purely via anti-entropy.
///
/// A is loaded before it has any peer, so nothing is broadcast eagerly; timed from spawning the
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
                                .with_insecure_no_key()
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

/// An [`InMemoryTransport`] wrapper tallying every datagram this node *sends* — fan-out is a
/// send-side cost, so receipts are not counted.
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
///
/// The octet cycles rather than growing: the RTT lanes rebuild their cluster once per iteration,
/// thousands of times per run, and each mesh's own `InMemoryNetwork` already isolates it — whereas
/// a 256th block would not be an address at all.
fn mesh_addrs(n: usize) -> Vec<IpAddr> {
    static BLOCK: AtomicU32 = AtomicU32::new(1);
    let block = BLOCK.fetch_add(1, Ordering::Relaxed) % 254 + 1;
    assert!(n < 255, "mesh_addrs: {n} nodes don't fit in one /24 octet");
    (0..n as u32)
        .map(|i| format!("127.3.{block}.{}", i + 1).parse().unwrap())
        .collect()
}

/// `n` in-process nodes on a fresh [`InMemoryNetwork`], each endpoint handed to `wrap` before it
/// reaches the store. **Unseeded**: who knows whom is the caller's decision, and the two families
/// of benchmark built on this differ on exactly that (see [`full_mesh_seed`]).
fn mesh_with<T: Transport<Addr = SocketAddr>>(
    n: usize,
    port: u16,
    mut wrap: impl FnMut(InMemoryTransport) -> T,
) -> (Vec<ReplicatedMap<u32, u32>>, Vec<IpAddr>) {
    let network = InMemoryNetwork::new();
    let addrs = mesh_addrs(n);
    let stores = addrs
        .iter()
        .map(|&addr| {
            let transport = wrap(network.bind(SocketAddr::new(addr, port)));
            let config = Config::default()
                .with_port(port)
                .with_listen_addr(addr)
                .with_net("127.0.0.1/8".parse().unwrap())
                .with_insecure_no_key();
            ReplicatedMap::<u32, u32>::new_with_transport(config, Arc::new(transport))
        })
        .collect();
    (stores, addrs)
}

/// Seed every node with every other, so a measurement excludes peer-discovery time.
fn full_mesh_seed(stores: &[ReplicatedMap<u32, u32>], addrs: &[IpAddr]) {
    for (i, store) in stores.iter().enumerate() {
        for (j, &addr) in addrs.iter().enumerate() {
            if i != j {
                store.seed_peer(addr);
            }
        }
    }
}

/// `n` in-process nodes on a fresh [`InMemoryNetwork`], **full-mesh-seeded** so the measurement
/// excludes peer-discovery time. The returned counters share index with the stores.
fn build_mesh(n: usize, port: u16) -> (Vec<ReplicatedMap<u32, u32>>, Vec<TrafficCounters>) {
    let mut counters = Vec::with_capacity(n);
    let (stores, addrs) = mesh_with(n, port, |inner| {
        let traffic = TrafficCounters::default();
        let transport = CountingTransport {
            inner,
            datagrams_sent: Arc::clone(&traffic.datagrams_sent),
            bytes_sent: Arc::clone(&traffic.bytes_sent),
        };
        counters.push(traffic);
        transport
    });
    full_mesh_seed(&stores, &addrs);
    (stores, counters)
}

/// `n` in-process nodes whose every outbound datagram crosses `link`. **Unseeded**, like
/// [`mesh_with`]. Call inside a runtime: each node's decorator spawns a delivery pump.
fn netem_mesh(
    n: usize,
    port: u16,
    link: Link,
    seed: Seed,
) -> (Vec<ReplicatedMap<u32, u32>>, Vec<IpAddr>) {
    mesh_with(n, port, |inner| {
        NetemTransport::new(Arc::new(inner), Netem::uniform(link, seed))
    })
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

/// Node counts for `gossip_propagation`, smaller than `FANOUT_NODE_COUNTS`: every node runs a live
/// loop on one runtime, so past a few dozen this measures scheduler and lock contention. Caveats:
/// `benches/README.md`.
const PROPAGATION_NODE_COUNTS: &[usize] = &[2, 4, 8, 16, 32];

/// Gossip fan-out: what one node sends for a single write as `N` grows. `Replica::broadcast` is
/// unbounded — only the periodic WAN round is capped by `remote_fanout` — so this quantifies that
/// `O(N)` cost (`SOTA.md` §1.2). Traffic is printed, not timed: it is exact, not a statistic.
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

/// Write→read propagation latency as `N` grows, with every node running its real loop — the full
/// send/receive/apply path, and the steady-state counterpart to `cold_sync`.
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
        // Outside `bench_with_input`, not inside its routine: Criterion calls that routine once per
        // sample (and again for warm-up), so a counter declared in there restarts at zero every
        // time and every sample after the first re-writes a key the cluster already holds — which
        // completes instantly and reports the poll loop instead of the propagation.
        let mut key = 0u32;
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
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

/// Round-trip times swept by the `*_rtt` lanes: loopback, a datacentre fabric, a LAN, a
/// same-continent WAN, an intercontinental one. Stated as RTT because that is what an operator
/// measures; the decorator injects half of each in either direction.
fn rtt_sweep() -> Vec<Rtt> {
    [0.0, 0.1, 1.0, 10.0, 50.0]
        .into_iter()
        .map(Rtt::from_millis)
        .collect()
}

/// Loss rates swept alongside the RTT sweep — the band a healthy WAN path sits in. Held at
/// [`LOSS_LANE_RTT`] so the loss lane varies one thing.
fn loss_sweep() -> Vec<Probability> {
    [0.1, 1.0].into_iter().map(Probability::percent).collect()
}

/// The RTT the loss lane runs at.
const LOSS_LANE_RTT: f64 = 1.0;

/// Datagrams per delay calibration point. Small: at the top of the sweep each one costs a full
/// one-way delay, and the quantity is a mean, not a tail.
const CALIBRATION_DATAGRAMS: usize = 50;

/// Datagrams per loss calibration point. Large, and at zero delay so it stays cheap: 0.1 % of
/// anything smaller rounds to no drops at all.
const LOSS_CALIBRATION_DATAGRAMS: usize = 20_000;

/// Send `datagrams` one at a time across `link`, returning the observed one-way delivery latency
/// (mean, worst) and the loss the model actually realized.
async fn probe_link(link: Link, datagrams: usize, port: u16) -> (Duration, Duration, f64) {
    let network = InMemoryNetwork::new();
    let addrs = mesh_addrs(2);
    let (src, dst) = (
        SocketAddr::new(addrs[0], port),
        SocketAddr::new(addrs[1], port),
    );
    let sender = NetemTransport::new(
        Arc::new(network.bind(src)),
        Netem::uniform(link, Seed::DEFAULT),
    );
    let receiver = network.bind(dst);
    let impairments = sender.impairments();

    let (mut total, mut worst, mut delivered) = (Duration::ZERO, Duration::ZERO, 0u32);
    let mut buf = [0u8; 64];
    for i in 0..datagrams {
        let dropped_before = impairments.dropped();
        let start = Instant::now();
        sender
            .send_to(&(i as u64).to_le_bytes(), &dst)
            .await
            .unwrap();
        // A drop is reported as a successful send, so ask the model rather than waiting forever.
        if impairments.dropped() != dropped_before {
            continue;
        }
        receiver.recv_from(&mut buf).await.unwrap();
        let elapsed = start.elapsed();
        total += elapsed;
        worst = worst.max(elapsed);
        delivered += 1;
    }
    (total / delivered.max(1), worst, impairments.loss_fraction())
}

/// Calibrate the instrument before reading anything it produces: configured one-way delay against
/// observed, configured loss against realized. Printed rather than timed, like `memory_footprint`
/// — and the reason the sweeps below can be quoted as measurements of the protocol rather than of
/// tokio's timer, which rounds a sleep up to the next millisecond (`benches/netem/mod.rs`).
fn netem_calibration(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    println!(
        "[netem] seed={:#x}; delay is one-way (half the stated RTT), {CALIBRATION_DATAGRAMS} \
         datagrams per delay point, {LOSS_CALIBRATION_DATAGRAMS} per loss point",
        Seed::DEFAULT.get()
    );
    rt.block_on(async {
        for rtt in rtt_sweep() {
            let (mean, worst, _) = probe_link(Link::at(rtt), CALIBRATION_DATAGRAMS, 9_950).await;
            println!(
                "[netem] {label:<12} injected one-way {injected:>9.3?} -> observed mean \
                 {mean:>9.3?}, worst {worst:>9.3?}",
                label = rtt.label(),
                injected = rtt.one_way(),
            );
        }
        for loss in loss_sweep() {
            let link = Link::PERFECT.with_loss(loss);
            let (_, _, realized) = probe_link(link, LOSS_CALIBRATION_DATAGRAMS, 9_951).await;
            println!(
                "[netem] {label:<12} configured {configured:.3} % -> realized {realized:.3} %",
                label = loss.label(),
                configured = loss.as_fraction() * 100.0,
                realized = realized * 100.0,
            );
        }
    });

    // The harness's own per-datagram cost, and therefore the floor under every lane below: what a
    // send through the decorator costs when the link is perfect and delivery is immediate.
    let (sender, dst) = rt.block_on(async {
        let network = InMemoryNetwork::new();
        let addrs = mesh_addrs(2);
        let dst = SocketAddr::new(addrs[1], 9_952);
        network.bind(dst);
        let sender = NetemTransport::new(
            Arc::new(network.bind(SocketAddr::new(addrs[0], 9_952))),
            Netem::uniform(Link::PERFECT, Seed::DEFAULT),
        );
        (sender, dst)
    });
    c.bench_function("netem_overhead::send_to", |b| {
        b.iter(|| rt.block_on(async { sender.send_to(black_box(b"probe"), &dst).await.unwrap() }));
    });
}

/// Dataset sizes for the cold-sync RTT lane, deliberately below `cold_sync`'s: the quantity of
/// interest is the *delta* against RTT 0 in the same harness, not the size curve `cold_sync`
/// already draws, and at 50 ms every sample costs the whole round-trip chain.
const RTT_SIZES: &[usize] = &[1_000, 10_000];

/// One cold-sync convergence over `link`, `iters` times: an empty node seeded to a full one, timed
/// from spawning both run loops until the fingerprints match. A fresh two-node network per
/// iteration, so no state and no impairment carries over.
async fn cold_sync_over(kvs: &[(u32, u32)], link: Link, iters: u64) -> Duration {
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        let (stores, addrs) = netem_mesh(2, 9_800, link, Seed::DEFAULT);
        let (full, empty) = (&stores[0], &stores[1]);
        // Loaded before it has a peer, so nothing is broadcast eagerly — as in `cold_sync`.
        full.insert_bulk(kvs);
        let target = full.fingerprint(..);
        // Only the empty node seeds, also as in `cold_sync`: two peers each initiating over the
        // same difference would dump the dataset twice and measure that instead.
        empty.seed_peer(addrs[0]);

        let start = Instant::now();
        let tasks = [
            tokio::spawn(full.clone().run()),
            tokio::spawn(empty.clone().run()),
        ];
        // Yield rather than sleep: tokio rounds a sleep up to a millisecond, which is half of what
        // this whole benchmark costs in the RTT-0 lane the deltas are taken against.
        while empty.fingerprint(..) != target {
            tokio::task::yield_now().await;
        }
        total += start.elapsed();
        for task in tasks {
            task.abort();
        }
    }
    total
}

/// `cold_sync` across the RTT sweep and a loss lane: what the round-trip chain costs once
/// round-trips are not free. The RTT-0 column is the same harness with a perfect link, so the
/// delta is the injected network and nothing else.
fn cold_sync_rtt(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "cold_sync_rtt");
    group.sample_size(10);
    // Flat sampling: one convergence per sample. Criterion's default linear mode extrapolates from
    // batches of iterations, which is for benchmarks far shorter than these.
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_millis(500));
    for &size in RTT_SIZES {
        let kvs = corpus(size);
        let id = format!("n={size}");
        for rtt in rtt_sweep() {
            group.bench_with_input(BenchmarkId::new(&id, rtt.label()), &rtt, |b, &rtt| {
                b.iter_custom(|iters| rt.block_on(cold_sync_over(&kvs, Link::at(rtt), iters)));
            });
        }
        for loss in loss_sweep() {
            let link = Link::at(Rtt::from_millis(LOSS_LANE_RTT)).with_loss(loss);
            group.bench_with_input(BenchmarkId::new(&id, loss.label()), &loss, |b, _| {
                b.iter_custom(|iters| rt.block_on(cold_sync_over(&kvs, link, iters)));
            });
        }
    }
    group.finish();
}

/// Node count for the propagation RTT lane: fixed and modest. `gossip_propagation` already sweeps
/// `N` at RTT ≈ 0 and starts measuring its own scheduler past a few dozen, so holding `N` still is
/// what makes the sweep vary one thing.
const PROPAGATION_RTT_NODES: usize = 8;

/// `gossip_propagation` across the RTT sweep and a loss lane. A write is broadcast to every peer at
/// once, so this asks whether propagation is one hop (≈ RTT/2, flat in `N`) or a chain — and, in
/// the loss lane, what a dropped broadcast costs when only the next anti-entropy round can repair
/// it.
fn gossip_propagation_rtt(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = log_group(c, "gossip_propagation_rtt");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_millis(500));
    let id = format!("N={PROPAGATION_RTT_NODES}");

    let mut lanes: Vec<(String, Link)> = rtt_sweep()
        .into_iter()
        .map(|rtt| (rtt.label(), Link::at(rtt)))
        .collect();
    lanes.extend(loss_sweep().into_iter().map(|loss| {
        (
            loss.label(),
            Link::at(Rtt::from_millis(LOSS_LANE_RTT)).with_loss(loss),
        )
    }));

    for (label, link) in lanes {
        let (stores, tasks) = rt.block_on(async {
            let (stores, addrs) = netem_mesh(PROPAGATION_RTT_NODES, 9_900, link, Seed::DEFAULT);
            full_mesh_seed(&stores, &addrs);
            let tasks: Vec<_> = stores
                .iter()
                .cloned()
                .map(|store| tokio::spawn(store.run()))
                .collect();
            (stores, tasks)
        });

        // A never-repeating key, for the reason spelled out in `gossip_propagation`.
        let mut key = 0u32;
        group.bench_with_input(BenchmarkId::new(&id, &label), &label, |b, _| {
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        key = key.wrapping_add(1);
                        let start = Instant::now();
                        stores[0].insert(black_box(key), key);
                        while !stores[1..].iter().all(|s| s.get(&key).is_some()) {
                            tokio::task::yield_now().await;
                        }
                        total += start.elapsed();
                    }
                    total
                })
            });
        });

        for task in tasks {
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
        let state: PersistedState<u32, u32> = PersistedState::from(
            (0..size as u32)
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
                .collect::<Vec<_>>(),
        );
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
    netem_calibration,
    cold_sync_rtt,
    gossip_propagation_rtt,
    durable_rejoin
);
criterion_main!(benches);
