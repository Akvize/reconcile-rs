// The benchmark drives the internal range-fingerprint via `HRTree::hash`, which is `pub(crate)`.
// Like the integration-test oracles it reaches that internal through the gated
// `reconcile::testing` seam (the `range_hash` shim), so the real bench body only compiles with the
// `internal-testing` feature. Without it we fall back to an empty `main` so the target still links.
#[cfg(not(feature = "internal-testing"))]
fn main() {}

#[cfg(feature = "internal-testing")]
use imp::main;

#[cfg(feature = "internal-testing")]
mod imp {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use rand::{distributions::Standard, Rng, SeedableRng};

    use criterion::{
        criterion_group, AxisScale, BenchmarkId, Criterion, PlotConfiguration, SamplingMode,
        Throughput,
    };

    use reconcile::testing::range_hash;
    use reconcile::{reconcile_store::Config, HRTree, ReconcileStore, Timestamp, ValueOnly};

    fn hrtree_new(c: &mut Criterion) {
        let mut group = c.benchmark_group("HRTree::new");
        group.bench_function("BTreeMap::new()", |b| b.iter(BTreeMap::<u32, u32>::new));
        group.bench_function("HRTree::new()", |b| b.iter(HRTree::<u32, u32>::new));
    }

    /// Measure the time to insert N elements in the tree
    fn hrtree_fill(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("HRTree::fill");
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
            group.bench_with_input(BenchmarkId::new("HRTree::fill", size), &size, |b, &size| {
                b.iter(|| {
                    let mut tree = HRTree::<u32, u32>::new();
                    for (k, v) in key_values[..size].iter().copied() {
                        tree.insert(k, v);
                    }
                })
            });
            size *= 10;
        }
    }

    /// Measure the time to insert (and remove) 1 element in a tree of size N
    fn hrtree_insert(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("HRTree::insert");
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
                BenchmarkId::new("HRTree::insert", size),
                &size,
                |b, &size| {
                    let mut tree = HRTree::<u32, u32>::new();
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

    /// Measure the time to remove (and restore) 1 element in a tree of size N
    fn hrtree_remove(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("HRTree::remove");
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
                BenchmarkId::new("HRTree::remove", size),
                &size,
                |b, &size| {
                    let mut tree = HRTree::<u32, u32>::new();
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

    /// Measure the time to compute the hash over a range in a HRTree of size N
    fn hrtree_hash(c: &mut Criterion) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);

        let mut key_values = Vec::new();
        for _ in 0..1_000_000 {
            let key: u32 = rng.gen();
            let value: u32 = rng.gen();
            key_values.push((key, value));
        }
        let key_values = &key_values;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("HRTree::hash");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
                let mut tree = HRTree::<u32, u32>::new();
                for (k, v) in key_values[..size].iter().copied() {
                    tree.insert(k, v);
                }
                b.iter(|| {
                    let k1: u32 = rng.gen();
                    let k2: u32 = rng.gen();
                    let range = if k1 < k2 { k1..k2 } else { k2..k1 };
                    range_hash(&tree, &range);
                })
            });
            size *= 10;
        }
    }

    /// Compare the in-memory cost of a **naive dated mirror** (`HRTree<K, (Timestamp, Option<V>)>`, which
    /// drags along a timestamp it never uses) against the **lightweight value-only mirror**
    /// (`HRTree<K, ValueOnly<V>>`) that issue #128 introduces.
    ///
    /// Criterion times the *fill* of each tree at growing sizes; the value-only tree both builds faster
    /// (less to move/hash per entry) and, as the one-off report below shows, stores fewer bytes per
    /// entry — the whole point of the optimization for fleets with many passive read replicas.
    fn mirror_memory(c: &mut Criterion) {
        let dated = std::mem::size_of::<(Timestamp, Option<u32>)>();
        let light = std::mem::size_of::<ValueOnly<u32>>();
        println!(
            "[mirror memory] per-entry value size: dated (Timestamp, Option<u32>) = {dated} B, \
         value-only ValueOnly<u32> = {light} B, saved = {} B/entry",
            dated - light
        );

        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut keys = Vec::new();
        for _ in 0..1_000_000 {
            keys.push(rng.gen::<u32>());
        }
        let keys = &keys;

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("mirror_memory::fill");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= keys.len() {
            group.throughput(Throughput::Elements(size as u64));
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(
                BenchmarkId::new("dated (Timestamp, Option<u32>)", size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let mut tree = HRTree::<u32, (Timestamp, Option<u32>)>::new();
                        for &k in keys[..size].iter() {
                            tree.insert(k, (Timestamp::new(k as u64, 0, 0), Some(k)));
                        }
                    })
                },
            );
            group.bench_with_input(
                BenchmarkId::new("value-only ValueOnly<u32>", size),
                &size,
                |b, &size| {
                    b.iter(|| {
                        let mut tree = HRTree::<u32, ValueOnly<u32>>::new();
                        for &k in keys[..size].iter() {
                            tree.insert(k, ValueOnly(Some(k)));
                        }
                    })
                },
            );
            size *= 10;
        }
    }

    /// Measure the time to send 1 insertion, and 1 removal between 2 ReconcileStore instances containing N items
    fn service_send(c: &mut Criterion) {
        let port = 8080;
        let net = "127.0.0.1/8".parse().unwrap();
        let addr1 = "127.0.0.44".parse().unwrap();
        let addr2 = "127.0.0.45".parse().unwrap();
        let cfg1 = Config::default()
            .with_port(port)
            .with_listen_addr(addr1)
            .with_net(net);
        let cfg2 = Config::default()
            .with_port(port)
            .with_listen_addr(addr2)
            .with_net(net);

        let mut rng = rand::rngs::ThreadRng::default();

        let key_values: Vec<(u32, u32)> =
            (&mut rng).sample_iter(Standard).take(1_000_000).collect();

        let rt = tokio::runtime::Runtime::new().unwrap();

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("ReconcileStore::send");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
                rt.block_on(async {
                    // start reconciliation stores
                    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
                    store1.insert_bulk(&key_values[..size]);
                    let store2 = ReconcileStore::new(cfg2).await.with_seed(addr1);
                    store2.insert_bulk(&key_values[..size]);
                    let task1 = tokio::spawn(store1.clone().run());
                    let task2 = tokio::spawn(store2.clone().run());

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

    /// Measure the time to reconcile 1 insertion/removal between ReconcileStore instances containing N items
    fn service_reconcile(c: &mut Criterion) {
        let port = 8080;
        let net = "127.0.0.1/8".parse().unwrap();
        let addr1 = "127.0.0.44".parse().unwrap();
        let addr2 = "127.0.0.45".parse().unwrap();
        let cfg1 = Config::default()
            .with_port(port)
            .with_listen_addr(addr1)
            .with_net(net);
        let cfg2 = Config::default()
            .with_port(port)
            .with_listen_addr(addr2)
            .with_net(net);

        let mut rng = rand::rngs::ThreadRng::default();

        let key_values: Vec<(u32, u32)> =
            (&mut rng).sample_iter(Standard).take(1_000_000).collect();

        let rt = tokio::runtime::Runtime::new().unwrap();

        let plot_config = PlotConfiguration::default().summary_scale(AxisScale::Logarithmic);
        let mut group = c.benchmark_group("ReconcileStore::reconcile");
        group.plot_config(plot_config);
        let mut size = 10;
        while size <= key_values.len() {
            group.sample_size(10.max(1_000_000 / size).min(100));
            group.sampling_mode(SamplingMode::Linear);
            group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
                rt.block_on(async {
                    // start reconciliation services
                    let store1 = ReconcileStore::new(cfg1).await.with_seed(addr2);
                    store1.insert_bulk(&key_values[..size]);
                    let store2 = ReconcileStore::new(cfg2).await.with_seed(addr1);
                    store2.insert_bulk(&key_values[..size]);
                    let task1 = tokio::spawn(store1.clone().run());
                    let task2 = tokio::spawn(store2.clone().run());

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

    criterion_group!(
        benches,
        hrtree_new,
        hrtree_fill,
        hrtree_insert,
        hrtree_remove,
        hrtree_hash,
        mirror_memory,
        service_send,
        service_reconcile,
    );
    // Equivalent to `criterion_main!(benches)`, but exposed as a named fn so the top-level `main`
    // (defined outside this feature-gated module) can drive it.
    pub fn main() {
        benches();
        Criterion::default().configure_from_args().final_summary();
    }
} // mod imp
