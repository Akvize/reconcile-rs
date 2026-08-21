// The benchmark drives the range-fingerprint via `FingerprintTreeMap::aggregate`, which is public
// on the standalone `rsos` crate — so, unlike when it went through the gated `reconcile::testing`
// seam, the bench body needs no feature gate at all.
use imp::main;

// `service_reconcile_rtt` below composes `just_insert`/`just_remove` (`reconcile_internal_testing`
// seams, AGENTS.md §6) with the injected-RTT decorator, so it lives here rather than in
// `system.rs`, which is deliberately feature-gate-free (`benches/README.md` "Pricing that
// end-to-end..."). Included the same way `system.rs` includes it — a `#[path]` `mod` rather than
// `benches/*/main.rs` auto-discovery, so cargo does not mistake it for a fifth target;
// `tests/netem.rs` is where it is tested.
#[path = "netem/mod.rs"]
mod netem;

mod imp {
    use std::collections::BTreeMap;
    use std::io;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rand::{distributions::Standard, Rng, SeedableRng};

    use criterion::{
        criterion_group, AxisScale, BenchmarkId, Criterion, PlotConfiguration, SamplingMode,
        Throughput,
    };

    use tokio_util::sync::CancellationToken;

    use reconcile::{
        replicated_map::Config, Entry, FingerprintTreeMap, Hlc, InMemoryNetwork, LogicalCounter,
        NodeId, PhysicalTime, ReplicatedMap, State, Timestamp, Transport,
    };

    use super::netem::{Link, Netem, NetemTransport, Rtt, Seed};

    fn fingerprint_tree_map_new(c: &mut Criterion) {
        let mut group = c.benchmark_group("FingerprintTreeMap::new");
        group.bench_function("BTreeMap::new()", |b| b.iter(BTreeMap::<u32, u32>::new));
        group.bench_function("FingerprintTreeMap::new()", |b| {
            b.iter(FingerprintTreeMap::<u32, u32>::new)
        });
    }

    fn fingerprint_tree_map_fill(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("FingerprintTreeMap::fill");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.throughput(Throughput::Elements(size as u64));
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(
                BenchmarkId::new("BTreeMap::fill", size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let mut tree = BTreeMap::<u32, u32>::new();
                        for (k, v) in key_values[..size].iter().copied() {
                            tree.insert(k, v);
                        }
                    })
                },
            );
            group.bench_with_input(
                BenchmarkId::new("FingerprintTreeMap::fill", size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let mut tree = FingerprintTreeMap::<u32, u32>::new();
                        for (k, v) in key_values[..size].iter().copied() {
                            tree.insert(k, v);
                        }
                    })
                },
            );
            size *= 10;
        }
    }

    fn fingerprint_tree_map_insert(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("FingerprintTreeMap::insert");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.throughput(Throughput::Elements(size as u64));
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(
                BenchmarkId::new("BTreeMap::insert", size),
                &size,
                |b, &size| {
                    let mut tree = BTreeMap::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                    b.iter(|| {
                        // NOTE: do the insertion first because inserting a just-removed element is
                        // likely easier; do not reuse the same key, since it was just removed during
                        // the last iteration
                        let k = rng.gen();
                        let v = rng.gen();
                        tree.insert(k, v);
                        tree.remove(&k);
                    })
                },
            );
            group.bench_with_input(
                BenchmarkId::new("FingerprintTreeMap::insert", size),
                &size,
                |b, &size| {
                    let mut tree = FingerprintTreeMap::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                    b.iter(|| {
                        // NOTE: do the insertion first because inserting a just-removed element is
                        // likely easier; do not reuse the same key, since it was just removed during
                        // the last iteration
                        let k = rng.gen();
                        let v = rng.gen();
                        tree.insert(k, v);
                        tree.remove(&k);
                    })
                },
            );
            size *= 10;
        }
    }

    fn fingerprint_tree_map_remove(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("FingerprintTreeMap::remove");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.throughput(Throughput::Elements(size as u64));
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(
                BenchmarkId::new("BTreeMap::remove", size),
                &size,
                |b, &size| {
                    let mut tree = BTreeMap::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                    b.iter(|| {
                        // NOTE: do the removal first because removing a just-inserted element is
                        // likely easier; do not reuse the same key, since it was just reinserted
                        // during the last iteration
                        let idx = rng.gen_range(0..size);
                        let (k, v) = &key_values[idx];
                        tree.remove(k);
                        tree.insert(*k, *v);
                    })
                },
            );
            group.bench_with_input(
                BenchmarkId::new("FingerprintTreeMap::remove", size),
                &size,
                |b, &size| {
                    let mut tree = FingerprintTreeMap::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                    b.iter(|| {
                        // NOTE: do the removal first because removing a just-inserted element is
                        // likely easier; do not reuse the same key, since it was just reinserted
                        // during the last iteration
                        let idx = rng.gen_range(0..size);
                        let (k, v) = &key_values[idx];
                        tree.remove(k);
                        tree.insert(*k, *v);
                    })
                },
            );
            size *= 10;
        }
    }

    fn fingerprint_tree_map_range_fingerprint(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("FingerprintTreeMap::aggregate");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
                let mut tree = FingerprintTreeMap::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
                b.iter(|| {
                    let k1: u32 = rng.gen();
                    let k2: u32 = rng.gen();
                    let range = if k1 < k2 { k1..k2 } else { k2..k1 };
                    tree.aggregate(range);
                })
            });
            size *= 10;
        }
    }

    /// In-memory cost of a dated replica (`FingerprintTreeMap<K, Entry<Timestamp, V>>`) against
    /// the value-only one (`FingerprintTreeMap<K, State<V>>`).
    ///
    /// Criterion times the fill at growing sizes; the report below adds bytes per entry.
    fn read_replica_memory(c: &mut Criterion) {
        let dated = std::mem::size_of::<Entry<Timestamp, u32>>();
        let light = std::mem::size_of::<State<u32>>();
        println!(
            "[read replica memory] per-entry value size: dated Entry<Timestamp, u32> = {dated} B, \
         value-only State<u32> = {light} B, saved = {} B/entry",
            dated - light
        );

        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut keys = Vec::new();
        for _ in 0..1_000_000 {
            keys.push(rng.gen::<u32>());
        }
        let keys = &keys;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("read_replica_memory::fill");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= keys.len() {
            group.throughput(Throughput::Elements(size as u64));
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(
                BenchmarkId::new("dated Entry<Timestamp, u32>", size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let mut tree = FingerprintTreeMap::<u32, Entry<Timestamp, u32>>::new();
                        for &k in keys[..size].iter() {
                            tree.insert(
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
                            );
                        }
                    })
                },
            );
            group.bench_with_input(
                BenchmarkId::new("value-only State<u32>", size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let mut tree = FingerprintTreeMap::<u32, State<u32>>::new();
                        for &k in keys[..size].iter() {
                            tree.insert(k, State::Present(k));
                        }
                    })
                },
            );
            size *= 10;
        }
    }

    fn service_send(c: &mut Criterion) {
        let port = 8080;
        let net = "127.0.0.1/8".parse().unwrap();
        let addr1 = "127.0.0.44".parse().unwrap();
        let addr2 = "127.0.0.45".parse().unwrap();
        let cfg1 = Config::default()
            .with_port(port)
            .with_listen_addr(addr1)
            .with_net(net)
            .with_insecure_no_key();
        let cfg2 = Config::default()
            .with_port(port)
            .with_listen_addr(addr2)
            .with_net(net)
            .with_insecure_no_key();

        let mut rng = rand::rngs::ThreadRng::default();

        let key_values: Vec<(u32, u32)> =
            (&mut rng).sample_iter(Standard).take(1_000_000).collect();

        let rt = tokio::runtime::Runtime::new().unwrap();

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("ReplicatedMap::send");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
                rt.block_on(async {
                    let store1 = ReplicatedMap::new(cfg1.clone())
                        .await
                        .expect("bind failed")
                        .with_seed(addr2);
                    store1.insert_bulk(&key_values[..size]);
                    let store2 = ReplicatedMap::new(cfg2.clone())
                        .await
                        .expect("bind failed")
                        .with_seed(addr1);
                    store2.insert_bulk(&key_values[..size]);
                    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
                    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

                    b.iter(|| {
                        let k: u32 = rng.gen();
                        let v: u32 = rng.gen();
                        store1.insert(k, v);
                        while store2.get(&k).is_none() {
                            std::thread::sleep(Duration::from_micros(1));
                        }
                        store1.remove(&k);
                        while store2.get(&k).is_some() {
                            std::thread::sleep(Duration::from_micros(1));
                        }
                    });

                    task2.abort();
                    task1.abort();
                    let _ = tokio::join!(task1, task2);
                })
            });
            size *= 10;
        }
    }

    fn service_reconcile(c: &mut Criterion) {
        let port = 8080;
        let net = "127.0.0.1/8".parse().unwrap();
        let addr1 = "127.0.0.44".parse().unwrap();
        let addr2 = "127.0.0.45".parse().unwrap();
        let cfg1 = Config::default()
            .with_port(port)
            .with_listen_addr(addr1)
            .with_net(net)
            .with_insecure_no_key();
        let cfg2 = Config::default()
            .with_port(port)
            .with_listen_addr(addr2)
            .with_net(net)
            .with_insecure_no_key();

        let mut rng = rand::rngs::ThreadRng::default();

        let key_values: Vec<(u32, u32)> =
            (&mut rng).sample_iter(Standard).take(1_000_000).collect();

        let rt = tokio::runtime::Runtime::new().unwrap();

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("ReplicatedMap::reconcile");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
                rt.block_on(async {
                    let store1 = ReplicatedMap::new(cfg1.clone())
                        .await
                        .expect("bind failed")
                        .with_seed(addr2);
                    store1.insert_bulk(&key_values[..size]);
                    let store2 = ReplicatedMap::new(cfg2.clone())
                        .await
                        .expect("bind failed")
                        .with_seed(addr1);
                    store2.insert_bulk(&key_values[..size]);
                    let task1 = tokio::spawn(store1.clone().run(CancellationToken::new()));
                    let task2 = tokio::spawn(store2.clone().run(CancellationToken::new()));

                    b.iter(|| {
                        let k: u32 = rng.gen();
                        let v: u32 = rng.gen();
                        store1.just_insert(k, v);
                        let clone = store1.clone();
                        let task = tokio::spawn(async move { clone.start_reconciliation().await });
                        while store2.get(&k).is_none() {
                            std::thread::sleep(Duration::from_micros(1));
                        }
                        store1.just_remove(&k);
                        task.abort();
                        let clone = store1.clone();
                        let task = tokio::spawn(async move { clone.start_reconciliation().await });
                        while store2.get(&k).is_some() {
                            std::thread::sleep(Duration::from_micros(1));
                        }
                        task.abort();
                    });

                    task2.abort();
                    task1.abort();
                    let _ = tokio::join!(task1, task2);
                })
            });
            size *= 10;
        }
    }

    /// RTT sweep for `service_reconcile_rtt`: the same grid `system.rs`'s injected-RTT lane
    /// sweeps (`rtt_sweep` there — duplicated here rather than shared, like `netem/mod.rs` itself:
    /// each bench binary is a separate compilation unit, and no target here imports another's).
    fn rtt_sweep() -> Vec<Rtt> {
        [0.0, 0.1, 1.0, 10.0, 50.0]
            .into_iter()
            .map(Rtt::from_millis)
            .collect()
    }

    /// Store sizes for `service_reconcile_rtt`: #461's grid, `n` = 10³…10⁶ — the same range
    /// `benches/protocol.rs`'s counted tables sweep, so the measured and counted columns line up.
    const RECONCILE_RTT_SIZES: &[usize] = &[1_000, 10_000, 100_000, 1_000_000];

    /// How the `d` differing keys are laid out — the same two layouts `benches/protocol.rs`'s
    /// (private) `Clustering` sweeps, duplicated here for the reason `rtt_sweep` above states.
    #[derive(Clone, Copy, Debug)]
    enum Clustering {
        /// Spread evenly, so every subtree refines.
        Scattered,
        /// One contiguous block, centred in the key space.
        Clustered,
    }

    impl Clustering {
        fn label(self) -> &'static str {
            match self {
                Clustering::Scattered => "scattered",
                Clustering::Clustered => "clustered",
            }
        }
    }

    /// `(d, clustering)` pairs #461 asks for. `d = 0` is the in-sync baseline — no layout to vary.
    /// `d = 1` only scatters, as in `protocol.rs::DIFFERENCES` — a single key has no layout either
    /// way. 10/100/1000 sweep both, past a 256-cell sketch's capacity at the top end.
    const D_CLUSTERINGS: &[(usize, Clustering)] = &[
        (0, Clustering::Scattered),
        (1, Clustering::Scattered),
        (10, Clustering::Scattered),
        (10, Clustering::Clustered),
        (100, Clustering::Scattered),
        (100, Clustering::Clustered),
        (1_000, Clustering::Scattered),
        (1_000, Clustering::Clustered),
    ];

    /// The `d` keys (out of `0..n`) one peer diverges on, laid out per `clustering` — the same
    /// layout `benches/protocol.rs::missing_keys` uses, so a round here and a round there refine
    /// over the same-shaped difference.
    fn diverging_keys(n: usize, d: usize, clustering: Clustering) -> Vec<u32> {
        if d == 0 {
            return Vec::new();
        }
        match clustering {
            Clustering::Scattered => (1..=d as u64)
                .map(|i| ((n as u64 / (d as u64 + 1)) * i) as u32)
                .collect(),
            // Centred so the block is not adjacent to either end of the key space, where a
            // partition's outermost child would absorb it for free.
            Clustering::Clustered => {
                let start = (n / 2 - d / 2) as u64;
                (start..start + d as u64).map(|k| k as u32).collect()
            }
        }
    }

    /// A fresh loopback address pair per `(n, rtt)` build — one build per combination, not per
    /// Criterion sample, so a handful suffice; a fresh pair still avoids any rebind collision.
    fn fresh_reconcile_rtt_pair() -> (IpAddr, IpAddr) {
        static N: AtomicU32 = AtomicU32::new(0);
        let i = N.fetch_add(1, Ordering::Relaxed);
        let hi = ((i >> 6) & 0xff) as u8;
        let lo = ((i & 0x3f) as u8) * 2 + 1;
        (
            format!("127.6.{hi}.{lo}").parse().unwrap(),
            format!("127.6.{hi}.{}", lo + 1).parse().unwrap(),
        )
    }

    /// Tallies datagrams a wrapped transport has *received*. The `d = 0` baseline round is
    /// otherwise unobservable: a root-fingerprint match makes `rbsr::protocol_round` return no
    /// comparison items and no differences (`src/replica/dispatch.rs`), so the responder sends
    /// nothing back and no local state changes anywhere — the only sign a round happened at all is
    /// that the initiator's message arrived at the responder's transport.
    struct RecvCountingTransport<T> {
        inner: T,
        received: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl<T: Transport> Transport for RecvCountingTransport<T> {
        async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
            let result = self.inner.recv_from(buf).await;
            if result.is_ok() {
                self.received.fetch_add(1, Ordering::Relaxed);
            }
            result
        }

        async fn send_to(&self, buf: &[u8], dst: &SocketAddr) -> io::Result<usize> {
            self.inner.send_to(buf, dst).await
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }
    }

    /// The refinement chain, timed, over an injected-RTT link: composes
    /// `ReplicatedMap::new_with_transport`, `netem::NetemTransport` and the existing `rtt_sweep`
    /// (`system.rs`'s, duplicated above) with `service_reconcile`'s own divergence mechanism — the
    /// only one that exercises refinement rather than the outer-range mismatch `cold_sync_rtt`
    /// builds (`benches/README.md` "Pricing that end-to-end...").
    ///
    /// Both peers load the identical `n`-entry corpus independently, before any peer is seeded:
    /// the reconciliation fingerprint is a function of `(key, value)` alone (`version_hash` hashes
    /// the value, not the stamp — `src/replica/gc.rs`), so two separately timestamped copies of the
    /// same corpus already agree at the root, and nothing is broadcast in doing so (`cold_sync`'s
    /// "no peer yet" discipline). `reconcile_interval` is set far longer than any sample, so every
    /// round below is the one `start_reconciliation` explicitly triggers, not a background tick.
    ///
    /// Per sample: `just_remove` the `d` chosen keys on the initiator (a genuine content
    /// difference, not a timestamp race), trigger one round, poll until the responder reflects the
    /// removal; then `just_insert` them back and repeat, so the pair returns to baseline for the
    /// next sample. `d = 0` has no keys to remove — its round finds nothing to refine, so it is
    /// timed via `RecvCountingTransport` instead (see that type's docs).
    fn service_reconcile_rtt(c: &mut Criterion) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let net = "127.0.0.1/8".parse().unwrap();
        let port = 9_990;

        let mut group = c.benchmark_group("service_reconcile_rtt");
        group.sample_size(10);
        group.sampling_mode(SamplingMode::Flat);
        group.warm_up_time(Duration::from_millis(500));
        // Criterion's 5 s default `measurement_time` is sized for a handful of benchmark ids;
        // `RECONCILE_RTT_SIZES × rtt_sweep() × D_CLUSTERINGS` is 160 of them, so left at the
        // default this group alone would take at least 160 × 5 s ≈ 13 minutes regardless of how
        // cheap any one round is. 1 s keeps `sample_size(10)`'s ten samples meaningful without
        // padding a microsecond-scale round (the `d = 0` baseline at `rtt = 0ms`) out to 5 s.
        group.measurement_time(Duration::from_millis(1_000));

        for &n in RECONCILE_RTT_SIZES {
            let key_values: Vec<(u32, u32)> = (0..n as u32)
                .map(|k| (k, k.wrapping_mul(2_654_435_761)))
                .collect();

            for rtt in rtt_sweep() {
                let link = Link::at(rtt);
                let (addr1, addr2) = fresh_reconcile_rtt_pair();
                let cfg = |addr: IpAddr| {
                    Config::default()
                        .with_port(port)
                        .with_listen_addr(addr)
                        .with_net(net)
                        .with_insecure_no_key()
                        .with_reconcile_interval(Duration::from_secs(3600))
                };

                let (store1, store2, _received1, received2, tasks) = rt.block_on(async {
                    let network = InMemoryNetwork::new();
                    let received1 = Arc::new(AtomicU64::new(0));
                    let transport1 = RecvCountingTransport {
                        inner: NetemTransport::new(
                            Arc::new(network.bind(SocketAddr::new(addr1, port))),
                            Netem::uniform(link, Seed::DEFAULT),
                        ),
                        received: Arc::clone(&received1),
                    };
                    let received2 = Arc::new(AtomicU64::new(0));
                    let transport2 = RecvCountingTransport {
                        inner: NetemTransport::new(
                            Arc::new(network.bind(SocketAddr::new(addr2, port))),
                            Netem::uniform(link, Seed::DEFAULT),
                        ),
                        received: Arc::clone(&received2),
                    };
                    let store1 = ReplicatedMap::<u32, u32>::new_with_transport(
                        cfg(addr1),
                        Arc::new(transport1),
                    );
                    let store2 = ReplicatedMap::<u32, u32>::new_with_transport(
                        cfg(addr2),
                        Arc::new(transport2),
                    );
                    store1.insert_bulk(&key_values);
                    store2.insert_bulk(&key_values);
                    store1.seed_peer(addr2);
                    store2.seed_peer(addr1);
                    let tasks = [
                        tokio::spawn(store1.clone().run(CancellationToken::new())),
                        tokio::spawn(store2.clone().run(CancellationToken::new())),
                    ];
                    // `Replica::run` fires one reconciliation round unconditionally before
                    // entering its receive loop (`src/replica/run.rs`), so each side sends one
                    // round to the other the instant its task starts — settle that transient
                    // before timing anything, or the first sample below would race it.
                    while received1.load(Ordering::Relaxed) < 1
                        || received2.load(Ordering::Relaxed) < 1
                    {
                        tokio::task::yield_now().await;
                    }
                    (store1, store2, received1, received2, tasks)
                });

                for &(d, clustering) in D_CLUSTERINGS {
                    let missing = diverging_keys(n, d, clustering);
                    let restored: Vec<u32> = missing
                        .iter()
                        .map(|&k| k.wrapping_mul(2_654_435_761))
                        .collect();
                    let id = if d == 0 {
                        format!("n={n}/d=0")
                    } else {
                        format!("n={n}/d={d}/{}", clustering.label())
                    };

                    group.bench_with_input(BenchmarkId::new(&id, rtt.label()), &rtt, |b, _| {
                        b.iter_custom(|iters| {
                            rt.block_on(async {
                                let mut total = Duration::ZERO;
                                for _ in 0..iters {
                                    if missing.is_empty() {
                                        let before = received2.load(Ordering::Relaxed);
                                        let start = Instant::now();
                                        let clone = store1.clone();
                                        let task = tokio::spawn(async move {
                                            clone.start_reconciliation().await
                                        });
                                        while received2.load(Ordering::Relaxed) <= before {
                                            tokio::task::yield_now().await;
                                        }
                                        total += start.elapsed();
                                        task.abort();
                                        continue;
                                    }

                                    let start = Instant::now();
                                    for &k in &missing {
                                        store1.just_remove(&k);
                                    }
                                    let clone = store1.clone();
                                    let task =
                                        tokio::spawn(
                                            async move { clone.start_reconciliation().await },
                                        );
                                    while missing.iter().any(|k| store2.get(k).is_some()) {
                                        tokio::task::yield_now().await;
                                    }
                                    task.abort();

                                    for (&k, &v) in missing.iter().zip(restored.iter()) {
                                        store1.just_insert(k, v);
                                    }
                                    let clone = store1.clone();
                                    let task =
                                        tokio::spawn(
                                            async move { clone.start_reconciliation().await },
                                        );
                                    while missing
                                        .iter()
                                        .zip(restored.iter())
                                        .any(|(k, &v)| store2.get_cloned(k) != Some(v))
                                    {
                                        tokio::task::yield_now().await;
                                    }
                                    task.abort();
                                    total += start.elapsed();
                                }
                                total
                            })
                        });
                    });
                }

                rt.block_on(async {
                    for task in tasks {
                        task.abort();
                    }
                });
            }
        }
        group.finish();
    }

    criterion_group!(
        benches,
        fingerprint_tree_map_new,
        fingerprint_tree_map_fill,
        fingerprint_tree_map_insert,
        fingerprint_tree_map_remove,
        fingerprint_tree_map_range_fingerprint,
        read_replica_memory,
        service_send,
        service_reconcile,
        service_reconcile_rtt,
    );
    // Equivalent to `criterion_main!(benches)`, but exposed as a named fn so the top-level `main`
    // (defined outside this feature-gated module) can drive it.
    pub fn main() {
        benches();
        Criterion::default().configure_from_args().final_summary();
    }
} // mod imp
